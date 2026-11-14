use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::event::Emitter;
use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::canonical_path;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::scopes::LifecycleScope;
use crate::human::utils::watch::{WatchHandle, WatchSubscription, watch_candidates};
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;

use super::workspace::{WORKSPACE_KIND_IMPLICIT, WORKSPACE_KIND_MANAGED, Workspace};
use super::workspace_persistence::{
    IWorkspacePersistence, PersistedWorkspaceEntry, PersistedWorkspaceFile, WorkspaceCatalog,
    WorkspaceCatalogMutation, WorkspaceChangeListener,
};

const WORKSPACE_CATALOG_VERSION: u64 = 1;
const WORKSPACE_CATALOG_SCOPE: &str = "";
const WORKSPACE_CATALOG_KEY: &str = "workspaces.json";
const WORKSPACE_CATALOG_LOCK_KEY: &str = "workspace-catalog.lock";
const WORKSPACE_CATALOG_LOCK_WAIT_MS: u64 = 2_000;
const WORKSPACE_CATALOG_LOCK_RETRY_MS: u64 = 100;
const WATCH_DEBOUNCE_MS: u64 = 150;

#[derive(Clone)]
struct CatalogCache {
    version: String,
    catalog: WorkspaceCatalog,
}

#[derive(Clone, Default)]
struct WatchDebounce {
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl WatchDebounce {
    fn cancel_and_set(&self, callback: impl Fn() + Send + Sync + 'static) {
        self.dispose();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS)).await;
            callback();
        });
        *self.task.lock() = Some(task);
    }

    fn dispose(&self) {
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
    }
}

struct CatalogLockGuard {
    file: Option<File>,
}

impl Drop for CatalogLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

pub struct FileWorkspacePersistence {
    docs: Arc<dyn IAtomicDocumentStore>,
    storage: Arc<dyn IFileSystemStorageService>,
    log: Arc<dyn ILogService>,
    change_emitter: Arc<Emitter<()>>,
    watch_debounce: WatchDebounce,
    catalog_lock_path: PathBuf,
    catalog_cache: Arc<Mutex<Option<CatalogCache>>>,
    watch_handle: Box<WatchHandle>,
    watch_subscription: Mutex<Option<Box<WatchSubscription>>>,
}

impl FileWorkspacePersistence {
    pub fn new(
        docs: Arc<dyn IAtomicDocumentStore>,
        bootstrap: Arc<dyn IBootstrapService>,
        storage: Arc<dyn IFileSystemStorageService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        let home_dir = bootstrap.home_dir();
        let catalog_file = Path::new(&home_dir)
            .join(WORKSPACE_CATALOG_KEY)
            .to_string_lossy()
            .into_owned();
        let catalog_lock_path = Path::new(&home_dir)
            .join("locks")
            .join(WORKSPACE_CATALOG_LOCK_KEY);
        let change_emitter = Arc::new(Emitter::new());
        let watch_debounce = WatchDebounce::default();
        let catalog_cache = Arc::new(Mutex::new(None));
        let watch_handle = watch_candidates(&home_dir, &[catalog_file.clone()], None);
        let watched_cache = catalog_cache.clone();
        let watched_debounce = watch_debounce.clone();
        let watched_emitter = change_emitter.clone();
        let watch_subscription = watch_handle.on_did_change(move |change| {
            if canonical_path(&change.path) != canonical_path(&catalog_file) {
                return;
            }
            *watched_cache.lock() = None;
            let emitter = watched_emitter.clone();
            watched_debounce.cancel_and_set(move || emitter.fire(()));
        });
        Self {
            docs,
            storage,
            log,
            change_emitter,
            watch_debounce,
            catalog_lock_path,
            catalog_cache,
            watch_handle,
            watch_subscription: Mutex::new(Some(watch_subscription)),
        }
    }

    async fn read_catalog(&self) -> Result<Option<WorkspaceCatalog>, WireAsyncError> {
        let Some(file) = self
            .docs
            .get(WORKSPACE_CATALOG_SCOPE, WORKSPACE_CATALOG_KEY)
            .await?
        else {
            return Ok(None);
        };
        if !file.is_object() && !file.is_array() {
            return Ok(None);
        }
        let Some(workspaces_value) = file.get("workspaces") else {
            return Ok(None);
        };
        let Some(workspace_entries) = js_object_entries(workspaces_value) else {
            return Ok(None);
        };
        let now = Utc::now().timestamp_millis();
        let mut workspaces = Vec::new();
        for (id, raw) in workspace_entries {
            let Some(entry) = sanitize_entry(raw) else {
                continue;
            };
            workspaces.push(Workspace {
                id,
                root: entry.root,
                name: entry.name,
                kind: Some(
                    if entry.kind.as_deref() == Some(WORKSPACE_KIND_MANAGED) {
                        WORKSPACE_KIND_MANAGED
                    } else {
                        WORKSPACE_KIND_IMPLICIT
                    }
                    .to_string(),
                ),
                additional_dirs: entry.additional_dirs.unwrap_or_default(),
                created_at: parse_time(&entry.created_at, now),
                last_opened_at: parse_time(&entry.last_opened_at, now),
            });
        }
        let deleted_ids = file
            .get("deleted_workspace_ids")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let idempotency_keys = file
            .get("idempotency_keys")
            .and_then(js_object_entries)
            .map(|entries| {
                entries
                    .into_iter()
                    .filter_map(|(key, value)| value.as_str().map(|value| (key, value.to_string())))
                    .collect::<IndexMap<_, _>>()
            });
        Ok(Some(WorkspaceCatalog {
            workspaces,
            deleted_ids,
            idempotency_keys,
        }))
    }

    async fn save_unlocked(&self, catalog: WorkspaceCatalog) -> Result<(), WireAsyncError> {
        let mut record = IndexMap::new();
        for workspace in catalog.workspaces {
            record.insert(
                workspace.id.clone(),
                PersistedWorkspaceEntry {
                    root: workspace.root,
                    name: workspace.name,
                    kind: workspace.kind,
                    additional_dirs: Some(workspace.additional_dirs),
                    created_at: format_time(workspace.created_at)?,
                    last_opened_at: format_time(workspace.last_opened_at)?,
                },
            );
        }
        let file = PersistedWorkspaceFile {
            version: WORKSPACE_CATALOG_VERSION,
            workspaces: record,
            deleted_workspace_ids: catalog.deleted_ids,
            idempotency_keys: catalog.idempotency_keys,
        };
        *self.catalog_cache.lock() = None;
        self.docs
            .set(
                WORKSPACE_CATALOG_SCOPE,
                WORKSPACE_CATALOG_KEY,
                serde_json::to_value(file)?,
            )
            .await?;
        self.change_emitter.fire(());
        Ok(())
    }

    async fn lock_catalog(&self) -> Result<CatalogLockGuard, WireAsyncError> {
        if let Some(parent) = self.catalog_lock_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.catalog_lock_path)
            .await?
            .into_std()
            .await;
        let deadline = Instant::now() + Duration::from_millis(WORKSPACE_CATALOG_LOCK_WAIT_MS);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(CatalogLockGuard { file: Some(file) });
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(Box::new(error)),
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(WORKSPACE_CATALOG_LOCK_RETRY_MS)).await;
        }
        self.log.warn(
            "workspace catalog lock unavailable; continuing without lock",
            Some(json!({ "path": self.catalog_lock_path.to_string_lossy() })),
        );
        Ok(CatalogLockGuard { file: None })
    }

    async fn catalog_version(&self) -> Option<String> {
        let path = self
            .storage
            .path_for(WORKSPACE_CATALOG_SCOPE, WORKSPACE_CATALOG_KEY)?;
        let metadata = tokio::fs::metadata(path).await.ok()?;
        Some(metadata_version(&metadata))
    }

    pub fn dispose(&self) {
        self.watch_debounce.dispose();
        self.change_emitter.dispose();
        self.watch_handle.dispose();
        if let Some(mut subscription) = self.watch_subscription.lock().take() {
            subscription.dispose();
        }
    }
}

#[async_trait]
impl IWorkspacePersistence for FileWorkspacePersistence {
    fn on_did_change(&self, listener: WorkspaceChangeListener) -> Box<dyn IDisposable> {
        Box::new(
            self.change_emitter
                .event()
                .subscribe(move |_| listener()),
        )
    }

    async fn load(&self) -> Result<Option<WorkspaceCatalog>, WireAsyncError> {
        let version = self.catalog_version().await;
        if let Some(version) = version.as_ref()
            && let Some(cache) = self.catalog_cache.lock().clone()
            && cache.version == *version
        {
            return Ok(Some(cache.catalog));
        }
        let catalog = self.read_catalog().await?;
        let cache = if catalog.is_some() && version.is_some() {
            let current_version = self.catalog_version().await;
            match current_version {
                Some(current_version) if current_version == version.clone().unwrap() => {
                    Some(CatalogCache {
                        version: current_version,
                        catalog: catalog.clone().unwrap(),
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        *self.catalog_cache.lock() = cache;
        Ok(catalog)
    }

    async fn save(&self, catalog: WorkspaceCatalog) -> Result<(), WireAsyncError> {
        let _release = self.lock_catalog().await?;
        self.save_unlocked(catalog).await
    }

    async fn update(
        &self,
        mutate: WorkspaceCatalogMutation,
    ) -> Result<WorkspaceCatalog, WireAsyncError> {
        let _release = self.lock_catalog().await?;
        let current = self.load().await?.unwrap_or_default();
        let result = mutate(current).await?;
        self.save_unlocked(result.clone()).await?;
        Ok(result)
    }

    async fn stamp(&self) -> Result<Option<String>, WireAsyncError> {
        let (size, mtime) = tokio::try_join!(
            self.storage.size(WORKSPACE_CATALOG_SCOPE, WORKSPACE_CATALOG_KEY),
            self.storage.mtime(WORKSPACE_CATALOG_SCOPE, WORKSPACE_CATALOG_KEY),
        )?;
        match (size, mtime) {
            (Some(size), Some(mtime)) => Ok(Some(format!("{size}:{mtime}"))),
            _ => Ok(None),
        }
    }
}

impl IDisposable for FileWorkspacePersistence {
    fn dispose(&self) {
        FileWorkspacePersistence::dispose(self);
    }
}

fn js_object_entries(value: &Value) -> Option<Vec<(String, &Value)>> {
    match value {
        Value::Object(object) => Some(
            object
                .iter()
                .map(|(key, value)| (key.clone(), value))
                .collect(),
        ),
        Value::Array(array) => Some(
            array
                .iter()
                .enumerate()
                .map(|(index, value)| (index.to_string(), value))
                .collect(),
        ),
        _ => None,
    }
}

fn sanitize_entry(value: &Value) -> Option<PersistedWorkspaceEntry> {
    if !value.is_object() && !value.is_array() {
        return None;
    }
    let root = value.get("root")?.as_str()?.to_string();
    let name = value.get("name")?.as_str()?.to_string();
    let created_at = value.get("created_at")?.as_str()?.to_string();
    let last_opened_at = value.get("last_opened_at")?.as_str()?.to_string();
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .map(str::to_string);
    let additional_dirs = value.get("additional_dirs").and_then(Value::as_array).map(
        |directories| {
            directories
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        },
    );
    Some(PersistedWorkspaceEntry {
        root,
        name,
        kind,
        additional_dirs,
        created_at,
        last_opened_at,
    })
}

fn parse_time(value: &str, fallback: i64) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.timestamp_millis())
        .unwrap_or(fallback)
}

fn format_time(value: i64) -> Result<String, WireAsyncError> {
    let date = DateTime::from_timestamp_millis(value).ok_or_else(|| {
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "workspace timestamp is outside the supported range",
        )) as WireAsyncError
    })?;
    Ok(date
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true))
}

#[cfg(unix)]
fn metadata_version(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!(
        "{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128,
    )
}

#[cfg(not(unix))]
fn metadata_version(metadata: &std::fs::Metadata) -> String {
    let mtime = metadata.modified().ok().map(system_time_ns).unwrap_or(0);
    let ctime = metadata.created().ok().map(system_time_ns).unwrap_or(0);
    format!("0:0:{}:{mtime}:{ctime}", metadata.len())
}

#[cfg(not(unix))]
fn system_time_ns(value: SystemTime) -> u128 {
    value
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

pub fn register_file_workspace_persistence() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "workspace",
    );
}
