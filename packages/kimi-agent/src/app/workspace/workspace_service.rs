use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::future::FutureExt;
use indexmap::{IndexMap, IndexSet};
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::fs::FS_PATH_NOT_FOUND;
use parking_lot::Mutex;
use serde_json::json;

use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::is_windows_absolute_path;
use crate::_base::utils::workdir_slug::{encode_work_dir_key, is_work_dir_key, workspace_root_key};
use crate::app::event::event::IEventService;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{is_host_fs_not_directory, is_host_fs_not_found};
use crate::persistence::interface::append_log_store::IAppendLogStore;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;

use super::errors::register_workspace_errors;
use super::workspace::{
    IWorkspaceService, WORKSPACE_KIND_IMPLICIT, WORKSPACE_KIND_MANAGED, Workspace, WorkspaceCreate,
    WorkspaceUpdate,
};
use super::workspace_alias::{
    SessionIndexCompactionDeps, collect_alias_ids, compact_session_index_if_stale, dedupe_by_root,
    read_session_index_entries, read_session_index_work_dirs,
};
use super::workspace_events::{
    WorkspaceCreated, WorkspaceCreatedPayload, WorkspaceDeleted, WorkspaceDeletedPayload,
    WorkspaceUpdated, WorkspaceUpdatedPayload,
};
use super::workspace_persistence::{
    IWorkspacePersistence, WorkspaceCatalog, WorkspaceCatalogMutation,
};

pub struct WorkspaceService {
    store: Arc<dyn IWorkspacePersistence>,
    storage: Arc<dyn IFileSystemStorageService>,
    append_logs: Arc<dyn IAppendLogStore>,
    host_fs: Arc<dyn IHostFileSystem>,
    log: Arc<dyn ILogService>,
    event: Arc<dyn IEventService>,
    merged: AtomicBool,
    op_queue: tokio::sync::Mutex<()>,
}

impl WorkspaceService {
    pub fn new(
        store: Arc<dyn IWorkspacePersistence>,
        storage: Arc<dyn IFileSystemStorageService>,
        append_logs: Arc<dyn IAppendLogStore>,
        host_fs: Arc<dyn IHostFileSystem>,
        log: Arc<dyn ILogService>,
        event: Arc<dyn IEventService>,
    ) -> Self {
        Self {
            store,
            storage,
            append_logs,
            host_fs,
            log,
            event,
            merged: AtomicBool::new(false),
            op_queue: tokio::sync::Mutex::new(()),
        }
    }

    async fn require_directory(&self, root: &str) -> Result<(), WireAsyncError> {
        let mut stat = match self.host_fs.stat(root).await {
            Ok(stat) => stat,
            Err(error) => {
                if is_host_fs_not_found(&error) || is_host_fs_not_directory(&error) {
                    return Err(Box::new(Error2::new(
                        FS_PATH_NOT_FOUND,
                        format!("workspace root {root} does not exist"),
                    )));
                }
                return Err(error);
            }
        };
        if !stat.is_directory {
            if let Ok(realpath) = self.host_fs.realpath(root).await {
                if let Ok(real_stat) = self.host_fs.stat(&realpath).await {
                    stat = real_stat;
                }
            }
        }
        if !stat.is_directory {
            return Err(Box::new(Error2::new(
                FS_PATH_NOT_FOUND,
                format!("workspace root {root} is not a directory"),
            )));
        }
        Ok(())
    }

    async fn normalize_additional_dirs(
        &self,
        root: &str,
        dirs: &[String],
    ) -> Result<Vec<String>, WireAsyncError> {
        let root_key = workspace_root_key(root);
        let mut kept = Vec::<String>::new();
        for dir in dirs {
            let resolved = if is_absolute(dir) {
                dir.clone()
            } else {
                format!("{root}/{dir}")
            };
            self.require_directory(&resolved).await?;
            let key = workspace_root_key(&resolved);
            if key == root_key {
                continue;
            }
            if kept
                .iter()
                .any(|entry| workspace_root_key(entry) == key)
            {
                continue;
            }
            kept.push(resolved);
        }
        Ok(kept)
    }

    async fn ensure_merged(&self) -> Result<(), WireAsyncError> {
        if self.merged.load(Ordering::Acquire) {
            return Ok(());
        }
        let loaded = self.store.load().await?;
        if loaded.is_none() {
            let rebuilt = self.rebuild_from_session_index().await?;
            self.store
                .save(WorkspaceCatalog {
                    workspaces: rebuilt.into_values().collect(),
                    deleted_ids: Vec::new(),
                    idempotency_keys: None,
                })
                .await?;
        } else {
            let loaded = loaded.unwrap();
            let mut by_id = loaded
                .workspaces
                .into_iter()
                .map(|workspace| (workspace.id.clone(), workspace))
                .collect::<IndexMap<_, _>>();
            let mut deleted_ids = loaded.deleted_ids.into_iter().collect::<IndexSet<_>>();
            if self
                .merge_from_session_index(&mut by_id, &deleted_ids)
                .await?
            {
                self.store
                    .save(WorkspaceCatalog {
                        workspaces: by_id.into_values().collect(),
                        deleted_ids: deleted_ids.into_iter().collect(),
                        idempotency_keys: None,
                    })
                    .await?;
            }
        }
        self.merged.store(true, Ordering::Release);
        self.compact_session_index().await;
        Ok(())
    }

    async fn compact_session_index(&self) {
        let host_fs = self.host_fs.clone();
        let result = compact_session_index_if_stale(SessionIndexCompactionDeps {
            storage: self.storage.clone(),
            append_logs: self.append_logs.clone(),
            session_dir_exists: Arc::new(move |dir| {
                let host_fs = host_fs.clone();
                Box::pin(async move { Ok(session_dir_exists(&host_fs, &dir).await) })
            }),
        })
        .await;
        if let Err(error) = result {
            self.log.warn(
                "session index compaction failed; will retry on the next startup",
                Some(json!({ "error": error.to_string() })),
            );
        }
    }

    async fn load_catalog(&self) -> Result<WorkspaceCatalog, WireAsyncError> {
        Ok(self.store.load().await?.unwrap_or_default())
    }

    async fn merge_from_session_index(
        &self,
        by_id: &mut IndexMap<String, Workspace>,
        deleted_ids: &IndexSet<String>,
    ) -> Result<bool, WireAsyncError> {
        let mut changed = false;
        let now = Utc::now().timestamp_millis();
        for work_dir in read_session_index_work_dirs(&self.storage).await? {
            let id = encode_work_dir_key(&work_dir);
            if by_id.contains_key(&id) || deleted_ids.contains(&id) {
                continue;
            }
            by_id.insert(
                id.clone(),
                Workspace {
                    id,
                    name: basename(&work_dir),
                    root: work_dir,
                    kind: Some(WORKSPACE_KIND_IMPLICIT.to_string()),
                    additional_dirs: Vec::new(),
                    created_at: now,
                    last_opened_at: now,
                },
            );
            changed = true;
        }
        Ok(changed)
    }

    async fn rebuild_from_session_index(
        &self,
    ) -> Result<IndexMap<String, Workspace>, WireAsyncError> {
        let mut result = IndexMap::new();
        let now = Utc::now().timestamp_millis();
        let mut seen_root_keys = IndexSet::new();
        for entry in read_session_index_entries(&self.storage).await? {
            if !is_absolute(&entry.work_dir) {
                continue;
            }
            let root_key = workspace_root_key(&entry.work_dir);
            if !seen_root_keys.insert(root_key) {
                continue;
            }
            let id = encode_work_dir_key(&entry.work_dir);
            result.insert(
                id.clone(),
                Workspace {
                    id,
                    root: entry.work_dir.clone(),
                    name: basename(&entry.work_dir),
                    kind: Some(WORKSPACE_KIND_IMPLICIT.to_string()),
                    additional_dirs: Vec::new(),
                    created_at: now,
                    last_opened_at: now,
                },
            );
        }
        Ok(result)
    }
}

#[async_trait]
impl IWorkspaceService for WorkspaceService {
    async fn list(&self) -> Result<Vec<Workspace>, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        let by_id = catalog
            .workspaces
            .into_iter()
            .map(|workspace| (workspace.id.clone(), workspace))
            .collect::<IndexMap<_, _>>();
        Ok(dedupe_by_root(&by_id))
    }

    async fn get(&self, id: &str) -> Result<Option<Workspace>, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        Ok(catalog
            .workspaces
            .into_iter()
            .find(|workspace| workspace.id == id))
    }

    async fn create(&self, input: WorkspaceCreate) -> Result<Workspace, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.require_directory(&input.root).await?;
        let additional_dirs = self
            .normalize_additional_dirs(
                &input.root,
                input.additional_dirs.as_deref().unwrap_or(&[]),
            )
            .await?;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        let mut idempotency_keys = catalog.idempotency_keys.clone().unwrap_or_default();
        let mapped = input
            .idempotency_key
            .as_ref()
            .and_then(|key| idempotency_keys.get(key))
            .cloned();
        let existing = mapped.and_then(|mapped| {
            catalog
                .workspaces
                .iter()
                .find(|workspace| workspace.id == mapped)
                .cloned()
        });
        if let Some(existing) = existing {
            return Ok(existing);
        }
        let now = Utc::now().timestamp_millis();
        let workspace = Workspace {
            id: format!("wm_{}", uuid::Uuid::new_v4()),
            root: input.root.clone(),
            name: input.name.clone().unwrap_or_else(|| basename(&input.root)),
            kind: Some(WORKSPACE_KIND_MANAGED.to_string()),
            additional_dirs,
            created_at: now,
            last_opened_at: now,
        };
        if let Some(key) = input.idempotency_key {
            idempotency_keys.insert(key, workspace.id.clone());
        }
        let mut workspaces = catalog.workspaces.clone();
        workspaces.push(workspace.clone());
        self.store
            .save(WorkspaceCatalog {
                workspaces,
                deleted_ids: catalog.deleted_ids,
                idempotency_keys: Some(idempotency_keys),
            })
            .await?;
        self.event.publish(
            WorkspaceCreated::new(WorkspaceCreatedPayload {
                workspace: workspace.clone(),
            })
            .to_event2(),
        );
        Ok(workspace)
    }

    async fn create_or_touch_with_name(
        &self,
        root: &str,
        name: Option<&str>,
    ) -> Result<Option<Workspace>, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        let mut stat = self.host_fs.stat(root).await.ok();
        if stat.as_ref().is_some_and(|stat| !stat.is_directory) {
            stat = match self.host_fs.realpath(root).await {
                Ok(realpath) => self.host_fs.stat(&realpath).await.ok(),
                Err(_) => None,
            };
        }
        if stat.as_ref().is_some_and(|stat| !stat.is_directory) {
            return Err(Box::new(Error2::new(
                FS_PATH_NOT_FOUND,
                format!("workspace root {root} is not a directory"),
            )));
        }
        self.ensure_merged().await?;
        let root = root.to_string();
        let name = name.map(str::to_string);
        let workspace_out = Arc::new(Mutex::new(None));
        let existing_out = Arc::new(Mutex::new(None));
        let workspace_capture = workspace_out.clone();
        let existing_capture = existing_out.clone();
        let mutate: WorkspaceCatalogMutation = Arc::new(move |catalog| {
            let root = root.clone();
            let name = name.clone();
            let workspace_capture = workspace_capture.clone();
            let existing_capture = existing_capture.clone();
            async move {
                let mut by_id = catalog
                    .workspaces
                    .into_iter()
                    .map(|entry| (entry.id.clone(), entry))
                    .collect::<IndexMap<_, _>>();
                let mut deleted_ids = catalog.deleted_ids.into_iter().collect::<IndexSet<_>>();
                let id = encode_work_dir_key(&root);
                let mut existing = by_id.get(&id).cloned();
                if existing.is_none() {
                    let root_key = workspace_root_key(&root);
                    for entry in by_id.values() {
                        if is_work_dir_key(&entry.id)
                            && workspace_root_key(&entry.root) == root_key
                        {
                            existing = Some(entry.clone());
                            break;
                        }
                    }
                }
                let now = Utc::now().timestamp_millis();
                let workspace = match existing.clone() {
                    Some(existing) => Workspace {
                        last_opened_at: now,
                        ..existing
                    },
                    None => Workspace {
                        id,
                        name: name.unwrap_or_else(|| basename(&root)),
                        root: root.clone(),
                        kind: Some(WORKSPACE_KIND_IMPLICIT.to_string()),
                        additional_dirs: Vec::new(),
                        created_at: now,
                        last_opened_at: now,
                    },
                };
                by_id.insert(workspace.id.clone(), workspace.clone());
                deleted_ids.shift_remove(&workspace.id);
                *existing_capture.lock() = existing;
                *workspace_capture.lock() = Some(workspace);
                Ok(WorkspaceCatalog {
                    workspaces: by_id.into_values().collect(),
                    deleted_ids: deleted_ids.into_iter().collect(),
                    idempotency_keys: catalog.idempotency_keys,
                })
            }
            .boxed()
        });
        self.store.update(mutate).await?;
        let workspace = workspace_out.lock().take().unwrap();
        if existing_out.lock().is_none() {
            self.event.publish(
                WorkspaceCreated::new(WorkspaceCreatedPayload {
                    workspace: workspace.clone(),
                })
                .to_event2(),
            );
        } else {
            self.event.publish(
                WorkspaceUpdated::new(WorkspaceUpdatedPayload {
                    workspace: workspace.clone(),
                })
                .to_event2(),
            );
        }
        Ok(Some(workspace))
    }

    async fn touch(&self, id: &str) -> Result<Option<Workspace>, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        let Some(existing) = catalog
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        self.require_directory(&existing.root).await?;
        let touched = Workspace {
            last_opened_at: Utc::now().timestamp_millis(),
            ..existing
        };
        let workspaces = catalog
            .workspaces
            .iter()
            .map(|workspace| {
                if workspace.id == id {
                    touched.clone()
                } else {
                    workspace.clone()
                }
            })
            .collect();
        self.store
            .save(WorkspaceCatalog {
                workspaces,
                deleted_ids: catalog.deleted_ids,
                idempotency_keys: catalog.idempotency_keys,
            })
            .await?;
        self.event.publish(
            WorkspaceUpdated::new(WorkspaceUpdatedPayload {
                workspace: touched.clone(),
            })
            .to_event2(),
        );
        Ok(Some(touched))
    }

    async fn set_primary(
        &self,
        id: &str,
        path: &str,
    ) -> Result<Option<Workspace>, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        let Some(existing) = catalog
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        let resolved = if is_absolute(path) {
            path.to_string()
        } else {
            format!("{}/{path}", existing.root)
        };
        self.require_directory(&resolved).await?;
        let resolved_key = workspace_root_key(&resolved);
        if resolved_key == workspace_root_key(&existing.root) {
            return Ok(Some(existing));
        }
        let was_additional = existing
            .additional_dirs
            .iter()
            .any(|dir| workspace_root_key(dir) == resolved_key);
        if !was_additional {
            return Err(Box::new(Error2::new(
                FS_PATH_NOT_FOUND,
                format!("primary {path} is not one of this workspace's source folders"),
            )));
        }
        let mut additional_dirs = existing
            .additional_dirs
            .iter()
            .filter(|dir| workspace_root_key(dir) != resolved_key)
            .cloned()
            .collect::<Vec<_>>();
        additional_dirs.push(existing.root.clone());
        let updated = Workspace {
            root: resolved,
            additional_dirs,
            ..existing
        };
        let workspaces = catalog
            .workspaces
            .iter()
            .map(|workspace| {
                if workspace.id == id {
                    updated.clone()
                } else {
                    workspace.clone()
                }
            })
            .collect();
        self.store
            .save(WorkspaceCatalog {
                workspaces,
                deleted_ids: catalog.deleted_ids,
                idempotency_keys: catalog.idempotency_keys,
            })
            .await?;
        self.event.publish(
            WorkspaceUpdated::new(WorkspaceUpdatedPayload {
                workspace: updated.clone(),
            })
            .to_event2(),
        );
        Ok(Some(updated))
    }

    async fn update(
        &self,
        id: &str,
        patch: WorkspaceUpdate,
    ) -> Result<Option<Workspace>, WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        let Some(existing) = catalog
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .cloned()
        else {
            return Ok(None);
        };
        let mut updated = existing;
        if let Some(name) = patch.name {
            updated.name = name;
        }
        if let Some(additional_dirs) = patch.additional_dirs {
            updated.additional_dirs = additional_dirs;
        }
        let workspaces = catalog
            .workspaces
            .iter()
            .map(|workspace| {
                if workspace.id == id {
                    updated.clone()
                } else {
                    workspace.clone()
                }
            })
            .collect();
        self.store
            .save(WorkspaceCatalog {
                workspaces,
                deleted_ids: catalog.deleted_ids,
                idempotency_keys: None,
            })
            .await?;
        self.event.publish(
            WorkspaceUpdated::new(WorkspaceUpdatedPayload {
                workspace: updated.clone(),
            })
            .to_event2(),
        );
        Ok(Some(updated))
    }

    async fn delete(&self, id: &str) -> Result<(), WireAsyncError> {
        let _operation = self.op_queue.lock().await;
        self.ensure_merged().await?;
        let catalog = self.load_catalog().await?;
        let mut root = catalog
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .map(|workspace| workspace.root.clone());
        if root.is_none() {
            root = read_session_index_entries(&self.storage)
                .await?
                .into_iter()
                .find(|line| encode_work_dir_key(&line.work_dir) == id)
                .map(|line| line.work_dir);
        }
        let Some(root) = root else {
            let mut deleted_ids = catalog.deleted_ids.into_iter().collect::<IndexSet<_>>();
            deleted_ids.insert(id.to_string());
            self.store
                .save(WorkspaceCatalog {
                    workspaces: catalog
                        .workspaces
                        .into_iter()
                        .filter(|workspace| workspace.id != id)
                        .collect(),
                    deleted_ids: deleted_ids.into_iter().collect(),
                    idempotency_keys: None,
                })
                .await?;
            return Ok(());
        };
        let root_key = workspace_root_key(&root);
        let alias_ids = collect_alias_ids(
            &catalog.workspaces,
            &read_session_index_entries(&self.storage).await?,
            &root,
        );
        let mut deleted_ids = catalog.deleted_ids.into_iter().collect::<IndexSet<_>>();
        deleted_ids.extend(alias_ids);
        self.store
            .save(WorkspaceCatalog {
                workspaces: catalog
                    .workspaces
                    .into_iter()
                    .filter(|workspace| workspace_root_key(&workspace.root) != root_key)
                    .collect(),
                deleted_ids: deleted_ids.into_iter().collect(),
                idempotency_keys: None,
            })
            .await?;
        self.event.publish(
            WorkspaceDeleted::new(WorkspaceDeletedPayload {
                workspace_id: id.to_string(),
                root,
            })
            .to_event2(),
        );
        Ok(())
    }
}

async fn session_dir_exists(host_fs: &Arc<dyn IHostFileSystem>, dir: &str) -> bool {
    match host_fs.stat(dir).await {
        Ok(stat) => stat.is_directory,
        Err(error) => !(is_host_fs_not_found(&error) || is_host_fs_not_directory(&error)),
    }
}

fn is_absolute(path: &str) -> bool {
    Path::new(path).is_absolute() || is_windows_absolute_path(path)
}

fn basename(root: &str) -> String {
    Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
