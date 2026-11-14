use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::features::snapshot::snapshot::{
    SNAPSHOT_RECORD_VERSION, SnapshotAgentMark, SnapshotAgentMarkInput, SnapshotPruneOptions,
    SnapshotPruneResult, SnapshotRecord, SnapshotRecordInput, SnapshotStore, SnapshotWriteMode,
};
use crate::rpc::types::BoxFuture;

use super::operationLog::{
    OperationKind, OperationLogError, OperationLogErrorCode, OperationRecord, OperationView,
};
use super::operationLogStore::{OperationLogStore, PartialRetention};
use super::repositoryViewService::{
    RepositoryFile, RepositoryFileQuery, RepositoryViewService, RepositoryViewServiceOptions,
};

pub struct OperationSnapshotStore {
    repo_root: PathBuf,
    backend: RepositoryViewService,
    store: OperationLogStore,
}

impl OperationSnapshotStore {
    pub async fn open(
        repo_root: impl AsRef<Path>,
        retention: PartialRetention,
    ) -> Result<Self, OperationLogError> {
        let repo_root = resolve_path(repo_root.as_ref());
        let backend = RepositoryViewService::open(
            &repo_root,
            RepositoryViewServiceOptions::default(),
        )
        .await?;
        let store = OperationLogStore::new(backend.storage_root(), retention)?;
        Ok(Self {
            repo_root,
            backend,
            store,
        })
    }

    pub async fn list(&self, path: &str) -> Result<Vec<SnapshotRecord>, OperationLogError> {
        let path_key = self.path_key(path)?;
        let backend = &self.backend;
        let store = &self.store;
        let operations = store.list(None).await?;
        let mut ids: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for operation in &operations {
            if let Some(before_view_id) = &operation.content.before_view_id
                && seen.insert(before_view_id.clone())
            {
                ids.push(before_view_id.clone());
            }
            if seen.insert(operation.content.view_id.clone()) {
                ids.push(operation.content.view_id.clone());
            }
        }
        let views = store.views(&ids).await?;
        let mut views_by_id: HashMap<String, OperationView> = HashMap::new();
        for (index, id) in ids.iter().enumerate() {
            let view = views.get(index).cloned().ok_or_else(|| {
                OperationLogError::new(
                    OperationLogErrorCode::NotFound,
                    format!("Operation view not found: {id}"),
                )
            })?;
            views_by_id.insert(id.clone(), view);
        }
        let mut queries: Vec<RepositoryFileQuery> = Vec::new();
        for operation in &operations {
            if let Some(before_view_id) = &operation.content.before_view_id {
                queries.push(RepositoryFileQuery {
                    view: views_by_id
                        .get(before_view_id)
                        .cloned()
                        .ok_or_else(|| view_not_found(before_view_id))?,
                    path: path_key.clone(),
                });
            }
            queries.push(RepositoryFileQuery {
                view: views_by_id
                    .get(&operation.content.view_id)
                    .cloned()
                    .ok_or_else(|| view_not_found(&operation.content.view_id))?,
                path: path_key.clone(),
            });
        }
        let files = backend.read_files(&queries).await?;
        let mut records: Vec<SnapshotRecord> = Vec::new();
        let mut cursor = 0usize;
        for operation in &operations {
            let before_file = if operation.content.before_view_id.is_some() {
                let file = files.get(cursor).cloned().flatten();
                cursor += 1;
                file
            } else {
                None
            };
            let after_file = {
                let file = files.get(cursor).cloned().flatten();
                cursor += 1;
                file
            };
            let before_hash = before_file.as_ref().map(|file| file.hash.clone());
            let after_hash = after_file.as_ref().map(|file| file.hash.clone());
            if before_hash == after_hash {
                continue;
            }
            let origin = operation.content.origin.as_ref();
            records.push(SnapshotRecord {
                version: SNAPSHOT_RECORD_VERSION,
                id: operation.id.clone(),
                path: path_key.clone(),
                hash: before_hash.unwrap_or_default(),
                size: before_file.as_ref().map(|file| file.size).unwrap_or(0),
                at: operation.content.at.clone(),
                tool: origin
                    .and_then(|origin| origin.tool_name.clone())
                    .unwrap_or_else(|| kind_name(operation.content.kind).to_string()),
                mode: if before_file.is_none() {
                    SnapshotWriteMode::Overwrite
                } else {
                    SnapshotWriteMode::Edit
                },
                session_id: origin.and_then(|origin| origin.session_id.clone()),
                agent_id: origin.and_then(|origin| origin.agent_id.clone()),
                previous_id: operation.content.parents.first().cloned(),
                mtime_ms: None,
                skipped: None,
                after_hash: Some(after_hash.unwrap_or_default()),
                after_size: Some(after_file.as_ref().map(|file| file.size).unwrap_or(0)),
            });
        }
        Ok(records)
    }

    pub async fn content(&self, hash: &str) -> Result<Option<Vec<u8>>, OperationLogError> {
        if hash.is_empty() {
            return Ok(None);
        }
        self.backend.read_blob(hash).await.map(Some)
    }

    pub async fn read_files(
        &self,
        view: &OperationView,
        paths: &[String],
    ) -> Result<Vec<Option<RepositoryFile>>, OperationLogError> {
        let mut queries: Vec<RepositoryFileQuery> = Vec::with_capacity(paths.len());
        for path in paths {
            queries.push(RepositoryFileQuery {
                view: view.clone(),
                path: self.path_key(path)?,
            });
        }
        self.backend.read_files(&queries).await
    }

    pub async fn read_blobs(
        &self,
        hashes: &[String],
    ) -> Result<Vec<Option<Vec<u8>>>, OperationLogError> {
        self.backend.read_blobs(hashes).await
    }

    pub async fn agents(
        &self,
        session_id: &str,
    ) -> Result<Vec<SnapshotAgentMark>, OperationLogError> {
        let mut marks: Vec<SnapshotAgentMark> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for operation in self.operations(None, None).await? {
            let Some(origin) = operation.content.origin.as_ref() else {
                continue;
            };
            if origin.session_id.as_deref() != Some(session_id) {
                continue;
            }
            let Some(agent_id) = origin.agent_id.clone() else {
                continue;
            };
            if !seen.insert(agent_id.clone()) {
                continue;
            }
            marks.push(SnapshotAgentMark {
                version: SNAPSHOT_RECORD_VERSION,
                agent_id,
                session_id: session_id.to_string(),
                at: operation.content.at.clone(),
                cwd: self.repo_root.display().to_string(),
                parent_agent_id: None,
                profile_name: None,
                swarm_index: None,
            });
        }
        Ok(marks)
    }

    pub async fn operations(
        &self,
        turn_id: Option<u64>,
        agent_id: Option<&str>,
    ) -> Result<Vec<OperationRecord>, OperationLogError> {
        let operations = self.store.list(None).await?;
        Ok(operations
            .into_iter()
            .filter(|operation| {
                let origin = operation.content.origin.as_ref();
                turn_id.is_none_or(|turn| origin.and_then(|origin| origin.turn_id) == Some(turn))
                    && agent_id.is_none_or(|agent| {
                        origin.and_then(|origin| origin.agent_id.as_deref()) == Some(agent)
                    })
            })
            .collect())
    }

    pub async fn view(&self, id: &str) -> Result<OperationView, OperationLogError> {
        self.store.view(id).await
    }

    pub async fn file_at(
        &self,
        view: &OperationView,
        path: &str,
    ) -> Result<Option<RepositoryFile>, OperationLogError> {
        let path = self.path_key(path)?;
        self.backend.read_file(view, &path).await
    }

    pub async fn changed_files(
        &self,
        before: &OperationView,
        after: &OperationView,
    ) -> Result<Vec<String>, OperationLogError> {
        self.backend.changed_files(before, after).await
    }

    pub fn record(
        &self,
        _input: SnapshotRecordInput,
    ) -> Result<SnapshotRecord, OperationLogError> {
        self.read_only()
    }

    pub fn complete(
        &self,
        _path: &str,
        _id: &str,
        _after_hash: &str,
        _after_size: u64,
    ) -> Result<bool, OperationLogError> {
        self.read_only()
    }

    pub fn remove(&self, _path: &str, _id: &str) -> Result<bool, OperationLogError> {
        self.read_only()
    }

    pub fn mark_agent(&self, _input: SnapshotAgentMarkInput) -> Result<(), OperationLogError> {
        self.read_only()
    }

    pub fn prune(&self, _options: SnapshotPruneOptions) -> Result<SnapshotPruneResult, OperationLogError> {
        self.read_only()
    }

    fn path_key(&self, path: &str) -> Result<String, OperationLogError> {
        let candidate = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.repo_root.join(path)
        };
        let normalized = normalize_lexical(&candidate);
        let root = normalize_lexical(&self.repo_root);
        let key = normalized
            .strip_prefix(&root)
            .map_err(|_| outside_error())?
            .to_string_lossy()
            .replace('\\', "/");
        if key.is_empty() || key == ".." || key.starts_with("../") || Path::new(&key).is_absolute()
        {
            return Err(outside_error());
        }
        Ok(key)
    }

    fn read_only<T>(&self) -> Result<T, OperationLogError> {
        Err(OperationLogError::new(
            OperationLogErrorCode::Invalid,
            "Operation snapshot store is read-only",
        ))
    }
}

impl SnapshotStore for OperationSnapshotStore {
    fn record<'a>(
        &'a self,
        input: SnapshotRecordInput,
    ) -> BoxFuture<'a, Result<SnapshotRecord, String>> {
        Box::pin(async move {
            OperationSnapshotStore::record(self, input).map_err(|error| error.to_string())
        })
    }

    fn complete<'a>(
        &'a self,
        path: String,
        id: String,
        after_hash: String,
        after_size: u64,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            OperationSnapshotStore::complete(self, &path, &id, &after_hash, after_size)
                .map_err(|error| error.to_string())
        })
    }

    fn remove<'a>(&'a self, path: String, id: String) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            OperationSnapshotStore::remove(self, &path, &id).map_err(|error| error.to_string())
        })
    }

    fn list<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<SnapshotRecord>, String>> {
        Box::pin(async move {
            OperationSnapshotStore::list(self, &path)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn content<'a>(&'a self, hash: String) -> BoxFuture<'a, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            OperationSnapshotStore::content(self, &hash)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn mark_agent<'a>(
        &'a self,
        input: SnapshotAgentMarkInput,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            OperationSnapshotStore::mark_agent(self, input).map_err(|error| error.to_string())
        })
    }

    fn agents<'a>(
        &'a self,
        session_id: String,
    ) -> BoxFuture<'a, Result<Vec<SnapshotAgentMark>, String>> {
        Box::pin(async move {
            OperationSnapshotStore::agents(self, &session_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn prune<'a>(
        &'a self,
        options: SnapshotPruneOptions,
    ) -> BoxFuture<'a, Result<SnapshotPruneResult, String>> {
        Box::pin(async move {
            OperationSnapshotStore::prune(self, options).map_err(|error| error.to_string())
        })
    }
}

fn kind_name(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Capture => "capture",
        OperationKind::Undo => "undo",
        OperationKind::Redo => "redo",
        OperationKind::Restore => "restore",
    }
}

fn view_not_found(id: &str) -> OperationLogError {
    OperationLogError::new(
        OperationLogErrorCode::NotFound,
        format!("Operation view not found: {id}"),
    )
}

fn outside_error() -> OperationLogError {
    OperationLogError::new(
        OperationLogErrorCode::Invalid,
        "Operation path is outside the repository",
    )
}

fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
