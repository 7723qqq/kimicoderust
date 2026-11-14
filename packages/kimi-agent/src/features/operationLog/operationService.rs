use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex as StdMutex};

use super::configSection::resolve_operation_log_retention;
use super::flag::{OPERATION_LOG_FLAG_ENV, OPERATION_LOG_FLAG_ID};
use super::operationLog::{
    OperationActionInput, OperationKind, OperationLogError, OperationLogErrorCode,
    OperationLogHead, OperationOrigin, OperationRecord, OperationView,
};
use super::operationLogStore::{OperationLogStore, PartialRetention};
use super::repositoryViewService::{RepositoryViewService, RepositoryViewServiceOptions};

pub const AGENT_OPERATION_SERVICE_ID: &str = "agentOperationService";

static EXECUTION_LOCKS: LazyLock<StdMutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn execution_lock_for(repo_root: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = EXECUTION_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(repo_root.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoMutation {
    Possible,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureOrigin {
    pub session_id: String,
    pub agent_id: String,
    pub turn_id: Option<u64>,
    pub tool_call_id: Option<String>,
    pub tool_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureContext {
    pub description: String,
    pub origin: CaptureOrigin,
    pub repo_mutation: Option<RepoMutation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreKind {
    Undo,
    Redo,
    Restore,
}

impl RestoreKind {
    fn description(self) -> &'static str {
        match self {
            Self::Undo => "Undo operation",
            Self::Redo => "Redo operation",
            Self::Restore => "Restore operation",
        }
    }
}

#[derive(Clone)]
pub struct AgentOperationService {
    repo_root: String,
    session_id: String,
    agent_id: String,
    retention: Option<PartialRetention>,
    flag_enabled: Option<bool>,
    repository_views: Arc<StdMutex<HashMap<String, Arc<RepositoryViewService>>>>,
}

impl AgentOperationService {
    pub fn new(
        repo_root: impl Into<String>,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            retention: None,
            flag_enabled: None,
            repository_views: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    pub(crate) fn with_identity(
        mut self,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        self.session_id = session_id.into();
        self.agent_id = agent_id.into();
        self
    }

    pub fn with_retention(mut self, retention: PartialRetention) -> Self {
        self.retention = Some(retention);
        self
    }

    pub fn with_flag_enabled(mut self, enabled: bool) -> Self {
        self.flag_enabled = Some(enabled);
        self
    }

    pub async fn list_operations(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<OperationRecord>, OperationLogError> {
        let repo_root = self.local_repo_root()?;
        self.with_execution_lock(repo_root, || async move {
            let (_backend, store) = self.open_operation_store().await?;
            store.list(limit).await
        })
        .await
    }

    pub async fn undo_operation(&self) -> Result<OperationRecord, OperationLogError> {
        self.restore(RestoreKind::Undo, None).await
    }

    pub async fn redo_operation(&self) -> Result<OperationRecord, OperationLogError> {
        self.restore(RestoreKind::Redo, None).await
    }

    pub async fn restore_operation(
        &self,
        target_id: &str,
    ) -> Result<OperationRecord, OperationLogError> {
        self.restore(RestoreKind::Restore, Some(target_id)).await
    }

    pub async fn restore_before_turn(
        &self,
        turn_id: u64,
    ) -> Result<Option<OperationRecord>, OperationLogError> {
        let repo_root = self.local_repo_root()?;
        self.with_execution_lock(repo_root, || async move {
            let (backend, store) = self.open_operation_store().await?;
            let operations = store.list(None).await?;
            let target = operations.iter().rev().find(|operation| {
                operation.content.kind == OperationKind::Capture
                    && operation.content.origin.as_ref().is_some_and(|origin| {
                        origin.session_id.as_deref() == Some(self.session_id.as_str())
                            && origin.agent_id.as_deref() == Some(self.agent_id.as_str())
                            && origin.turn_id.is_some_and(|candidate| candidate >= turn_id)
                    })
            });
            let Some(target) = target.cloned() else {
                return Ok(None);
            };
            let Some(before_view_id) = target.content.before_view_id.clone() else {
                return Err(OperationLogError::new(
                    OperationLogErrorCode::Invalid,
                    "The operation has no before view",
                ));
            };
            let current = backend.capture_view().await?;
            let (_head, expected) = self.align_head_to_view(&store, &current).await?;
            let target_view = store.view(&before_view_id).await?;
            let input = OperationActionInput {
                at: now_rfc3339(),
                description: format!("Restore before turn {turn_id}"),
                origin: Some(self.capture_origin()),
            };
            backend.restore_view(&expected, &target_view).await?;
            match store.restore_before(&target.id, &input).await {
                Ok(record) => Ok(Some(record)),
                Err(error) => {
                    backend.restore_view(&target_view, &expected).await?;
                    Err(error)
                }
            }
        })
        .await
    }

    pub async fn capture<F, Fut>(
        &self,
        context: &CaptureContext,
        next: F,
    ) -> Result<(), OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), OperationLogError>>,
    {
        if !self.flag_enabled.unwrap_or_else(operation_log_enabled)
            || context.repo_mutation == Some(RepoMutation::None)
        {
            return next().await;
        }
        let repo_root = self.local_repo_root()?;
        self.with_execution_lock(repo_root, || async move {
            self.capture_locked(context, next).await
        })
        .await
    }

    async fn capture_locked<F, Fut>(
        &self,
        context: &CaptureContext,
        next: F,
    ) -> Result<(), OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), OperationLogError>>,
    {
        let (backend, store) = self.open_operation_store().await?;
        let before = backend.capture_view().await?;
        if let Some(head) = store.head().await? {
            let operation = store.get(&head.operation_id).await?;
            let expected = store.view(&operation.content.view_id).await?;
            if !before.equals(&expected) {
                self.import_external_changes(&store, &before, &expected)
                    .await?;
            }
        }
        let result = next().await;
        let after = backend.capture_view().await?;
        if !before.equals(&after) {
            let input = OperationActionInput {
                at: now_rfc3339(),
                description: context.description.clone(),
                origin: Some(OperationOrigin {
                    session_id: Some(context.origin.session_id.clone()),
                    agent_id: Some(context.origin.agent_id.clone()),
                    turn_id: context.origin.turn_id,
                    tool_call_id: context.origin.tool_call_id.clone(),
                    tool_name: Some(context.origin.tool_name.clone()),
                }),
            };
            store.capture(after, &input, Some(before)).await?;
        }
        result
    }

    async fn restore(
        &self,
        kind: RestoreKind,
        target_id: Option<&str>,
    ) -> Result<OperationRecord, OperationLogError> {
        let repo_root = self.local_repo_root()?;
        let target_id = target_id.map(str::to_string);
        self.with_execution_lock(repo_root, || async move {
            self.restore_locked(kind, target_id.as_deref()).await
        })
        .await
    }

    async fn restore_locked(
        &self,
        kind: RestoreKind,
        target_id: Option<&str>,
    ) -> Result<OperationRecord, OperationLogError> {
        let (backend, store) = self.open_operation_store().await?;
        let current = backend.capture_view().await?;
        let (head, expected) = self.align_head_to_view(&store, &current).await?;
        let input = OperationActionInput {
            at: now_rfc3339(),
            description: kind.description().to_string(),
            origin: Some(self.capture_origin()),
        };
        enum Target {
            Undo(String),
            Redo(String),
            Restore(String),
        }
        let target = match kind {
            RestoreKind::Undo => {
                let undo_target_id = head.undo_target_id.clone().ok_or_else(|| {
                    OperationLogError::new(
                        OperationLogErrorCode::NothingToUndo,
                        "No operation is available to undo",
                    )
                })?;
                let target = store.get(&undo_target_id).await?;
                let Some(before_view_id) = target.content.before_view_id.clone() else {
                    return Err(OperationLogError::new(
                        OperationLogErrorCode::NothingToUndo,
                        "The first operation cannot be undone",
                    ));
                };
                Target::Undo(before_view_id)
            }
            RestoreKind::Redo => {
                let redo_target_id = head.redo_stack.last().cloned().ok_or_else(|| {
                    OperationLogError::new(
                        OperationLogErrorCode::NothingToRedo,
                        "No operation is available to redo",
                    )
                })?;
                Target::Redo(redo_target_id)
            }
            RestoreKind::Restore => {
                let target_id = target_id.ok_or_else(|| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        "A restore target is required",
                    )
                })?;
                Target::Restore(target_id.to_string())
            }
        };
        let target_view = match &target {
            Target::Undo(view_id) => store.view(view_id).await?,
            Target::Redo(target_id) => {
                let target = store.get(target_id).await?;
                store.view(&target.content.view_id).await?
            }
            Target::Restore(target_id) => {
                let target = store.get(target_id).await?;
                store.view(&target.content.view_id).await?
            }
        };
        backend.restore_view(&expected, &target_view).await?;
        let append = match target {
            Target::Undo(_) => store.undo(&input).await,
            Target::Redo(_) => store.redo(&input).await,
            Target::Restore(target_id) => store.restore(&target_id, &input).await,
        };
        match append {
            Ok(record) => Ok(record),
            Err(error) => {
                backend.restore_view(&target_view, &expected).await?;
                Err(error)
            }
        }
    }

    async fn align_head_to_view(
        &self,
        store: &OperationLogStore,
        current: &OperationView,
    ) -> Result<(OperationLogHead, OperationView), OperationLogError> {
        let head = store.head().await?.ok_or_else(|| {
            OperationLogError::new(OperationLogErrorCode::NotFound, "Operation log is empty")
        })?;
        let parent = store.get(&head.operation_id).await?;
        let expected = store.view(&parent.content.view_id).await?;
        if current.equals(&expected) {
            return Ok((head, expected));
        }
        let imported = self
            .import_external_changes(store, current, &expected)
            .await?;
        Ok((
            OperationLogHead {
                operation_id: imported.id.clone(),
                undo_target_id: Some(imported.id.clone()),
                redo_stack: Vec::new(),
            },
            current.clone(),
        ))
    }

    async fn import_external_changes(
        &self,
        store: &OperationLogStore,
        current: &OperationView,
        expected: &OperationView,
    ) -> Result<OperationRecord, OperationLogError> {
        let input = OperationActionInput {
            at: now_rfc3339(),
            description: "Import external repository changes".to_string(),
            origin: Some(self.capture_origin()),
        };
        store
            .capture(current.clone(), &input, Some(expected.clone()))
            .await
    }

    fn capture_origin(&self) -> OperationOrigin {
        OperationOrigin {
            session_id: Some(self.session_id.clone()),
            agent_id: Some(self.agent_id.clone()),
            turn_id: None,
            tool_call_id: None,
            tool_name: None,
        }
    }

    fn local_repo_root(&self) -> Result<&str, OperationLogError> {
        if !self.flag_enabled.unwrap_or_else(operation_log_enabled) {
            return Err(OperationLogError::new(
                OperationLogErrorCode::Invalid,
                "Operation Log is disabled",
            ));
        }
        Ok(&self.repo_root)
    }

    async fn with_execution_lock<T, F, Fut>(&self, repo_root: &str, run: F) -> Result<T, OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, OperationLogError>>,
    {
        let lock = execution_lock_for(repo_root);
        let _guard = lock.lock_owned().await;
        run().await
    }

    async fn open_operation_store(
        &self,
    ) -> Result<(Arc<RepositoryViewService>, OperationLogStore), OperationLogError> {
        let backend = self.repository_view().await?;
        let retention = self.retention.clone().unwrap_or_else(configured_retention);
        let store = OperationLogStore::new(backend.storage_root(), retention)?;
        Ok((backend, store))
    }

    async fn repository_view(&self) -> Result<Arc<RepositoryViewService>, OperationLogError> {
        let repo_root = self.repo_root.clone();
        let cached = self
            .repository_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&repo_root)
            .cloned();
        if let Some(cached) = cached
            && cached.is_repository_current().await
        {
            return Ok(cached);
        }
        let backend = Arc::new(
            RepositoryViewService::open(&repo_root, RepositoryViewServiceOptions::default())
                .await?,
        );
        self.repository_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(repo_root, backend.clone());
        Ok(backend)
    }

    pub async fn dispose(&self) {
        let views = self
            .repository_views
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, view)| view)
            .collect::<Vec<_>>();
        for view in views {
            view.dispose().await;
        }
    }
}

pub fn operation_log_enabled() -> bool {
    if let Some(enabled) =
        crate::env::parse_bool_env(std::env::var(OPERATION_LOG_FLAG_ENV).ok().as_deref())
    {
        return enabled;
    }
    if let Ok((config, _)) = crate::config::KimiConfig::discover() {
        for key in [OPERATION_LOG_FLAG_ID, "operation-log"] {
            if let Some(crate::config::ExperimentalValue::Bool(enabled)) =
                config.experimental.get(key)
            {
                return *enabled;
            }
        }
    }
    crate::env::parse_bool_env(std::env::var("KIMI_CODE_EXPERIMENTAL_FLAG").ok().as_deref())
        == Some(true)
}

fn configured_retention() -> PartialRetention {
    let retention = crate::config::KimiConfig::discover()
        .map(|(config, _)| resolve_operation_log_retention(&config))
        .unwrap_or_default();
    PartialRetention {
        max_operations: retention.max_operations,
        max_age_ms: retention.max_age_ms,
        max_total_bytes: retention.max_total_bytes,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
