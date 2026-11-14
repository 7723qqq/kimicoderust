use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use indexmap::IndexMap;
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::sync::Mutex;

use super::fileHistory::{
    FileBackupEntry, FileHistoryChange, FileHistoryChangeStatus, FileHistoryCheckpointPhase,
    FileHistoryCheckpointRecord, FileHistoryContent, FileHistoryState, FILE_HISTORY_BLOB_PREFIX,
};
use super::fileHistoryOps::{
    FileHistoryCheckpointed, FileHistoryTracked, apply_checkpointed, apply_tracked,
    checkpoint_phase_of, displaced_checkpoints,
};
use super::fileHistoryRetention::{FileHistoryRetentionInput, touch_file_history_session};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::features::operationLog::operationLog::OperationLogError;
use crate::features::operationLog::operationSnapshotStore::OperationSnapshotStore;
use crate::session::sqlite_store::{
    FileHistoryChange as SessionFileHistoryChange,
    FileHistoryContent as SessionFileHistoryContent, FileRevertError, SqliteSessionStore,
};

pub const FILE_HISTORY_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const FILE_HISTORY_READ_ATTEMPTS: usize = 3;
pub const LCS_CELL_BUDGET: usize = 4_000_000;
pub const LCS_AGGREGATE_CELL_BUDGET: usize = 16_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentFile {
    Bytes(Vec<u8>),
    Missing,
    Unreadable,
    Oversize { size: u64, mtime_ms: Option<i64> },
}

#[async_trait]
pub trait FileHistoryBackend: Send + Sync {
    async fn read_current_once(&self, path: &str) -> std::io::Result<CurrentFile>;
    async fn blob_has(&self, key: &str) -> std::io::Result<bool>;
    async fn blob_get(&self, key: &str) -> std::io::Result<Option<Vec<u8>>>;
    async fn blob_put(&self, key: &str, content: &[u8]) -> std::io::Result<()>;
    async fn blob_delete(&self, key: &str) -> std::io::Result<()>;
    async fn blob_list(&self, prefix: &str) -> std::io::Result<Vec<String>>;
}

pub struct LocalFileHistoryBackend {
    work_dir: PathBuf,
    blob_root: PathBuf,
}

impl LocalFileHistoryBackend {
    pub fn new(work_dir: impl Into<PathBuf>, blob_root: impl Into<PathBuf>) -> Self {
        Self {
            work_dir: work_dir.into(),
            blob_root: blob_root.into(),
        }
    }

    fn blob_path(&self, key: &str) -> std::io::Result<PathBuf> {
        let path = Path::new(key);
        if path.is_absolute()
            || path.components().any(|component| {
                !matches!(component, Component::Normal(_))
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid file history blob key",
            ));
        }
        Ok(self.blob_root.join(path))
    }
}

#[async_trait]
impl FileHistoryBackend for LocalFileHistoryBackend {
    async fn read_current_once(&self, path: &str) -> std::io::Result<CurrentFile> {
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.work_dir.join(path)
        };
        let info = match fs::metadata(&absolute).await {
            Ok(info) => info,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CurrentFile::Missing);
            }
            Err(_) => return Ok(CurrentFile::Unreadable),
        };
        if !info.is_file() {
            return Ok(CurrentFile::Unreadable);
        }
        let mtime_ms = info
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64);
        if info.len() > FILE_HISTORY_MAX_FILE_BYTES {
            return Ok(CurrentFile::Oversize {
                size: info.len(),
                mtime_ms,
            });
        }
        let bytes = match fs::read(&absolute).await {
            Ok(bytes) => bytes,
            Err(_) => return Ok(CurrentFile::Unreadable),
        };
        if bytes.len() as u64 > FILE_HISTORY_MAX_FILE_BYTES {
            let grown = fs::metadata(&absolute).await.ok();
            let size = grown
                .as_ref()
                .map(|info| info.len())
                .unwrap_or(bytes.len() as u64);
            let mtime_ms = grown.and_then(|info| {
                info.modified()
                    .ok()
                    .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as i64)
            });
            return Ok(CurrentFile::Oversize { size, mtime_ms });
        }
        if bytes.len() as u64 != info.len() {
            return Ok(CurrentFile::Unreadable);
        }
        Ok(CurrentFile::Bytes(bytes))
    }

    async fn blob_has(&self, key: &str) -> std::io::Result<bool> {
        Ok(fs::try_exists(self.blob_path(key)?).await?)
    }

    async fn blob_get(&self, key: &str) -> std::io::Result<Option<Vec<u8>>> {
        match fs::read(self.blob_path(key)?).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn blob_put(&self, key: &str, content: &[u8]) -> std::io::Result<()> {
        let path = self.blob_path(key)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(path, content).await
    }

    async fn blob_delete(&self, key: &str) -> std::io::Result<()> {
        match fs::remove_file(self.blob_path(key)?).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn blob_list(&self, prefix: &str) -> std::io::Result<Vec<String>> {
        let root = self.blob_path(prefix)?;
        let mut keys = Vec::new();
        collect_blob_keys(&self.blob_root, &root, &mut keys).await?;
        Ok(keys)
    }
}

async fn collect_blob_keys(
    blob_root: &Path,
    directory: &Path,
    out: &mut Vec<String>,
) -> std::io::Result<()> {
    let mut entries = match fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if entry.file_type().await?.is_dir() {
            Box::pin(collect_blob_keys(blob_root, &path, out)).await?;
        } else if let Ok(relative) = path.strip_prefix(blob_root) {
            out.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct AgentFileHistoryInner {
    history: FileHistoryState,
    active_turn_id: Option<u64>,
    orphan_sweep_done: bool,
}

pub struct AgentFileHistoryService {
    agent_id: String,
    work_dir: PathBuf,
    backend: Arc<dyn FileHistoryBackend>,
    operation_store: Option<Arc<OperationSnapshotStore>>,
    retention: Option<FileHistoryRetentionInput>,
    inner: Mutex<AgentFileHistoryInner>,
}

impl AgentFileHistoryService {
    pub fn new(
        agent_id: impl Into<String>,
        work_dir: impl Into<PathBuf>,
        backend: Arc<dyn FileHistoryBackend>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            work_dir: work_dir.into(),
            backend,
            operation_store: None,
            retention: None,
            inner: Mutex::new(AgentFileHistoryInner::default()),
        }
    }

    pub fn with_operation_store(mut self, store: Arc<OperationSnapshotStore>) -> Self {
        self.operation_store = Some(store);
        self
    }

    pub fn with_retention(mut self, retention: FileHistoryRetentionInput) -> Self {
        self.retention = Some(retention);
        self
    }

    pub async fn history(&self) -> FileHistoryState {
        self.inner.lock().await.history.clone()
    }

    pub async fn settled(&self) {
        drop(self.inner.lock().await);
    }

    pub async fn turn_started(&self, agent_id: &str, turn_id: u64) {
        if self.operation_store.is_some() || agent_id != self.agent_id {
            return;
        }
        self.inner.lock().await.active_turn_id = Some(turn_id);
    }

    pub async fn turn_ended(&self, agent_id: &str, turn_id: u64) -> Result<(), String> {
        if self.operation_store.is_some() || agent_id != self.agent_id {
            return Ok(());
        }
        {
            let mut inner = self.inner.lock().await;
            if inner.active_turn_id == Some(turn_id) {
                inner.active_turn_id = None;
            }
        }
        self.end_checkpoint(turn_id).await
    }

    pub async fn capture_for_active_turn(&self, path: &str) -> Result<(), String> {
        if self.operation_store.is_some() {
            return Ok(());
        }
        let turn_id = self.inner.lock().await.active_turn_id;
        if let Some(turn_id) = turn_id {
            self.capture(path, turn_id).await?;
        }
        Ok(())
    }

    pub async fn changes(&self, turn_id: u64) -> Result<Vec<FileHistoryChange>, String> {
        if let Some(store) = &self.operation_store {
            return operation_file_history_changes(store, turn_id, &self.agent_id)
                .await
                .map(|changes| changes.into_iter().map(FileHistoryChange::from).collect())
                .map_err(|error| error.to_string());
        }
        let inner = self.inner.lock().await;
        self.read_changes(&inner, turn_id).await
    }

    pub async fn turn_recorded(&self, turn_id: u64) -> Result<bool, String> {
        if let Some(store) = &self.operation_store {
            return store
                .operations(Some(turn_id), Some(&self.agent_id))
                .await
                .map(|operations| !operations.is_empty())
                .map_err(|error| error.to_string());
        }
        let inner = self.inner.lock().await;
        let state = &inner.history;
        let Some((index, start)) = state
            .checkpoints
            .iter()
            .enumerate()
            .find(|(_, checkpoint)| {
                checkpoint.turn_id == turn_id
                    && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::Start
            })
        else {
            return Ok(false);
        };
        let end = state.checkpoints.iter().find(|checkpoint| {
            checkpoint.turn_id == turn_id
                && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::End
        });
        let live = end.is_none()
            && index == state.checkpoints.len() - 1
            && inner.active_turn_id == Some(turn_id);
        if end.is_none() && !live {
            return Ok(false);
        }
        let keys: HashSet<&str> = start
            .entries
            .values()
            .chain(end.into_iter().flat_map(|checkpoint| checkpoint.entries.values()))
            .filter_map(|entry| entry.key.as_deref())
            .collect();
        for key in keys {
            if !self.backend.blob_has(key).await.map_err(|error| error.to_string())? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn content_at(
        &self,
        turn_id: u64,
        path: &str,
        phase: FileHistoryCheckpointPhase,
    ) -> Result<Option<FileHistoryContent>, String> {
        if let Some(store) = &self.operation_store {
            let phase_name = match phase {
                FileHistoryCheckpointPhase::Start => "start",
                FileHistoryCheckpointPhase::End => "end",
            };
            return operation_file_history_content(
                store,
                turn_id,
                &self.agent_id,
                path,
                phase_name,
            )
            .await
            .map(|content| content.map(FileHistoryContent::from))
            .map_err(|error| error.to_string());
        }
        let inner = self.inner.lock().await;
        self.read_content_at(&inner, turn_id, path, phase).await
    }

    async fn read_changes(
        &self,
        inner: &AgentFileHistoryInner,
        turn_id: u64,
    ) -> Result<Vec<FileHistoryChange>, String> {
        let state = &inner.history;
        let Some((index, start)) = state
            .checkpoints
            .iter()
            .enumerate()
            .find(|(_, checkpoint)| {
                checkpoint.turn_id == turn_id
                    && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::Start
            })
        else {
            return Ok(Vec::new());
        };
        let end = state.checkpoints.iter().find(|checkpoint| {
            checkpoint.turn_id == turn_id
                && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::End
        });
        let live = end.is_none()
            && index == state.checkpoints.len() - 1
            && inner.active_turn_id == Some(turn_id);
        if end.is_none() && !live {
            return Ok(Vec::new());
        }
        let mut paths: Vec<String> = end
            .map(|checkpoint| checkpoint.entries.keys().cloned().collect())
            .unwrap_or_else(|| start.entries.keys().cloned().collect());
        paths.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        let mut changes = Vec::new();
        let mut budget = LcsCellBudget {
            remaining: LCS_AGGREGATE_CELL_BUDGET,
        };
        for path in paths {
            let before = start.entries.get(&path);
            let after = end.and_then(|checkpoint| checkpoint.entries.get(&path));
            if end.is_some() && before.map(|entry| entry.version) == after.map(|entry| entry.version)
            {
                continue;
            }
            if before.is_some_and(|entry| entry.oversize == Some(true))
                && after.is_some_and(|entry| entry.oversize == Some(true))
            {
                if before.map(|entry| entry.version) != after.map(|entry| entry.version) {
                    changes.push(change(&path, FileHistoryChangeStatus::Modified).oversize());
                }
                continue;
            }
            let before_missing = before.is_none_or(|entry| {
                entry.key.is_none() && entry.oversize != Some(true)
            });
            let mut live_oversize = None;
            let mut live_missing = false;
            let mut after_bytes = None;
            if end.is_some() {
                if after.is_some_and(|entry| entry.unreadable == Some(true)) {
                    changes.push(
                        change(
                            &path,
                            if before_missing {
                                FileHistoryChangeStatus::Added
                            } else {
                                FileHistoryChangeStatus::Modified
                            },
                        )
                        .unreadable(),
                    );
                    continue;
                }
                if before.is_none_or(|entry| entry.oversize != Some(true))
                    && after.is_none_or(|entry| entry.oversize != Some(true))
                {
                    after_bytes = self.entry_bytes(after).await?;
                }
            } else {
                match self.read_current(&path).await {
                    CurrentFile::Unreadable => {
                        changes.push(
                            change(
                                &path,
                                if before_missing {
                                    FileHistoryChangeStatus::Added
                                } else {
                                    FileHistoryChangeStatus::Modified
                                },
                            )
                            .unreadable(),
                        );
                        continue;
                    }
                    CurrentFile::Bytes(bytes) => after_bytes = Some(bytes),
                    CurrentFile::Missing => live_missing = true,
                    CurrentFile::Oversize { size, mtime_ms } => {
                        live_oversize = Some((size, mtime_ms));
                    }
                }
            }
            let after_missing = if end.is_some() {
                after.is_none_or(|entry| entry.key.is_none() && entry.oversize != Some(true))
            } else {
                live_missing
            };
            if before.is_some_and(|entry| entry.oversize == Some(true))
                || after.is_some_and(|entry| entry.oversize == Some(true))
                || live_oversize.is_some()
            {
                if let (Some(before), Some((size, mtime_ms))) = (before, live_oversize)
                    && before.oversize == Some(true)
                    && before.size == Some(size)
                    && before.mtime_ms == mtime_ms
                {
                    continue;
                }
                let status = if before_missing {
                    FileHistoryChangeStatus::Added
                } else if after_missing {
                    FileHistoryChangeStatus::Deleted
                } else {
                    FileHistoryChangeStatus::Modified
                };
                changes.push(change(&path, status).oversize());
                continue;
            }
            let before_bytes = self.entry_bytes(before).await?;
            let before_lost = before.is_some_and(|entry| entry.key.is_some()) && before_bytes.is_none();
            let after_lost = end.is_some()
                && after.is_some_and(|entry| entry.key.is_some())
                && after_bytes.is_none();
            if before_lost || after_lost {
                let status = if before_missing {
                    FileHistoryChangeStatus::Added
                } else if after_missing {
                    FileHistoryChangeStatus::Deleted
                } else {
                    FileHistoryChangeStatus::Modified
                };
                changes.push(change(&path, status).binary());
                continue;
            }
            if let Some(change) = diff_change(&path, before_bytes, after_bytes, Some(&mut budget)) {
                changes.push(change);
            }
        }
        Ok(changes)
    }

    async fn read_content_at(
        &self,
        inner: &AgentFileHistoryInner,
        turn_id: u64,
        path: &str,
        phase: FileHistoryCheckpointPhase,
    ) -> Result<Option<FileHistoryContent>, String> {
        let state = &inner.history;
        let Some(record) = state.checkpoints.iter().find(|checkpoint| {
            checkpoint.turn_id == turn_id && checkpoint_phase_of(checkpoint) == phase
        }) else {
            return Ok(None);
        };
        let path_key = self.path_key(inner, path);
        let mut entry = record.entries.get(&path_key);
        if entry.is_none() && phase == FileHistoryCheckpointPhase::End {
            entry = state
                .checkpoints
                .iter()
                .find(|checkpoint| {
                    checkpoint.turn_id == turn_id
                        && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::Start
                })
                .and_then(|start| start.entries.get(&path_key));
        }
        let Some(entry) = entry else {
            return Ok(None);
        };
        if entry.oversize == Some(true) {
            return Ok(None);
        }
        let Some(key) = &entry.key else {
            return Ok(Some(FileHistoryContent {
                version: entry.version,
                content: None,
                binary: None,
            }));
        };
        let Some(bytes) = self
            .backend
            .blob_get(key)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(match decode_text(&bytes) {
            Some(content) => FileHistoryContent {
                version: entry.version,
                content: Some(content),
                binary: None,
            },
            None => FileHistoryContent {
                version: entry.version,
                content: None,
                binary: Some(true),
            },
        }))
    }

    async fn capture(&self, path: &str, turn_id: u64) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        let path_key = self.path_key(&inner, path);
        if inner.history.checkpoints.iter().any(|checkpoint| {
            checkpoint.turn_id == turn_id
                && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::Start
                && checkpoint.entries.contains_key(&path_key)
        }) {
            return Ok(());
        }
        let current = self.read_current(&path_key).await;
        if current == CurrentFile::Unreadable {
            return Ok(());
        }
        let latest = latest_entry(&inner.history.checkpoints, &path_key).cloned();
        let next_version = max_version(&inner.history.checkpoints, &path_key) + 1;
        let entry = match current {
            CurrentFile::Missing => match latest {
                Some(latest) if latest.key.is_none() && latest.oversize != Some(true) => latest,
                _ => FileBackupEntry {
                    key: None,
                    version: next_version,
                    ..FileBackupEntry::default()
                },
            },
            CurrentFile::Bytes(bytes) => {
                let content_hash = sha256(&bytes);
                match latest {
                    Some(latest) if latest.content_hash.as_deref() == Some(&content_hash) => latest,
                    _ => {
                        self.backup(&path_key, next_version, &bytes, Some(content_hash))
                            .await?
                    }
                }
            }
            CurrentFile::Oversize { size, mtime_ms } => match latest {
                Some(latest)
                    if latest.oversize == Some(true)
                        && latest.size == Some(size)
                        && latest.mtime_ms == mtime_ms =>
                {
                    latest
                }
                _ => FileBackupEntry {
                    key: None,
                    version: next_version,
                    size: Some(size),
                    oversize: Some(true),
                    mtime_ms,
                    ..FileBackupEntry::default()
                },
            },
            CurrentFile::Unreadable => return Ok(()),
        };
        apply_tracked(
            &mut inner.history,
            &FileHistoryTracked {
                agent_id: self.agent_id.clone(),
                turn_id,
                path: path_key,
                entry,
            },
        );
        Ok(())
    }

    async fn end_checkpoint(&self, turn_id: u64) -> Result<(), String> {
        let mut inner = self.inner.lock().await;
        if inner.history.checkpoints.iter().any(|checkpoint| {
            checkpoint.turn_id == turn_id
                && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::End
        }) {
            return Ok(());
        }
        let Some(start) = inner
            .history
            .checkpoints
            .iter()
            .find(|checkpoint| {
                checkpoint.turn_id == turn_id
                    && checkpoint_phase_of(checkpoint) == FileHistoryCheckpointPhase::Start
            })
            .cloned()
        else {
            return Ok(());
        };
        self.sweep_orphan_blobs(&mut inner).await;
        if let Some(retention) = self.retention.clone() {
            tokio::spawn(async move {
                touch_file_history_session(retention).await;
            });
        }
        let mut entries = IndexMap::new();
        for (path_key, before) in &start.entries {
            let next_version = max_version(&inner.history.checkpoints, path_key) + 1;
            match self.read_current(path_key).await {
                CurrentFile::Unreadable => {
                    entries.insert(
                        path_key.clone(),
                        FileBackupEntry {
                            key: None,
                            version: next_version,
                            unreadable: Some(true),
                            ..FileBackupEntry::default()
                        },
                    );
                }
                CurrentFile::Missing => {
                    if before.key.is_some() || before.oversize == Some(true) {
                        entries.insert(
                            path_key.clone(),
                            FileBackupEntry {
                                key: None,
                                version: next_version,
                                ..FileBackupEntry::default()
                            },
                        );
                    }
                }
                CurrentFile::Oversize { size, mtime_ms } => {
                    if before.oversize != Some(true)
                        || before.size != Some(size)
                        || before.mtime_ms != mtime_ms
                    {
                        entries.insert(
                            path_key.clone(),
                            FileBackupEntry {
                                key: None,
                                version: next_version,
                                size: Some(size),
                                oversize: Some(true),
                                mtime_ms,
                                ..FileBackupEntry::default()
                            },
                        );
                    }
                }
                CurrentFile::Bytes(bytes) => {
                    let content_hash = sha256(&bytes);
                    if before.content_hash.as_deref() != Some(&content_hash) {
                        entries.insert(
                            path_key.clone(),
                            self.backup(path_key, next_version, &bytes, Some(content_hash))
                                .await?,
                        );
                    }
                }
            }
        }
        let evictable = displaced_checkpoints(&inner.history.checkpoints, Some(turn_id));
        apply_checkpointed(
            &mut inner.history,
            &FileHistoryCheckpointed {
                agent_id: self.agent_id.clone(),
                turn_id,
                phase: Some(FileHistoryCheckpointPhase::End),
                entries,
            },
        );
        if !evictable.is_empty() {
            self.evict_blobs(&inner.history, &evictable).await;
        }
        Ok(())
    }

    async fn backup(
        &self,
        path_key: &str,
        version: u64,
        content: &[u8],
        content_hash: Option<String>,
    ) -> Result<FileBackupEntry, String> {
        let hash = content_hash.unwrap_or_else(|| sha256(content));
        let key = blob_key(path_key, version);
        self.backend
            .blob_put(&key, content)
            .await
            .map_err(|error| error.to_string())?;
        Ok(FileBackupEntry {
            key: Some(key),
            version,
            content_hash: Some(hash),
            size: Some(content.len() as u64),
            ..FileBackupEntry::default()
        })
    }

    async fn sweep_orphan_blobs(&self, inner: &mut AgentFileHistoryInner) {
        if inner.orphan_sweep_done {
            return;
        }
        inner.orphan_sweep_done = true;
        let referenced: HashSet<&str> = inner
            .history
            .checkpoints
            .iter()
            .flat_map(|checkpoint| checkpoint.entries.values())
            .filter_map(|entry| entry.key.as_deref())
            .collect();
        let keys = match self.backend.blob_list(FILE_HISTORY_BLOB_PREFIX).await {
            Ok(keys) => keys,
            Err(error) => {
                on_unexpected_error(error);
                return;
            }
        };
        for key in keys {
            if referenced.contains(key.as_str()) {
                continue;
            }
            if let Err(error) = self.backend.blob_delete(&key).await {
                on_unexpected_error(error);
            }
        }
    }

    async fn evict_blobs(
        &self,
        state: &FileHistoryState,
        evicted: &[FileHistoryCheckpointRecord],
    ) {
        let retained: HashSet<&str> = state
            .checkpoints
            .iter()
            .flat_map(|checkpoint| checkpoint.entries.values())
            .filter_map(|entry| entry.key.as_deref())
            .collect();
        for entry in evicted
            .iter()
            .flat_map(|checkpoint| checkpoint.entries.values())
        {
            let Some(key) = entry.key.as_deref() else {
                continue;
            };
            if !retained.contains(key)
                && let Err(error) = self.backend.blob_delete(key).await
            {
                on_unexpected_error(error);
            }
        }
    }

    async fn entry_bytes(
        &self,
        entry: Option<&FileBackupEntry>,
    ) -> Result<Option<Vec<u8>>, String> {
        let Some(key) = entry.and_then(|entry| entry.key.as_deref()) else {
            return Ok(None);
        };
        self.backend
            .blob_get(key)
            .await
            .map_err(|error| error.to_string())
    }

    async fn read_current(&self, path_key: &str) -> CurrentFile {
        for attempt in 0..FILE_HISTORY_READ_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            match self.backend.read_current_once(path_key).await {
                Ok(CurrentFile::Unreadable) => continue,
                Ok(current) => return current,
                Err(_) => continue,
            }
        }
        CurrentFile::Unreadable
    }

    fn path_key(&self, inner: &AgentFileHistoryInner, path: &str) -> String {
        let mut raw = path.to_string();
        let candidate = Path::new(path);
        if candidate.is_absolute()
            && let Ok(relative) = candidate.strip_prefix(&self.work_dir)
            && !relative.as_os_str().is_empty()
            && !relative.to_string_lossy().starts_with("..")
            && !relative.is_absolute()
        {
            raw = relative.to_string_lossy().into_owned();
        }
        let case_insensitive = cfg!(windows) || is_windows_path(&self.work_dir.to_string_lossy());
        let key = comparison_key(&raw, case_insensitive);
        inner
            .history
            .tracked
            .iter()
            .find(|tracked| comparison_key(tracked, case_insensitive) == key)
            .cloned()
            .unwrap_or(raw)
    }
}

#[async_trait]
impl super::fileHistory::IAgentFileHistoryService for AgentFileHistoryService {
    async fn history(&self) -> FileHistoryState {
        AgentFileHistoryService::history(self).await
    }

    async fn settled(&self) {
        AgentFileHistoryService::settled(self).await;
    }

    async fn capture_for_active_turn(&self, path: &str) -> Result<(), String> {
        AgentFileHistoryService::capture_for_active_turn(self, path).await
    }

    async fn changes(&self, turn_id: u64) -> Result<Vec<FileHistoryChange>, String> {
        AgentFileHistoryService::changes(self, turn_id).await
    }

    async fn turn_recorded(&self, turn_id: u64) -> Result<bool, String> {
        AgentFileHistoryService::turn_recorded(self, turn_id).await
    }

    async fn content_at(
        &self,
        turn_id: u64,
        path: &str,
        phase: FileHistoryCheckpointPhase,
    ) -> Result<Option<FileHistoryContent>, String> {
        AgentFileHistoryService::content_at(self, turn_id, path, phase).await
    }
}

pub async fn operation_file_history_changes(
    store: &OperationSnapshotStore,
    turn_id: u64,
    agent_id: &str,
) -> Result<Vec<SessionFileHistoryChange>, OperationLogError> {
    let operations = store.operations(Some(turn_id), Some(agent_id)).await?;
    let (Some(latest), Some(first)) = (operations.first(), operations.last()) else {
        return Ok(Vec::new());
    };
    let Some(before_view_id) = first.content.before_view_id.as_deref() else {
        return Ok(Vec::new());
    };
    let before = store.view(before_view_id).await?;
    let after = store.view(&latest.content.view_id).await?;
    let paths = store.changed_files(&before, &after).await?;
    let (before_files, after_files) = tokio::try_join!(
        store.read_files(&before, &paths),
        store.read_files(&after, &paths)
    )?;
    let mut seen = HashSet::new();
    let mut hashes = Vec::new();
    for file in before_files.iter().chain(after_files.iter()).flatten() {
        if seen.insert(file.hash.clone()) {
            hashes.push(file.hash.clone());
        }
    }
    let blobs = store.read_blobs(&hashes).await?;
    let contents: HashMap<String, Option<Vec<u8>>> = hashes.into_iter().zip(blobs).collect();
    let mut changes = Vec::new();
    let mut budget = LcsCellBudget {
        remaining: LCS_AGGREGATE_CELL_BUDGET,
    };
    for (index, path) in paths.iter().enumerate() {
        let before_file = before_files.get(index).cloned().flatten();
        let after_file = after_files.get(index).cloned().flatten();
        let before_bytes =
            before_file.and_then(|file| contents.get(&file.hash).cloned().flatten());
        let after_bytes =
            after_file.and_then(|file| contents.get(&file.hash).cloned().flatten());
        if let Some(change) = diff_change(path, before_bytes, after_bytes, Some(&mut budget)) {
            changes.push(session_change(change));
        }
    }
    Ok(changes)
}

pub async fn operation_file_history_all_changes(
    store: &OperationSnapshotStore,
    agent_id: &str,
) -> Result<Vec<SessionFileHistoryChange>, OperationLogError> {
    let operations = store.operations(None, Some(agent_id)).await?;
    let turn_ids: HashSet<u64> = operations
        .iter()
        .filter_map(|operation| operation.content.origin.as_ref()?.turn_id)
        .collect();
    let mut turn_ids: Vec<u64> = turn_ids.into_iter().collect();
    turn_ids.sort_unstable();
    let mut changes = Vec::new();
    for turn_id in turn_ids {
        changes.extend(operation_file_history_changes(store, turn_id, agent_id).await?);
    }
    Ok(changes)
}

pub async fn operation_file_history_content(
    store: &OperationSnapshotStore,
    turn_id: u64,
    agent_id: &str,
    path: &str,
    phase: &str,
) -> Result<Option<SessionFileHistoryContent>, OperationLogError> {
    let operations = store.operations(Some(turn_id), Some(agent_id)).await?;
    let operation = if phase == "start" {
        operations.last()
    } else {
        operations.first()
    };
    let Some(operation) = operation else {
        return Ok(None);
    };
    let view_id = if phase == "start" {
        operation.content.before_view_id.as_deref()
    } else {
        Some(operation.content.view_id.as_str())
    };
    let Some(view_id) = view_id else {
        return Ok(None);
    };
    let version = chrono::DateTime::parse_from_rfc3339(&operation.content.at)
        .map(|at| at.timestamp_millis().max(0) as usize)
        .unwrap_or(0);
    let view = store.view(view_id).await?;
    let Some(file) = store.file_at(&view, path).await? else {
        return Ok(Some(SessionFileHistoryContent {
            version,
            content: None,
            binary: None,
            oversize: None,
        }));
    };
    let bytes = store.content(&file.hash).await?;
    let Some(bytes) = bytes else {
        return Ok(Some(SessionFileHistoryContent {
            version,
            content: None,
            binary: Some(true),
            oversize: None,
        }));
    };
    Ok(Some(match decode_text(&bytes) {
        Some(content) => SessionFileHistoryContent {
            version,
            content: Some(content),
            binary: None,
            oversize: None,
        },
        None => SessionFileHistoryContent {
            version,
            content: None,
            binary: Some(true),
            oversize: None,
        },
    }))
}

fn session_change(change: FileHistoryChange) -> SessionFileHistoryChange {
    SessionFileHistoryChange {
        path: change.path,
        status: match change.status {
            FileHistoryChangeStatus::Added => "added",
            FileHistoryChangeStatus::Modified => "modified",
            FileHistoryChangeStatus::Deleted => "deleted",
        }
        .to_string(),
        additions: change.additions,
        deletions: change.deletions,
        binary: change.binary,
        oversize: change.oversize,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LcsCellBudget {
    pub remaining: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineDiff {
    pub additions: usize,
    pub deletions: usize,
}

fn change(path: &str, status: FileHistoryChangeStatus) -> FileHistoryChange {
    FileHistoryChange {
        path: path.to_string(),
        status,
        additions: 0,
        deletions: 0,
        binary: None,
        oversize: None,
        unreadable: None,
    }
}

trait ChangeFlag {
    fn binary(self) -> Self;
    fn oversize(self) -> Self;
    fn unreadable(self) -> Self;
}

impl ChangeFlag for FileHistoryChange {
    fn binary(mut self) -> Self {
        self.binary = Some(true);
        self
    }

    fn oversize(mut self) -> Self {
        self.oversize = Some(true);
        self
    }

    fn unreadable(mut self) -> Self {
        self.unreadable = Some(true);
        self
    }
}

fn is_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        return true;
    }
    if !(value.starts_with("\\\\") || value.starts_with("//")) {
        return false;
    }
    let mut parts = value[2..].split(['\\', '/']);
    parts.next().is_some_and(|server| !server.is_empty())
        && parts.next().is_some_and(|share| !share.is_empty())
}

fn comparison_key(path_key: &str, case_insensitive: bool) -> String {
    if !case_insensitive && !is_windows_path(path_key) {
        return path_key.to_string();
    }
    path_key.replace('\\', "/").to_lowercase()
}

fn latest_entry<'a>(
    checkpoints: &'a [FileHistoryCheckpointRecord],
    path: &str,
) -> Option<&'a FileBackupEntry> {
    checkpoints
        .iter()
        .rev()
        .find_map(|checkpoint| checkpoint.entries.get(path))
}

fn max_version(checkpoints: &[FileHistoryCheckpointRecord], path: &str) -> u64 {
    checkpoints
        .iter()
        .filter_map(|checkpoint| checkpoint.entries.get(path))
        .map(|entry| entry.version)
        .max()
        .unwrap_or(0)
}

fn blob_key(path_key: &str, version: u64) -> String {
    format!("{FILE_HISTORY_BLOB_PREFIX}/{}@v{version}", sha256(path_key.as_bytes()))
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8(bytes.to_vec()).ok()?;
    Some(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
}

pub fn diff_change(
    path: &str,
    before_bytes: Option<Vec<u8>>,
    after_bytes: Option<Vec<u8>>,
    mut budget: Option<&mut LcsCellBudget>,
) -> Option<FileHistoryChange> {
    if before_bytes.is_none() && after_bytes.is_none() {
        return None;
    }
    let before = before_bytes.as_deref().and_then(decode_text);
    let after = after_bytes.as_deref().and_then(decode_text);
    let binary =
        (before_bytes.is_some() && before.is_none()) || (after_bytes.is_some() && after.is_none());
    if before_bytes.is_none() {
        let mut result = change(path, FileHistoryChangeStatus::Added);
        if binary {
            return Some(result.binary());
        }
        result.additions = count_lines(after.as_deref().unwrap_or(""));
        return Some(result);
    }
    if after_bytes.is_none() {
        let mut result = change(path, FileHistoryChangeStatus::Deleted);
        if binary {
            return Some(result.binary());
        }
        result.deletions = count_lines(before.as_deref().unwrap_or(""));
        return Some(result);
    }
    if binary {
        return (before_bytes != after_bytes)
            .then(|| change(path, FileHistoryChangeStatus::Modified).binary());
    }
    if before == after {
        return None;
    }
    let Some(counted) = count_line_diff(
        before.as_deref().unwrap_or(""),
        after.as_deref().unwrap_or(""),
        budget.as_deref_mut(),
    ) else {
        return Some(change(path, FileHistoryChangeStatus::Modified).oversize());
    };
    Some(FileHistoryChange {
        additions: counted.additions,
        deletions: counted.deletions,
        ..change(path, FileHistoryChangeStatus::Modified)
    })
}

fn split_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = content.split('\n').map(str::to_string).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    } else if let Some(last) = lines.last_mut() {
        last.push('\0');
    }
    lines
}

fn count_lines(content: &str) -> usize {
    split_lines(content).len()
}

pub fn count_line_diff(
    before: &str,
    after: &str,
    mut budget: Option<&mut LcsCellBudget>,
) -> Option<LineDiff> {
    let before_lines = split_lines(before);
    let after_lines = split_lines(after);
    let mut start = 0;
    while start < before_lines.len()
        && start < after_lines.len()
        && before_lines[start] == after_lines[start]
    {
        start += 1;
    }
    let mut before_end = before_lines.len();
    let mut after_end = after_lines.len();
    while before_end > start
        && after_end > start
        && before_lines[before_end - 1] == after_lines[after_end - 1]
    {
        before_end -= 1;
        after_end -= 1;
    }
    let old_slice = &before_lines[start..before_end];
    let new_slice = &after_lines[start..after_end];
    let cells = old_slice.len().checked_mul(new_slice.len())?;
    if cells > LCS_CELL_BUDGET {
        return None;
    }
    if let Some(budget) = budget.as_deref_mut() {
        if cells > budget.remaining {
            return None;
        }
        budget.remaining -= cells;
    }
    let common = lcs_length(old_slice, new_slice);
    Some(LineDiff {
        additions: new_slice.len() - common,
        deletions: old_slice.len() - common,
    })
}

fn lcs_length(left: &[String], right: &[String]) -> usize {
    if left.is_empty() || right.is_empty() {
        return 0;
    }
    let mut previous = vec![0usize; right.len() + 1];
    let mut current = vec![0usize; right.len() + 1];
    for left_index in 1..=left.len() {
        for right_index in 1..=right.len() {
            current[right_index] = if left[left_index - 1] == right[right_index - 1] {
                previous[right_index - 1] + 1
            } else {
                previous[right_index].max(current[right_index - 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

pub const FILE_HISTORY_MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;

const CHECKPOINT_MAX_TURNS: i64 = 20;

pub fn compute_line_diff(before: Option<&str>, after: Option<&str>) -> (String, usize, usize) {
    match (before, after) {
        (None, Some(a)) => {
            let lines = a.lines().count();
            ("added".to_string(), lines, 0)
        }
        (Some(b), None) => {
            let lines = b.lines().count();
            ("deleted".to_string(), 0, lines)
        }
        (Some(b), Some(a)) => {
            if b == a {
                ("modified".to_string(), 0, 0)
            } else {
                let b_lines: Vec<&str> = b.lines().collect();
                let a_lines: Vec<&str> = a.lines().collect();
                let b_set: std::collections::HashSet<&str> = b_lines.iter().copied().collect();
                let a_set: std::collections::HashSet<&str> = a_lines.iter().copied().collect();
                let mut additions = 0;
                let mut deletions = 0;
                for l in &a_lines {
                    if !b_set.contains(l) {
                        additions += 1;
                    }
                }
                for l in &b_lines {
                    if !a_set.contains(l) {
                        deletions += 1;
                    }
                }
                if additions == 0 && deletions == 0 {
                    additions = a_lines.len().abs_diff(b_lines.len());
                }
                ("modified".to_string(), additions, deletions)
            }
        }
        (None, None) => ("modified".to_string(), 0, 0),
    }
}

pub(crate) fn reclaim_file_history_space(conn: &Connection) -> Result<(), rusqlite::Error> {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(0) => {}
        Ok(busy) => tracing::warn!(
            busy,
            "file history rows were pruned but another reader held the WAL; the freed space stays on disk until the next checkpoint"
        ),
        Err(error) => tracing::warn!(
            %error,
            "file history rows were pruned but the WAL could not be checkpointed; the freed space may remain on disk until SQLite checkpoints it"
        ),
    }
    Ok(())
}

impl SqliteSessionStore {
    pub fn revert_turn_file_changes(
        &self,
        session_id: &str,
        turn_id: usize,
        workspace_root: &Path,
    ) -> Result<Vec<String>, FileRevertError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT path, content_before, content_after, status, oversize
                 FROM session_file_history
                 WHERE session_id = ?1 AND turn_id = ?2 ORDER BY id DESC",
            )
            .map_err(|e| FileRevertError::Io(e.to_string()))?;

        let rows = stmt
            .query_map(params![session_id, turn_id as i64], |row| {
                let path: String = row.get(0)?;
                let content_before: Option<String> = row.get(1)?;
                let content_after: Option<String> = row.get(2)?;
                let status: String = row.get(3)?;
                let oversize: bool = row.get(4)?;
                Ok((path, content_before, content_after, status, oversize))
            })
            .map_err(|e| FileRevertError::Io(e.to_string()))?;

        let mut reverted = Vec::new();
        for item in rows {
            let (rel_path, before, after, status, oversize) =
                item.map_err(|e| FileRevertError::Io(e.to_string()))?;
            if oversize {
                tracing::warn!(
                    %session_id,
                    path = %rel_path,
                    "cannot revert a file change without a stored pre-image; leaving the file untouched"
                );
                continue;
            }
            let full_path = workspace_root.join(&rel_path);
            let current = match std::fs::read_to_string(&full_path) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(FileRevertError::Io(format!(
                        "Failed to read {}: {error}",
                        full_path.display()
                    )));
                }
            };

            if status == "added" {
                let Some(current) = current else {
                    continue;
                };
                if let Some(expected) = &after
                    && *expected != current
                {
                    return Err(FileRevertError::Conflict {
                        path: rel_path,
                        reason: "it was edited after the tool wrote it".into(),
                    });
                }
                std::fs::remove_file(&full_path).map_err(|e| {
                    FileRevertError::Io(format!("Failed to remove {}: {e}", full_path.display()))
                })?;
            } else {
                let Some(current) = current else {
                    return Err(FileRevertError::Conflict {
                        path: rel_path,
                        reason: "it was deleted after the tool wrote it".into(),
                    });
                };
                if let Some(expected) = &after
                    && *expected != current
                {
                    return Err(FileRevertError::Conflict {
                        path: rel_path,
                        reason: "it was edited after the tool wrote it".into(),
                    });
                }
                let Some(text) = before else {
                    tracing::warn!(
                        %session_id,
                        path = %full_path.display(),
                        "cannot revert a file change without a stored pre-image; leaving the file untouched"
                    );
                    continue;
                };
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        FileRevertError::Io(format!("Failed to create {}: {e}", parent.display()))
                    })?;
                }
                std::fs::write(&full_path, text).map_err(|e| {
                    FileRevertError::Io(format!("Failed to restore {}: {e}", full_path.display()))
                })?;
            }
            reverted.push(rel_path);
        }
        Ok(reverted)
    }

    pub fn record_file_change(
        &self,
        session_id: &str,
        turn_id: usize,
        path: &str,
        content_before: Option<&str>,
        content_after: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let (status, additions, deletions) = compute_line_diff(content_before, content_after);
        let before_bytes = content_before.map_or(0, str::len);
        let after_bytes = content_after.map_or(0, str::len);
        let oversize = before_bytes > FILE_HISTORY_MAX_CONTENT_BYTES
            || after_bytes > FILE_HISTORY_MAX_CONTENT_BYTES;
        if oversize {
            tracing::warn!(
                %session_id,
                %path,
                before_bytes,
                after_bytes,
                limit_bytes = FILE_HISTORY_MAX_CONTENT_BYTES,
                "file history content is over the byte cap: recording the change without its content"
            );
        }
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO session_file_history (session_id, turn_id, path, status, content_before, content_after, additions, deletions, oversize, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                turn_id as i64,
                path,
                status,
                if oversize { None } else { content_before },
                if oversize { None } else { content_after },
                additions as i64,
                deletions as i64,
                oversize,
                now
            ],
        )?;

        conn.execute(
            "DELETE FROM session_file_history
             WHERE session_id = ?1 AND id NOT IN (
                 SELECT id FROM session_file_history
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT 500
             )",
            params![session_id],
        )?;

        conn.execute(
            "DELETE FROM session_file_history
             WHERE session_id = ?1 AND turn_id NOT IN (
                 SELECT DISTINCT turn_id FROM session_file_history
                 WHERE session_id = ?1
                 ORDER BY turn_id DESC
                 LIMIT ?2
             )",
            params![session_id, CHECKPOINT_MAX_TURNS],
        )?;
        Ok(())
    }

    pub fn prune_file_history(
        &self,
        session_id: &str,
        max_entries: usize,
    ) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let deleted = conn.execute(
            "DELETE FROM session_file_history
             WHERE session_id = ?1 AND id NOT IN (
                 SELECT id FROM session_file_history
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT ?2
             )",
            params![session_id, max_entries as i64],
        )?;
        if deleted > 0 {
            reclaim_file_history_space(&conn)?;
        }
        Ok(deleted)
    }

    pub fn get_file_history_changes(
        &self,
        session_id: &str,
        turn_id: Option<usize>,
    ) -> Result<(Vec<crate::session::sqlite_store::FileHistoryChange>, bool), rusqlite::Error> {
        let conn = self.conn.lock();
        let (query, has_turn) = match turn_id {
            Some(t) => (
                "SELECT path, status, additions, deletions, oversize FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2 ORDER BY id ASC",
                Some(t as i64),
            ),
            None => (
                "SELECT path, status, additions, deletions, oversize FROM session_file_history WHERE session_id = ?1 ORDER BY id ASC",
                None,
            ),
        };

        let mut stmt = conn.prepare(query)?;
        let mut changes = Vec::new();
        let mut rows = match has_turn {
            Some(t) => stmt.query(params![session_id, t])?,
            None => stmt.query(params![session_id])?,
        };

        while let Some(row) = rows.next()? {
            changes.push(crate::session::sqlite_store::FileHistoryChange {
                path: row.get(0)?,
                status: row.get(1)?,
                additions: row.get::<_, i64>(2)? as usize,
                deletions: row.get::<_, i64>(3)? as usize,
                binary: Some(false),
                oversize: Some(row.get::<_, i64>(4)? != 0),
            });
        }

        let recorded = !changes.is_empty();
        Ok((changes, recorded))
    }

    pub fn get_file_history_content(
        &self,
        session_id: &str,
        turn_id: usize,
        path: &str,
        phase: &str,
    ) -> Result<Option<crate::session::sqlite_store::FileHistoryContent>, rusqlite::Error> {
        let conn = self.conn.lock();
        let query = if phase == "start" {
            "SELECT content_before, id, oversize FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2 AND path = ?3 ORDER BY id ASC LIMIT 1"
        } else {
            "SELECT content_after, id, oversize FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2 AND path = ?3 ORDER BY id DESC LIMIT 1"
        };

        let mut stmt = conn.prepare(query)?;
        let mut rows = stmt.query(params![session_id, turn_id as i64, path])?;
        if let Some(row) = rows.next()? {
            let content: Option<String> = row.get(0)?;
            let version: i64 = row.get(1)?;
            let oversize: i64 = row.get(2)?;
            Ok(Some(crate::session::sqlite_store::FileHistoryContent {
                version: version as usize,
                content,
                binary: Some(false),
                oversize: (oversize != 0).then_some(true),
            }))
        } else {
            Ok(None)
        }
    }
}
