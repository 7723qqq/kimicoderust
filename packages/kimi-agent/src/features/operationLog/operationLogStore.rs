use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::operationLog::{
    OPERATION_LOG_DEFAULT_RETENTION, OPERATION_LOG_RECORD_VERSION, OperationActionInput,
    OperationKind, OperationLogError, OperationLogErrorCode, OperationLogErrorDetails,
    OperationLogErrorLimit, OperationLogHead, OperationLogRetention, OperationRecord,
    OperationRecordContent, OperationView, StoredOperationView,
};

const LOCK_STALE_MS: u64 = 30_000;
const LOCK_HEARTBEAT_MS: u64 = 10_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

static LOCAL_LOCKS: LazyLock<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn local_lock_for(root: &Path) -> Arc<Mutex<()>> {
    let mut locks = LOCAL_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[derive(Debug, Default, Clone)]
pub struct PartialRetention {
    pub max_operations: Option<u64>,
    pub max_age_ms: Option<u64>,
    pub max_total_bytes: Option<u64>,
}

#[derive(Debug, Clone)]
struct OperationDocument {
    raw: String,
    operation: OperationRecord,
}

pub struct OperationLogStore {
    root: PathBuf,
    operations_dir: PathBuf,
    views_dir: PathBuf,
    head_path: PathBuf,
    retention: OperationLogRetention,
    local_lock: Arc<Mutex<()>>,
}

impl OperationLogStore {
    pub fn new(
        root: impl AsRef<Path>,
        retention: PartialRetention,
    ) -> Result<Self, OperationLogError> {
        let resolved = resolve_path(root.as_ref());
        let merged = OperationLogRetention {
            max_operations: retention
                .max_operations
                .unwrap_or(OPERATION_LOG_DEFAULT_RETENTION.max_operations),
            max_age_ms: retention
                .max_age_ms
                .unwrap_or(OPERATION_LOG_DEFAULT_RETENTION.max_age_ms),
            max_total_bytes: retention
                .max_total_bytes
                .unwrap_or(OPERATION_LOG_DEFAULT_RETENTION.max_total_bytes),
        };
        if merged.max_operations < 1
            || merged.max_operations > MAX_SAFE_INTEGER
            || merged.max_total_bytes < 1
            || merged.max_total_bytes > MAX_SAFE_INTEGER
        {
            return Err(OperationLogError::new(
                OperationLogErrorCode::Invalid,
                "Invalid operation log retention configuration",
            ));
        }
        Ok(Self {
            operations_dir: resolved.join("operations"),
            views_dir: resolved.join("views"),
            head_path: resolved.join("head.json"),
            local_lock: local_lock_for(&resolved),
            root: resolved,
            retention: merged,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn capture(
        &self,
        view: OperationView,
        input: &OperationActionInput,
        before_view: Option<OperationView>,
    ) -> Result<OperationRecord, OperationLogError> {
        self.with_lock(|| async move {
            let head = self.read_head().await?;
            let parent = match head {
                Some(head) => Some(self.read_operation(&head.operation_id).await?),
                None => None,
            };
            self.append(AppendInput {
                kind: OperationKind::Capture,
                view,
                before_view,
                parent,
                input,
                target_id: None,
                next_head: Box::new(|operation| OperationLogHead {
                    operation_id: operation.id.clone(),
                    undo_target_id: Some(operation.id.clone()),
                    redo_stack: Vec::new(),
                }),
            })
            .await
        })
        .await
    }

    pub async fn undo(
        &self,
        input: &OperationActionInput,
    ) -> Result<OperationRecord, OperationLogError> {
        self.with_lock(|| async move {
            let head = self.require_head().await?;
            let undo_target_id = head.undo_target_id.clone().ok_or_else(|| {
                OperationLogError::new(
                    OperationLogErrorCode::NothingToUndo,
                    "No operation is available to undo",
                )
            })?;
            let parent = self.read_operation(&head.operation_id).await?;
            let target = self.read_operation(&undo_target_id).await?;
            let before_view_id = target.content.before_view_id.clone().ok_or_else(|| {
                OperationLogError::new(
                    OperationLogErrorCode::NothingToUndo,
                    "The first operation cannot be undone",
                )
            })?;
            let view = self.read_view(&before_view_id).await?;
            let next_undo_target_id = target.content.parents.first().cloned();
            let redo_stack = head.redo_stack.clone();
            let target_id = target.id.clone();
            self.append(AppendInput {
                kind: OperationKind::Undo,
                view,
                before_view: None,
                parent: Some(parent),
                input,
                target_id: Some(target_id.clone()),
                next_head: Box::new(move |operation| OperationLogHead {
                    operation_id: operation.id.clone(),
                    undo_target_id: next_undo_target_id.clone(),
                    redo_stack: {
                        let mut stack = redo_stack.clone();
                        stack.push(target_id.clone());
                        stack
                    },
                }),
            })
            .await
        })
        .await
    }

    pub async fn redo(
        &self,
        input: &OperationActionInput,
    ) -> Result<OperationRecord, OperationLogError> {
        self.with_lock(|| async move {
            let head = self.require_head().await?;
            let target_id = head.redo_stack.last().cloned().ok_or_else(|| {
                OperationLogError::new(
                    OperationLogErrorCode::NothingToRedo,
                    "No operation is available to redo",
                )
            })?;
            let parent = self.read_operation(&head.operation_id).await?;
            let target = self.read_operation(&target_id).await?;
            let view = self.read_view(&target.content.view_id).await?;
            let redo_stack = head.redo_stack.clone();
            self.append(AppendInput {
                kind: OperationKind::Redo,
                view,
                before_view: None,
                parent: Some(parent),
                input,
                target_id: Some(target_id),
                next_head: Box::new(move |operation| OperationLogHead {
                    operation_id: operation.id.clone(),
                    undo_target_id: Some(operation.id.clone()),
                    redo_stack: redo_stack[..redo_stack.len().saturating_sub(1)].to_vec(),
                }),
            })
            .await
        })
        .await
    }

    pub async fn restore(
        &self,
        target_id: &str,
        input: &OperationActionInput,
    ) -> Result<OperationRecord, OperationLogError> {
        self.restore_view(target_id, input, false).await
    }

    pub async fn restore_before(
        &self,
        target_id: &str,
        input: &OperationActionInput,
    ) -> Result<OperationRecord, OperationLogError> {
        self.restore_view(target_id, input, true).await
    }

    async fn restore_view(
        &self,
        target_id: &str,
        input: &OperationActionInput,
        before: bool,
    ) -> Result<OperationRecord, OperationLogError> {
        assert_id(target_id)?;
        self.with_lock(|| async move {
            let head = self.require_head().await?;
            let parent = self.read_operation(&head.operation_id).await?;
            let target = self.read_operation(target_id).await?;
            let view = if before {
                let before_view_id = target.content.before_view_id.clone().ok_or_else(|| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        "The operation has no before view",
                    )
                })?;
                self.read_view(&before_view_id).await?
            } else {
                self.read_view(&target.content.view_id).await?
            };
            self.append(AppendInput {
                kind: OperationKind::Restore,
                view,
                before_view: None,
                parent: Some(parent),
                input,
                target_id: Some(target_id.to_string()),
                next_head: Box::new(|operation| OperationLogHead {
                    operation_id: operation.id.clone(),
                    undo_target_id: Some(operation.id.clone()),
                    redo_stack: Vec::new(),
                }),
            })
            .await
        })
        .await
    }

    pub async fn head(&self) -> Result<Option<OperationLogHead>, OperationLogError> {
        self.with_lock(|| async move { self.read_head().await })
            .await
    }

    pub async fn get(&self, id: &str) -> Result<OperationRecord, OperationLogError> {
        assert_id(id)?;
        self.with_lock(|| async move { self.read_operation(id).await })
            .await
    }

    pub async fn view(&self, id: &str) -> Result<OperationView, OperationLogError> {
        assert_id(id)?;
        self.with_lock(|| async move { self.read_view(id).await })
            .await
    }

    pub async fn views(&self, ids: &[String]) -> Result<Vec<OperationView>, OperationLogError> {
        for id in ids {
            assert_id(id)?;
        }
        self.with_lock(|| async move {
            let mut views = Vec::with_capacity(ids.len());
            for id in ids {
                views.push(self.read_view(id).await?);
            }
            Ok(views)
        })
        .await
    }

    pub async fn list(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<OperationRecord>, OperationLogError> {
        self.with_lock(|| async move {
            let head = match self.read_head().await? {
                Some(head) => head,
                None => return Ok(Vec::new()),
            };
            let mut operations = Vec::new();
            let mut cursor = Some(head.operation_id);
            let mut seen = std::collections::HashSet::new();
            while let Some(id) = cursor {
                if seen.contains(&id) {
                    break;
                }
                if let Some(limit) = limit
                    && operations.len() >= limit
                {
                    break;
                }
                seen.insert(id.clone());
                let operation = self.read_operation(&id).await?;
                cursor = operation.content.parents.first().cloned();
                operations.push(operation);
            }
            Ok(operations)
        })
        .await
    }

    async fn append(&self, input: AppendInput<'_>) -> Result<OperationRecord, OperationLogError> {
        let stored_view = StoredOperationView::new(input.view);
        let view_raw = encode(&stored_view)?;
        let view_id = sha256(&view_raw);

        let mut before_view_raw: Option<String> = None;
        let mut before_view_id: Option<String> = None;
        if let Some(before_view) = input.before_view {
            let stored_before = StoredOperationView::new(before_view);
            let raw = encode(&stored_before)?;
            let id = sha256(&raw);
            if let Some(parent) = input.parent.as_ref()
                && parent.content.view_id != id
            {
                return Err(OperationLogError::new(
                    OperationLogErrorCode::ViewMismatch,
                    "The capture before view differs from the operation head view",
                ));
            }
            before_view_raw = Some(raw);
            before_view_id = Some(id);
        }

        let effective_before_view_id = input
            .parent
            .as_ref()
            .map(|parent| parent.content.view_id.clone())
            .or_else(|| before_view_id.clone());

        let content = OperationRecordContent {
            version: OPERATION_LOG_RECORD_VERSION,
            parents: input
                .parent
                .as_ref()
                .map(|parent| vec![parent.id.clone()])
                .unwrap_or_default(),
            before_view_id: effective_before_view_id,
            view_id,
            kind: input.kind,
            target_id: input.target_id.clone(),
            at: input.input.at.clone(),
            description: input.input.description.clone(),
            nonce: new_nonce(),
            origin: input.input.origin.clone(),
        };
        let content_raw = encode(&content)?;
        let id = sha256(&content_raw);
        let record = OperationRecord {
            id: id.clone(),
            content,
        };

        self.assert_retention(&record, &content_raw, &view_raw, before_view_raw.as_deref())
            .await?;

        if let (Some(raw), Some(id)) = (before_view_raw.as_deref(), before_view_id.as_deref()) {
            self.write_immutable(&self.view_path(id)?, raw).await?;
        }
        self.write_immutable(&self.view_path(&record.content.view_id)?, &view_raw)
            .await?;
        self.write_immutable(&self.operation_path(&id)?, &encode(&record)?)
            .await?;
        let head = (input.next_head)(&record);
        self.write_atomic(&self.head_path, &encode(&head)?).await?;
        Ok(record)
    }

    async fn assert_retention(
        &self,
        record: &OperationRecord,
        operation_raw: &str,
        view_raw: &str,
        before_view_raw: Option<&str>,
    ) -> Result<(), OperationLogError> {
        let documents = self.operation_documents().await?;
        if documents.len() as u64 >= self.retention.max_operations {
            return Err(OperationLogError::with_details(
                OperationLogErrorCode::RetentionExceeded,
                format!(
                    "Operation count limit reached ({})",
                    self.retention.max_operations
                ),
                OperationLogErrorDetails {
                    limit: Some(OperationLogErrorLimit::Operations),
                    maximum: Some(self.retention.max_operations),
                    operation: None,
                },
            ));
        }
        let oldest_allowed = chrono::DateTime::parse_from_rfc3339(&record.content.at)
            .ok()
            .map(|at| at.timestamp_millis() - self.retention.max_age_ms as i64);
        let too_old = oldest_allowed.is_some_and(|oldest_allowed| {
            documents.iter().any(|document| {
                chrono::DateTime::parse_from_rfc3339(&document.operation.content.at)
                    .map(|at| at.timestamp_millis() < oldest_allowed)
                    .unwrap_or(false)
            })
        });
        if too_old {
            return Err(OperationLogError::with_details(
                OperationLogErrorCode::RetentionExceeded,
                format!(
                    "Operation age limit reached ({}ms)",
                    self.retention.max_age_ms
                ),
                OperationLogErrorDetails {
                    limit: Some(OperationLogErrorLimit::Age),
                    maximum: Some(self.retention.max_age_ms),
                    operation: None,
                },
            ));
        }
        let mut view_ids: std::collections::HashSet<String> = documents
            .iter()
            .flat_map(|document| {
                let mut ids = vec![document.operation.content.view_id.clone()];
                if let Some(before) = document.operation.content.before_view_id.as_ref() {
                    ids.push(before.clone());
                }
                ids
            })
            .collect();
        let mut total_bytes: u64 = documents
            .iter()
            .map(|document| document.raw.len() as u64)
            .sum();
        for id in &view_ids {
            total_bytes += self.file_size(&self.view_path(id)?).await?;
        }
        total_bytes += operation_raw.len() as u64;
        if !view_ids.contains(&record.content.view_id) {
            total_bytes += view_raw.len() as u64;
            view_ids.insert(record.content.view_id.clone());
        }
        if let (Some(raw), Some(id)) = (before_view_raw, record.content.before_view_id.as_ref())
            && !view_ids.contains(id)
        {
            total_bytes += raw.len() as u64;
        }
        if total_bytes > self.retention.max_total_bytes {
            return Err(OperationLogError::with_details(
                OperationLogErrorCode::RetentionExceeded,
                format!(
                    "Operation byte limit reached ({})",
                    self.retention.max_total_bytes
                ),
                OperationLogErrorDetails {
                    limit: Some(OperationLogErrorLimit::Bytes),
                    maximum: Some(self.retention.max_total_bytes),
                    operation: None,
                },
            ));
        }
        Ok(())
    }

    async fn operation_documents(&self) -> Result<Vec<OperationDocument>, OperationLogError> {
        let mut documents = Vec::new();
        for bucket in readdir_safe(&self.operations_dir).await? {
            let dir = self.operations_dir.join(bucket);
            for name in readdir_safe(&dir).await? {
                if !name.ends_with(".json") {
                    continue;
                }
                let path = dir.join(&name);
                let raw = read_raw(&path).await?.ok_or_else(|| {
                    OperationLogError::new(
                        OperationLogErrorCode::Corrupt,
                        format!("Operation object vanished: {}", path.display()),
                    )
                })?;
                let operation = parse_operation(&raw, &name[..name.len() - ".json".len()])?;
                documents.push(OperationDocument { raw, operation });
            }
        }
        Ok(documents)
    }

    async fn require_head(&self) -> Result<OperationLogHead, OperationLogError> {
        match self.read_head().await? {
            Some(head) => Ok(head),
            None => Err(OperationLogError::new(
                OperationLogErrorCode::NotFound,
                "Operation log is empty",
            )),
        }
    }

    async fn read_head(&self) -> Result<Option<OperationLogHead>, OperationLogError> {
        let raw = match read_raw(&self.head_path).await? {
            Some(raw) => raw,
            None => return Ok(None),
        };
        let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
            OperationLogError::new(
                OperationLogErrorCode::Corrupt,
                "Operation head is not valid JSON",
            )
        })?;
        if !is_operation_head(&parsed) {
            return Err(OperationLogError::new(
                OperationLogErrorCode::Corrupt,
                "Operation head is invalid",
            ));
        }
        serde_json::from_value(parsed)
            .map(Some)
            .map_err(|_| OperationLogError::new(OperationLogErrorCode::Corrupt, "Operation head is invalid"))
    }

    async fn read_operation(&self, id: &str) -> Result<OperationRecord, OperationLogError> {
        let raw = read_raw(&self.operation_path(id)?).await?.ok_or_else(|| {
            OperationLogError::new(
                OperationLogErrorCode::NotFound,
                format!("Operation not found: {id}"),
            )
        })?;
        parse_operation(&raw, id)
    }

    async fn read_view(&self, id: &str) -> Result<OperationView, OperationLogError> {
        let raw = read_raw(&self.view_path(id)?).await?.ok_or_else(|| {
            OperationLogError::new(
                OperationLogErrorCode::NotFound,
                format!("Operation view not found: {id}"),
            )
        })?;
        let parsed: serde_json::Value = serde_json::from_str(&raw).map_err(|_| {
            OperationLogError::new(
                OperationLogErrorCode::Corrupt,
                format!("Operation view is not valid JSON: {id}"),
            )
        })?;
        if !is_stored_operation_view(&parsed) || sha256(&raw) != id {
            return Err(OperationLogError::new(
                OperationLogErrorCode::Corrupt,
                format!("Operation view content does not match its id: {id}"),
            ));
        }
        let stored: StoredOperationView = serde_json::from_value(parsed).map_err(|_| {
            OperationLogError::new(
                OperationLogErrorCode::Corrupt,
                format!("Operation view content does not match its id: {id}"),
            )
        })?;
        Ok(stored.view)
    }

    fn operation_path(&self, id: &str) -> Result<PathBuf, OperationLogError> {
        assert_id(id)?;
        Ok(self
            .operations_dir
            .join(&id[..2])
            .join(format!("{id}.json")))
    }

    fn view_path(&self, id: &str) -> Result<PathBuf, OperationLogError> {
        assert_id(id)?;
        Ok(self.views_dir.join(&id[..2]).join(format!("{id}.json")))
    }

    async fn file_size(&self, path: &Path) -> Result<u64, OperationLogError> {
        fs::metadata(path).await.map(|info| info.len()).map_err(|error| {
            OperationLogError::new(
                OperationLogErrorCode::Corrupt,
                format!("cannot stat {}: {error}", path.display()),
            )
        })
    }

    async fn write_immutable(&self, path: &Path, raw: &str) -> Result<(), OperationLogError> {
        if let Some(existing) = read_raw(path).await? {
            if existing != raw {
                return Err(OperationLogError::new(
                    OperationLogErrorCode::Corrupt,
                    format!("Immutable object differs: {}", path.display()),
                ));
            }
            return Ok(());
        }
        write_atomic(path, raw).await
    }

    async fn write_atomic(&self, path: &Path, raw: &str) -> Result<(), OperationLogError> {
        write_atomic(path, raw).await
    }

    async fn with_lock<T, F, Fut>(&self, operation: F) -> Result<T, OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, OperationLogError>>,
    {
        let _local = self.local_lock.clone().lock_owned().await;
        self.with_file_lock(operation).await
    }

    async fn with_file_lock<T, F, Fut>(&self, operation: F) -> Result<T, OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, OperationLogError>>,
    {
        create_directory(&self.root).await.map_err(|error| {
            OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot create the operation log root: {error}"),
            )
        })?;
        let lock_path = self.root.join(".lock");
        let token = new_nonce();
        match create_new_options().open(&lock_path).await {
            Ok(mut file) => {
                if let Err(error) = file.write_all(token.as_bytes()).await {
                    let _ = fs::remove_file(&lock_path).await;
                    return Err(OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot write the operation log lock: {error}"),
                    ));
                }
            }
            Err(error) => {
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) {
                    return Err(OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot create the operation log lock: {error}"),
                    ));
                }
                let age = fs::metadata(&lock_path)
                    .await
                    .ok()
                    .and_then(|info| info.modified().ok())
                    .and_then(|modified| modified.elapsed().ok())
                    .map(|elapsed| elapsed.as_millis() as u64)
                    .unwrap_or(0);
                if age <= LOCK_STALE_MS {
                    return Err(OperationLogError::new(
                        OperationLogErrorCode::Busy,
                        "Operation log is locked by another process",
                    ));
                }
                fs::remove_file(&lock_path).await.map_err(|error| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot replace the operation log lock: {error}"),
                    )
                })?;
                let mut file = create_new_options().open(&lock_path).await.map_err(|error| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot create the operation log lock: {error}"),
                    )
                })?;
                file.write_all(token.as_bytes()).await.map_err(|error| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot write the operation log lock: {error}"),
                    )
                })?;
            }
        }

        let heartbeat_path = lock_path.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(LOCK_HEARTBEAT_MS));
            interval.tick().await;
            loop {
                interval.tick().await;
                let path = heartbeat_path.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                        let _ = file.set_modified(std::time::SystemTime::now());
                    }
                })
                .await;
            }
        });

        let result = operation().await;
        heartbeat.abort();
        let owner = read_raw(&lock_path).await.ok().flatten();
        if owner.as_deref() == Some(token.as_str()) {
            let _ = fs::remove_file(&lock_path).await;
        }
        result
    }
}

struct AppendInput<'a> {
    kind: OperationKind,
    view: OperationView,
    before_view: Option<OperationView>,
    parent: Option<OperationRecord>,
    input: &'a OperationActionInput,
    target_id: Option<String>,
    next_head: Box<dyn Fn(&OperationRecord) -> OperationLogHead + Send + Sync>,
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

fn new_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn sha256(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn encode<T: Serialize>(value: &T) -> Result<String, OperationLogError> {
    serde_json::to_string(value)
        .map(|raw| format!("{raw}\n"))
        .map_err(|error| {
            OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot encode the operation object: {error}"),
            )
        })
}

fn assert_id(id: &str) -> Result<(), OperationLogError> {
    if id.len() != 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(OperationLogError::new(
            OperationLogErrorCode::Invalid,
            "Invalid operation object id",
        ));
    }
    Ok(())
}

fn is_operation_head(value: &serde_json::Value) -> bool {
    let Some(candidate) = value.as_object() else {
        return false;
    };
    if !candidate
        .get("operationId")
        .is_some_and(serde_json::Value::is_string)
    {
        return false;
    }
    if candidate
        .get("undoTargetId")
        .is_some_and(|value| !value.is_string())
    {
        return false;
    }
    candidate
        .get("redoStack")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|stack| stack.iter().all(serde_json::Value::is_string))
}

fn is_stored_operation_view(value: &serde_json::Value) -> bool {
    let Some(candidate) = value.as_object() else {
        return false;
    };
    if candidate.get("version").and_then(serde_json::Value::as_u64)
        != Some(OPERATION_LOG_RECORD_VERSION as u64)
    {
        return false;
    }
    if !candidate
        .get("tree")
        .is_some_and(serde_json::Value::is_string)
        || !candidate
            .get("indexTree")
            .is_some_and(serde_json::Value::is_string)
    {
        return false;
    }
    let Some(refs) = candidate.get("refs").and_then(serde_json::Value::as_object) else {
        return false;
    };
    if !refs.values().all(serde_json::Value::is_string) {
        return false;
    }
    let Some(head) = candidate.get("head").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let valid_head = match head.get("kind").and_then(|kind| kind.as_str()) {
        Some("detached") => head.get("target").is_some_and(serde_json::Value::is_string),
        Some("unborn") => head.get("ref").is_some_and(serde_json::Value::is_string),
        Some("branch") => {
            head.get("ref").is_some_and(serde_json::Value::is_string)
                && head.get("target").is_some_and(serde_json::Value::is_string)
        }
        _ => false,
    };
    valid_head
        && candidate
            .get("unfinishedOperation")
            .map(|value| {
                value
                    .as_str()
                    .is_some_and(|value| value == "merge" || value == "rebase")
            })
            .unwrap_or(true)
}

fn parse_operation(raw: &str, id: &str) -> Result<OperationRecord, OperationLogError> {
    let parsed: serde_json::Value = serde_json::from_str(raw).map_err(|_| {
        OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!("Operation is not valid JSON: {id}"),
        )
    })?;
    let record: OperationRecord = serde_json::from_value(parsed).map_err(|_| {
        OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!("Operation content does not match its id: {id}"),
        )
    })?;
    if record.id != id {
        return Err(OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!("Operation content does not match its id: {id}"),
        ));
    }
    if sha256(&encode(&record.content)?) != id {
        return Err(OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!("Operation content does not match its id: {id}"),
        ));
    }
    Ok(record)
}

async fn read_raw(path: &Path) -> Result<Option<String>, OperationLogError> {
    match fs::read_to_string(path).await {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!("cannot read {}: {error}", path.display()),
        )),
    }
}

async fn readdir_safe(path: &Path) -> Result<Vec<String>, OperationLogError> {
    match fs::read_dir(path).await {
        Ok(mut entries) => {
            let mut names = Vec::new();
            while let Some(entry) = entries.next_entry().await.map_err(|error| {
                OperationLogError::new(
                    OperationLogErrorCode::Corrupt,
                    format!("cannot list {}: {error}", path.display()),
                )
            })? {
                if let Some(name) = entry.file_name().to_str() {
                    names.push(name.to_string());
                }
            }
            Ok(names)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!("cannot list {}: {error}", path.display()),
        )),
    }
}

async fn create_directory(path: &Path) -> Result<(), std::io::Error> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).await
}

fn create_new_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

async fn write_atomic(path: &Path, raw: &str) -> Result<(), OperationLogError> {
    if let Some(parent) = path.parent() {
        create_directory(parent).await.map_err(|error| {
            OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot create {}: {error}", parent.display()),
            )
        })?;
    }
    let temp = PathBuf::from(format!("{}.{}.tmp", path.display(), uuid::Uuid::new_v4()));
    let write = async {
        let mut file = create_new_options().open(&temp).await?;
        file.write_all(raw.as_bytes()).await?;
        file.flush().await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    match write {
        Ok(()) => fs::rename(&temp, path).await.map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot commit {}: {error}", path.display()),
            )
        }),
        Err(error) => {
            let _ = std::fs::remove_file(&temp).await;
            Err(OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot write {}: {error}", temp.display()),
            ))
        }
    }
}
