use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::{Arc, LazyLock};

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::log::ILogService;
use crate::app::session_index::session_index::ISessionIndexMirror;
use crate::app::session_index::session_index_source::{SessionSummaryInput, build_session_summary};
use crate::environment::environment::{EnvironmentBinding, LOCAL_ENVIRONMENT_ID};
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::state::session_state::{ISessionStateService, StateDefinition};
use crate::session::workspace_context::workspace_roots::resolve_effective_workspace_roots;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::state::state::define_state;
use crate::wire::record::WireAsyncError;

use super::session_metadata::*;
use super::prompt_metadata::is_untitled;

const META_KEY: &str = "state.json";

static PENDING_WRITES: LazyLock<Mutex<HashMap<usize, Shared<BoxFuture<'static, ()>>>>> = LazyLock::new(|| {
    Mutex::new(HashMap::new())
});
static NEXT_WRITE_ID: LazyLock<Mutex<usize>> = LazyLock::new(|| Mutex::new(0));

pub async fn drain_session_metadata_writes() -> Result<(), WireAsyncError> {
    let writes = PENDING_WRITES.lock().values().cloned().collect::<Vec<_>>();
    let _ = futures_util::future::join_all(writes).await;
    Ok(())
}

pub static SESSION_METADATA_DATA_KEY: LazyLock<StateDefinition<Option<SessionMeta>>> =
    LazyLock::new(|| define_state("sessionMetadata.data", Arc::new(|| None)));

#[derive(Debug, Clone)]
struct SharedMetadataError(Arc<String>);

impl SharedMetadataError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedMetadataError {}

pub struct SessionMetadata {
    states: Arc<dyn ISessionStateService>,
    ctx: Arc<dyn ISessionContext>,
    store: Arc<dyn IAtomicDocumentStore>,
    log: Arc<dyn ILogService>,
    mirror: Arc<dyn ISessionIndexMirror>,
    session_locks: Arc<dyn ISessionLockService>,
    workspace_info: Arc<dyn ISessionWorkspaceInfo>,
    scope: String,
    update_queue: Arc<Mutex<Option<Shared<BoxFuture<'static, ()>>>>>,
    refresh_before_write: Arc<Mutex<bool>>,
    disposed: std::sync::atomic::AtomicBool,
    ready: Arc<Mutex<Option<Shared<BoxFuture<'static, Result<(), SharedMetadataError>>>>>>,
    listeners: Arc<Mutex<HashMap<u64, Arc<dyn Fn(SessionMetadataChangedEvent) + Send + Sync>>>>,
    next_listener_id: Arc<Mutex<u64>>,
    _subscriptions: Mutex<Vec<Box<dyn IDisposable>>>,
}

impl SessionMetadata {
    pub fn new(
        states: Arc<dyn ISessionStateService>,
        ctx: Arc<dyn ISessionContext>,
        store: Arc<dyn IAtomicDocumentStore>,
        log: Arc<dyn ILogService>,
        mirror: Arc<dyn ISessionIndexMirror>,
        session_locks: Arc<dyn ISessionLockService>,
        workspace_info: Arc<dyn ISessionWorkspaceInfo>,
    ) -> Self {
        states.contribute_state(&SESSION_METADATA_DATA_KEY);
        let scope = ctx.meta_scope();
        let this = Self {
            states,
            ctx: ctx.clone(),
            store,
            log,
            mirror,
            session_locks: session_locks.clone(),
            workspace_info: workspace_info.clone(),
            scope: scope.clone(),
            update_queue: Arc::new(Mutex::new(None)),
            refresh_before_write: Arc::new(Mutex::new(true)),
            disposed: std::sync::atomic::AtomicBool::new(false),
            ready: Arc::new(Mutex::new(None)),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            next_listener_id: Arc::new(Mutex::new(0)),
            _subscriptions: Mutex::new(Vec::new()),
        };
        let refresh = this.refresh_before_write.clone();
        let session_id = ctx.session_id();
        this._subscriptions.lock().push(session_locks.on_did_hold(Arc::new(move |event| {
            if event.session_id == session_id {
                *refresh.lock() = true;
            }
        })));
        let weak_states = this.states.clone();
        let weak_scope = this.scope.clone();
        let weak_ctx = this.ctx.clone();
        let weak_mirror = this.mirror.clone();
        let weak_workspace = this.workspace_info.clone();
        let weak_log = this.log.clone();
        this._subscriptions.lock().push(workspace_info.on_did_change(Arc::new(move || {
            if weak_states.get(&SESSION_METADATA_DATA_KEY).is_some() {
                mirror_to_read_model(&weak_states, &weak_ctx, &weak_mirror, &weak_workspace, &weak_scope, &weak_log);
            }
        })));
        let ready = this.load(scope);
        *this.ready_lock() = Some(ready);
        this
    }

    fn ready_lock(&self) -> Arc<Mutex<Option<Shared<BoxFuture<'static, Result<(), SharedMetadataError>>>>>> {
        self.ready.clone()
    }

    fn data(&self) -> SessionMeta {
        self.states.get(&SESSION_METADATA_DATA_KEY).expect("session metadata is not loaded")
    }

    fn set_data(&self, value: SessionMeta) {
        self.states.set(&SESSION_METADATA_DATA_KEY, Some(value));
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let ready = self.ready.lock().clone();
        async move {
            if let Some(ready) = ready {
                ready.await.map_err(SharedMetadataError::boxed)
            } else {
                Ok(())
            }
        }
        .boxed()
    }

    pub async fn read(&self) -> Result<SessionMeta, WireAsyncError> {
        self.ready().await?;
        Ok(self.data())
    }

    pub async fn update(&self, patch: SessionMetaPatch, touch_updated_at: Option<bool>) -> Result<(), WireAsyncError> {
        self.enqueue_update(move || async move {
            if !(self.prepare_write().await?) {
                return Ok(());
            }
            self.apply_update(&patch, touch_updated_at).await
        }).await
    }

    async fn prepare_write(&self) -> Result<bool, WireAsyncError> {
        self.ready().await?;
        if self.disposed.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(false);
        }
        self.session_locks.hold(&self.ctx.session_id());
        if *self.refresh_before_write.lock() {
            *self.refresh_before_write.lock() = false;
            if let Some(existing) = self.store.get(&self.scope, META_KEY).await? {
                self.set_data(normalize_session_meta(existing, &self.ctx.session_id()));
            }
        }
        Ok(!self.disposed.load(std::sync::atomic::Ordering::Relaxed))
    }

    async fn apply_update(&self, patch: &SessionMetaPatch, touch_updated_at: Option<bool>) -> Result<bool, WireAsyncError> {
        let lease = self.session_locks.lease(&self.ctx.session_id());
        let result = async {
            let updated_at = patch.updated_at.unwrap_or_else(|| {
                if touch_updated_at == Some(false) {
                    self.data().updated_at
                } else {
                    chrono::Utc::now().timestamp_millis()
                }
            });
            let mut next = self.data();
            apply_patch(&mut next, patch);
            next.updated_at = updated_at;
            self.set_data(next.clone());
            self.store.set(&self.scope, META_KEY, encode_session_meta(&next)).await?;
            if self.disposed.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(false);
            }
            self.mirror_to_read_model();
            self.fire(SessionMetadataChangedEvent {
                changed: changed_patch_keys(patch),
            });
            Ok(true)
        }
        .await;
        lease.release();
        result
    }

    pub async fn set_title(&self, title: &str) -> Result<(), WireAsyncError> {
        self.update(
            SessionMetaPatch {
                title: Some(title.to_string()),
                title_kind: Some(SessionTitleKind::Custom),
                ..Default::default()
            },
            Some(false),
        )
        .await
    }

    pub async fn set_generated_title_if_uncustomized(&self, title: &str, force: Option<bool>) -> Result<bool, WireAsyncError> {
        self.enqueue_update(move || async move {
            if !(self.prepare_write().await?) {
                return Ok(false);
            }
            if force != Some(true) && self.data().title_kind == Some(SessionTitleKind::Custom) {
                return Ok(false);
            }
            self.apply_update(
                &SessionMetaPatch {
                    title: Some(title.to_string()),
                    title_kind: Some(SessionTitleKind::Generated),
                    ..Default::default()
                },
                Some(false),
            )
            .await
        })
        .await
    }

    pub async fn apply_prompt_metadata(&self, last_prompt: &str, title: &str) -> Result<bool, WireAsyncError> {
        self.enqueue_update(move || async move {
            if !(self.prepare_write().await?) {
                return Ok(false);
            }
            let apply_title = self.data().title_kind != Some(SessionTitleKind::Custom)
                && is_untitled(&self.data().title);
            let patch = if apply_title {
                SessionMetaPatch {
                    last_prompt: Some(last_prompt.to_string()),
                    title: Some(title.to_string()),
                    title_kind: Some(SessionTitleKind::Replaceable),
                    ..Default::default()
                }
            } else {
                SessionMetaPatch {
                    last_prompt: Some(last_prompt.to_string()),
                    ..Default::default()
                }
            };
            let applied = self.apply_update(&patch, None).await?;
            Ok(applied && apply_title)
        })
        .await
    }

    pub async fn set_archived(&self, archived: bool) -> Result<(), WireAsyncError> {
        self.update(
            SessionMetaPatch {
                archived: Some(archived),
                archived_at: archived.then(|| chrono::Utc::now().timestamp_millis()),
                ..Default::default()
            },
            Some(false),
        )
        .await
    }

    pub async fn register_agent(&self, agent_id: &str, meta: AgentMeta) -> Result<(), WireAsyncError> {
        self.enqueue_update(move || async move {
            self.ready().await?;
            let existing = self.data().agents.as_ref().and_then(|agents| agents.get(agent_id)).cloned();
            if existing.as_ref().is_some_and(|existing| agent_meta_equals(&existing, &meta)) {
                return Ok(());
            }
            if !self.session_locks.try_hold(&self.ctx.session_id()) {
                self.log.warn(
                    "session metadata agent registration skipped; session is in use by another client",
                    Some(json!({ "sessionId": self.ctx.session_id(), "agentId": agent_id })),
                );
                return Ok(());
            }
            if !(self.prepare_write().await?) {
                return Ok(());
            }
            let mut agents = self.data().agents.unwrap_or_default();
            agents.insert(agent_id.to_string(), meta);
            self.apply_update(
                &SessionMetaPatch {
                    agents: Some(agents),
                    ..Default::default()
                },
                Some(false),
            )
            .await
        })
        .await
    }

    fn enqueue_update<T: Clone + Send + 'static>(
        &self,
        work: impl FnOnce() -> BoxFuture<'static, Result<T, WireAsyncError>> + Send + 'static,
    ) -> BoxFuture<'static, Result<T, WireAsyncError>> {
        let previous = self.update_queue.lock().clone();
        let run = async move {
            if let Some(previous) = previous {
                previous.await;
            }
            work().await.map_err(SharedMetadataError::new)
        }
        .boxed()
        .shared();
        let tail = {
            let run = run.clone();
            async move {
                let _ = run.await;
            }
            .boxed()
            .shared()
        };
        self.update_queue.lock().replace(tail.clone());
        let id = {
            let mut next = NEXT_WRITE_ID.lock();
            let id = *next;
            *next += 1;
            id
        };
        PENDING_WRITES.lock().insert(id, tail.clone());
        tokio::spawn(tail.clone());
        async move {
            run.await.map_err(SharedMetadataError::boxed)
        }
        .boxed()
    }

    pub fn effective_cwd(&self) -> String {
        if self.data().environment_id.as_deref().is_some_and(|id| id != LOCAL_ENVIRONMENT_ID) {
            return self.ctx.cwd();
        }
        let binding = EnvironmentBinding {
            environment_id: self.data().environment_id.clone().unwrap_or_else(|| LOCAL_ENVIRONMENT_ID.to_string()),
            cwd: self.data().environment_cwd.clone(),
        };
        resolve_effective_workspace_roots(self.ctx.as_ref(), Some(&binding), self.workspace_info.as_ref(), None).work_dir
    }

    fn mirror_to_read_model(&self) {
        mirror_to_read_model(&self.states, &self.ctx, &self.mirror, &self.workspace_info, &self.scope, &self.log);
    }

    fn fire(&self, event: SessionMetadataChangedEvent) {
        let listeners = self.listeners.lock().values().cloned().collect::<Vec<_>>();
        for listener in listeners {
            listener(event.clone());
        }
    }

    pub fn on_did_change_metadata(&self, listener: Arc<dyn Fn(SessionMetadataChangedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        let id = {
            let mut next = self.next_listener_id.lock();
            let id = *next;
            *next += 1;
            self.listeners.lock().insert(id, listener);
            id
        };
        let listeners = self.listeners.clone();
        crate::_base::di::lifecycle::to_disposable(move || {
            listeners.lock().remove(&id);
        })
    }

    async fn load(&self, scope: String) -> Result<(), SharedMetadataError> {
        if let Some(existing) = self.store.get(&scope, META_KEY).await.map_err(|error| SharedMetadataError::new(error))? {
            let raw = existing;
            let existing = normalize_session_meta(raw.clone(), &self.ctx.session_id());
            let needs_defaults = existing.agents.is_none() || existing.custom.is_none();
            let needs_migration = session_meta_title_needs_migration(&raw, &existing);
            self.set_data(existing.clone());
            if needs_defaults || needs_migration {
                let mut next = existing;
                next.agents.get_or_insert_with(HashMap::new);
                next.custom.get_or_insert_with(HashMap::new);
                self.set_data(next.clone());
                if self.session_locks.try_hold(&self.ctx.session_id()) {
                    self.store.set(&scope, META_KEY, encode_session_meta(&next)).await.map_err(|error| SharedMetadataError::new(error))?;
                }
            }
            return Ok(());
        }
        let now = chrono::Utc::now().timestamp_millis();
        self.set_data(SessionMeta {
            id: self.ctx.session_id(),
            version: Some(SESSION_META_VERSION),
            cwd: Some(self.ctx.cwd()),
            created_at: now,
            updated_at: now,
            archived: false,
            agents: Some(HashMap::new()),
            custom: Some(HashMap::new()),
            ..Default::default()
        });
        if self.session_locks.try_hold(&self.ctx.session_id()) {
            self.store.set(&scope, META_KEY, encode_session_meta(&self.data())).await.map_err(|error| SharedMetadataError::new(error))?;
        }
        self.mirror_to_read_model();
        self.log.debug(
            "session metadata created",
            Some(json!({ "sessionId": self.ctx.session_id() })),
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl ISessionMetadata for SessionMetadata {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        SessionMetadata::ready(self)
    }

    fn effective_cwd(&self) -> String {
        SessionMetadata::effective_cwd(self)
    }

    fn on_did_change_metadata(&self, listener: Arc<dyn Fn(SessionMetadataChangedEvent) + Send + Sync>) -> Box<dyn IDisposable> {
        SessionMetadata::on_did_change_metadata(self, listener)
    }

    async fn read(&self) -> Result<SessionMeta, WireAsyncError> {
        SessionMetadata::read(self).await
    }

    async fn update(&self, patch: SessionMetaPatch, touch_updated_at: Option<bool>) -> Result<(), WireAsyncError> {
        SessionMetadata::update(self, patch, touch_updated_at).await
    }

    async fn apply_prompt_metadata(&self, last_prompt: &str, title: &str) -> Result<bool, WireAsyncError> {
        SessionMetadata::apply_prompt_metadata(self, last_prompt, title).await
    }

    async fn set_title(&self, title: &str) -> Result<(), WireAsyncError> {
        SessionMetadata::set_title(self, title).await
    }

    async fn set_generated_title_if_uncustomized(&self, title: &str, force: Option<bool>) -> Result<bool, WireAsyncError> {
        SessionMetadata::set_generated_title_if_uncustomized(self, title, force).await
    }

    async fn set_archived(&self, archived: bool) -> Result<(), WireAsyncError> {
        SessionMetadata::set_archived(self, archived).await
    }

    async fn register_agent(&self, agent_id: &str, meta: AgentMeta) -> Result<(), WireAsyncError> {
        SessionMetadata::register_agent(self, agent_id, meta).await
    }
}

fn apply_patch(data: &mut SessionMeta, patch: &SessionMetaPatch) {
    if let Some(value) = patch.version { data.version = Some(value); }
    if let Some(value) = &patch.title { data.title = Some(value.clone()); }
    if let Some(value) = patch.title_kind { data.title_kind = Some(value); }
    if let Some(value) = &patch.last_prompt { data.last_prompt = Some(value.clone()); }
    if let Some(value) = patch.archived { data.archived = value; }
    if let Some(value) = patch.archived_at { data.archived_at = Some(value); }
    if patch.archived == Some(false) { data.archived_at = None; }
    if let Some(value) = &patch.cwd { data.cwd = Some(value.clone()); }
    if let Some(value) = &patch.environment_id { data.environment_id = Some(value.clone()); }
    if let Some(value) = &patch.environment_cwd { data.environment_cwd = Some(value.clone()); }
    if let Some(value) = &patch.forked_from { data.forked_from = Some(value.clone()); }
    if let Some(value) = &patch.agents { data.agents = Some(value.clone()); }
    if let Some(value) = &patch.custom { data.custom = Some(value.clone()); }
    if let Some(value) = patch.last_turn_reason { data.last_turn_reason = Some(value); }
}

fn changed_patch_keys(patch: &SessionMetaPatch) -> Vec<String> {
    let mut keys = Vec::new();
    if patch.version.is_some() { keys.push("version".to_string()); }
    if patch.title.is_some() { keys.push("title".to_string()); }
    if patch.title_kind.is_some() { keys.push("titleKind".to_string()); }
    if patch.last_prompt.is_some() { keys.push("lastPrompt".to_string()); }
    if patch.updated_at.is_some() { keys.push("updatedAt".to_string()); }
    if patch.archived.is_some() { keys.push("archived".to_string()); }
    if patch.archived_at.is_some() || patch.archived.is_some() { keys.push("archivedAt".to_string()); }
    if patch.cwd.is_some() { keys.push("cwd".to_string()); }
    if patch.environment_id.is_some() { keys.push("environmentId".to_string()); }
    if patch.environment_cwd.is_some() { keys.push("environmentCwd".to_string()); }
    if patch.forked_from.is_some() { keys.push("forkedFrom".to_string()); }
    if patch.agents.is_some() { keys.push("agents".to_string()); }
    if patch.custom.is_some() { keys.push("custom".to_string()); }
    if patch.last_turn_reason.is_some() { keys.push("lastTurnReason".to_string()); }
    keys
}

fn agent_meta_equals(a: &AgentMeta, b: &AgentMeta) -> bool {
    a.homedir == b.homedir
        && a.type_ == b.type_
        && a.parent_agent_id == b.parent_agent_id
        && a.forked_from == b.forked_from
        && a.swarm_item == b.swarm_item
        && a.labels == b.labels
}

pub fn normalize_session_meta(raw: Value, session_id: &str) -> SessionMeta {
    let mut clean = raw.as_object().cloned().unwrap_or_default();
    let original = clean.clone();
    let legacy_created_at = clean.remove("createdAt");
    let legacy_updated_at = clean.remove("updatedAt");
    let legacy_work_dir = clean.remove("workDir");
    clean.remove("titleSource");
    clean.remove("isCustomTitle");
    clean.remove("customTitle");
    let cwd = clean
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| legacy_work_dir.as_ref().and_then(Value::as_str).map(str::to_string));
    let (title, title_kind) = normalize_session_title(&original);
    let id = if clean.get("version").and_then(Value::as_u64) == Some(SESSION_META_VERSION as u64) {
        clean.get("id").and_then(Value::as_str).unwrap_or(session_id).to_string()
    } else {
        session_id.to_string()
    };
    SessionMeta {
        id,
        version: Some(SESSION_META_VERSION),
        title,
        title_kind,
        last_prompt: clean.get("lastPrompt").and_then(Value::as_str).map(str::to_string),
        created_at: to_epoch_ms(legacy_created_at.as_ref()),
        updated_at: to_epoch_ms(legacy_updated_at.as_ref()),
        archived: clean.get("archived").and_then(Value::as_bool) == Some(true),
        archived_at: clean.get("archivedAt").and_then(Value::as_i64),
        cwd,
        environment_id: clean.get("environmentId").and_then(Value::as_str).map(str::to_string),
        environment_cwd: clean.get("environmentCwd").and_then(Value::as_str).map(str::to_string),
        forked_from: clean.get("forkedFrom").and_then(Value::as_str).map(str::to_string),
        agents: clean.get("agents").and_then(Value::as_object).cloned().unwrap_or_default().into_iter().filter_map(|(key, value)| {
            serde_json::from_value(value).ok().map(|meta| (key, meta))
        }).collect(),
        custom: clean.get("custom").and_then(Value::as_object).cloned(),
        last_turn_reason: clean.get("lastTurnReason").and_then(|value| serde_json::from_value(value).ok()),
    }
}

fn normalize_session_title(raw: &Map<String, Value>) -> (Option<String>, Option<SessionTitleKind>) {
    let title = raw.get("title").and_then(Value::as_str).map(str::to_string);
    if title.is_some() && raw.get("isCustomTitle").and_then(Value::as_bool) == Some(true) {
        return (title, Some(SessionTitleKind::Custom));
    }
    if title.is_some() {
        if let Some(kind) = raw.get("titleKind").and_then(Value::as_str) {
            if matches!(kind, "replaceable" | "generated" | "custom") {
                return (
                    title,
                    Some(match kind {
                        "generated" => SessionTitleKind::Generated,
                        "custom" => SessionTitleKind::Custom,
                        _ => SessionTitleKind::Replaceable,
                    }),
                );
            }
        }
    }
    if title.is_some() && raw.get("isCustomTitle").and_then(Value::as_bool) == Some(false) {
        return (title, Some(SessionTitleKind::Replaceable));
    }
    if let Some(custom_title) = raw.get("customTitle").and_then(Value::as_str) {
        return (Some(custom_title.to_string()), Some(SessionTitleKind::Custom));
    }
    if title.is_some() {
        return (title, Some(SessionTitleKind::Replaceable));
    }
    (None, None)
}

pub fn encode_session_meta(meta: &SessionMeta) -> PersistedSessionMeta {
    PersistedSessionMeta {
        meta: meta.clone(),
        is_custom_title: meta.title_kind == Some(SessionTitleKind::Custom),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PersistedSessionMeta {
    #[serde(flatten)]
    pub meta: SessionMeta,
    pub is_custom_title: bool,
}

fn session_meta_title_needs_migration(raw: &SessionMeta, normalized: &SessionMeta) -> bool {
    let record = serde_json::to_value(raw).unwrap();
    record.get("title") != Some(&json!(normalized.title))
        || record.get("titleKind") != Some(&json!(normalized.title_kind))
        || record.get("isCustomTitle").and_then(Value::as_bool) != Some(normalized.title_kind == Some(SessionTitleKind::Custom))
        || record.get("titleSource").is_some()
        || record.get("customTitle").is_some()
}

pub fn to_epoch_ms(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().filter(|value| value.is_finite()).unwrap_or(0.0) as i64,
        Some(Value::String(value)) => chrono::DateTime::parse_from_rfc3339(value)
            .map(|date| date.timestamp_millis())
            .or_else(|_| {
                chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                    .map(|date| date.and_hms_milli_opt(0, 0, 0, 0).unwrap().and_utc().timestamp_millis())
            })
            .unwrap_or(0),
        _ => 0,
    }
}

fn mirror_to_read_model(
    states: &Arc<dyn ISessionStateService>,
    ctx: &Arc<dyn ISessionContext>,
    mirror: &Arc<dyn ISessionIndexMirror>,
    workspace_info: &Arc<dyn ISessionWorkspaceInfo>,
    scope: &str,
    log: &Arc<dyn ILogService>,
) {
    let data = states.get(&SESSION_METADATA_DATA_KEY).expect("session metadata is not loaded");
    let binding = EnvironmentBinding {
        environment_id: data.environment_id.clone().unwrap_or_else(|| LOCAL_ENVIRONMENT_ID.to_string()),
        cwd: data.environment_cwd.clone(),
    };
    let effective_cwd = if data.environment_id.as_deref().is_some_and(|id| id != LOCAL_ENVIRONMENT_ID) {
        ctx.cwd()
    } else {
        resolve_effective_workspace_roots(ctx.as_ref(), Some(&binding), workspace_info.as_ref(), None).work_dir
    };
    let summary = build_session_summary(SessionSummaryInput {
        id: data.id.clone(),
        workspace_id: ctx.workspace_id(),
        cwd: Some(effective_cwd),
        environment_id: data.environment_id.clone(),
        environment_cwd: data.environment_cwd.clone(),
        title: data.title.clone(),
        last_prompt: data.last_prompt.clone(),
        created_at: data.created_at,
        updated_at: data.updated_at,
        archived: data.archived,
        archived_at: data.archived_at,
        custom: data.custom.clone(),
        last_turn_reason: data.last_turn_reason.clone(),
    });
    if let Err(error) = mirror.record(summary) {
        log.warn(
            "session index mirror record failed; the read model heals by reconciliation",
            Some(json!({
                "sessionId": ctx.session_id(),
                "scope": scope,
                "error": error.to_string(),
            })),
        );
    }
}
