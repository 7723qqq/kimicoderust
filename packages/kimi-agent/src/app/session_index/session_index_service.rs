use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::instantiation::LiveRef;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::_base::utils::timer::IntervalTimer;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::workspace::workspace::{IWorkspaceService, Workspace};
use crate::app::workspace::workspace_alias::{
    SESSION_INDEX_KEY, SESSION_INDEX_SCOPE, read_session_index_entries,
};
use crate::app::workspace::workspace_persistence::IWorkspacePersistence;
use crate::persistence::config_section::database_base_enabled;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::query_store::{
    Checkpoint, ColumnBounds, ColumnPageQuery, IQueryStore, IQueryStoreExt, IndexDef, Page,
    QueryFilter, SortDir,
};
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::session_index::{
    CHILD_SESSION_KIND, CHILD_SESSION_KIND_KEY, ISessionIndex, ISessionIndexMirror,
    PARENT_SESSION_ID_KEY, SessionCountQuery, SessionIndexPrepareOptions, SessionIndexState,
    SessionIndexStatus, SessionListQuery, SessionSummary,
};
use super::session_index_dirty_journal::{list_dirty_marks, mark_session_dirty};
use super::session_index_model::{
    PARENT_INDEX_NAME, SESSION_INDEX_MANIFEST, SESSION_INDEX_SCHEMA_VERSION,
    SessionWorkspaceCounts, recency_column, session_collection, session_counters_collection,
    strip_recency_field,
};
use super::session_index_projector::{SessionIndexProjector, SessionIndexProjectorDeps};
use super::session_index_source::{
    list_session_ids, list_workspace_ids, read_session_summary, summary_matches_child_of,
};

const RECONCILE_INTERVAL_MS: i64 = 60_000;
const DEGRADED_RETRY_MS: i64 = 5_000;
const TIE_REPAIR_LIMIT: usize = 1_000;
const UNBOUNDED: usize = 9_007_199_254_740_991;

#[derive(Default)]
struct FileSessionIndexState {
    state: SessionIndexState,
    generation: Option<u64>,
    status_reason: Option<String>,
    degraded_count: u64,
    next_prepare_retry_at: i64,
    last_degraded_key: Option<String>,
}

struct SessionIndexHint {
    mtime_ms: Option<i64>,
    by_id: HashMap<String, String>,
}

#[derive(Clone)]
struct CursorPosition {
    u: i64,
    id: String,
    before: bool,
}

#[derive(Default)]
struct CursorResolution {
    filter: QueryFilter,
    bounds: ColumnBounds,
    position: Option<CursorPosition>,
}

#[derive(Debug, Clone)]
struct SharedPrepareError(Arc<String>);

impl SharedPrepareError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedPrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

impl Error for SharedPrepareError {}

pub struct FileSessionIndex {
    bootstrap: Arc<dyn IBootstrapService>,
    storage: Arc<dyn IFileSystemStorageService>,
    docs: Arc<dyn IAtomicDocumentStore>,
    query_store: Arc<dyn IQueryStore>,
    config: Arc<dyn IConfigService>,
    mirror: Arc<dyn ISessionIndexMirror>,
    telemetry: Arc<dyn ITelemetryService>,
    log: Arc<dyn ILogService>,
    workspaces: LiveRef<Arc<dyn IWorkspaceService>>,
    workspace_store: LiveRef<Arc<dyn IWorkspacePersistence>>,
    state: Mutex<FileSessionIndexState>,
    prepare_flight: Mutex<Option<(u64, Shared<BoxFuture<'static, SessionIndexStatus>>)>>,
    project_flight: Mutex<Option<(u64, Shared<BoxFuture<'static, ()>>)>>,
    next_flight_id: AtomicU64,
    reconcile_timer: IntervalTimer,
    reconcile_timer_set: AtomicBool,
    projector: Arc<SessionIndexProjector>,
    primary_snapshot: Mutex<Option<HashMap<String, String>>>,
    primary_refresh_lock: Arc<tokio::sync::Mutex<()>>,
    workspace_persistence_subscription: Mutex<Option<DisposableRef>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
    session_index_hint: Mutex<Option<SessionIndexHint>>,
    weak: Weak<Self>,
}

impl FileSessionIndex {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        storage: Arc<dyn IFileSystemStorageService>,
        docs: Arc<dyn IAtomicDocumentStore>,
        query_store: Arc<dyn IQueryStore>,
        config: Arc<dyn IConfigService>,
        mirror: Arc<dyn ISessionIndexMirror>,
        telemetry: Arc<dyn ITelemetryService>,
        log: Arc<dyn ILogService>,
        workspaces: LiveRef<Arc<dyn IWorkspaceService>>,
        workspace_store: LiveRef<Arc<dyn IWorkspacePersistence>>,
    ) -> Arc<Self> {
        let sessions_scope = bootstrap.scope("sessions");
        let this = Arc::new_cyclic(|weak| {
            let primary_weak = weak.clone();
            let primary_by_workspace = Arc::new(move || {
                let weak = primary_weak.clone();
                Box::pin(async move {
                    let this = weak.upgrade().ok_or_else(|| {
                        Box::new(std::io::Error::other("session index is disposed"))
                            as WireAsyncError
                    })?;
                    this.primary_by_workspace().await
                }) as BoxFuture<'static, Result<HashMap<String, String>, WireAsyncError>>
            });
            let projector = SessionIndexProjector::new(SessionIndexProjectorDeps {
                storage: storage.clone(),
                docs: docs.clone(),
                query_store: query_store.clone(),
                log: log.clone(),
                sessions_scope,
                primary_by_workspace,
            });
            Self {
                bootstrap,
                storage,
                docs,
                query_store,
                config,
                mirror,
                telemetry,
                log,
                workspaces,
                workspace_store,
                state: Mutex::new(FileSessionIndexState::default()),
                prepare_flight: Mutex::new(None),
                project_flight: Mutex::new(None),
                next_flight_id: AtomicU64::new(1),
                reconcile_timer: IntervalTimer::new(true),
                reconcile_timer_set: AtomicBool::new(false),
                projector,
                primary_snapshot: Mutex::new(None),
                primary_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
                workspace_persistence_subscription: Mutex::new(None),
                subscriptions: Mutex::new(Vec::new()),
                session_index_hint: Mutex::new(None),
                weak: weak.clone(),
            }
        });
        this.subscribe_workspace_persistence();
        let weak = Arc::downgrade(&this);
        let subscription = this.workspace_store.on_did_change.subscribe(move |_| {
            if let Some(this) = weak.upgrade() {
                this.subscribe_workspace_persistence();
            }
        });
        this.subscriptions.lock().push(subscription);
        this
    }

    fn subscribe_workspace_persistence(&self) {
        let subscription = self.workspace_store.current().map(|store| {
            let weak = self.weak.clone();
            let subscription = store.on_did_change(Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.schedule_primary_refresh();
                }
            }));
            Arc::from(subscription) as DisposableRef
        });
        *self.workspace_persistence_subscription.lock() = subscription;
    }

    fn schedule_primary_refresh(self: &Arc<Self>) {
        let lock = self.primary_refresh_lock.clone();
        let log = self.log.clone();
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            if let Err(error) = this.refresh_primaries().await {
                log.warn(
                    "session index primary refresh failed",
                    Some(json!({ "error": error.to_string() })),
                );
            }
        });
    }

    fn ensure_reconcile_timer(&self) {
        if self.reconcile_timer_set.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = self.weak.clone();
        self.reconcile_timer.cancel_and_set(
            move || {
                if let Some(this) = weak.upgrade() {
                    tokio::spawn(async move {
                        this.tick().await;
                    });
                }
            },
            RECONCILE_INTERVAL_MS,
        );
    }

    fn spawn_prepare(&self) {
        if let Some(this) = self.weak.upgrade() {
            tokio::spawn(async move {
                this.prepare(None).await;
            });
        }
    }

    pub async fn prepare(
        &self,
        options: Option<SessionIndexPrepareOptions>,
    ) -> SessionIndexStatus {
        if !self.read_model_enabled() {
            return self.status();
        }
        if let Some((_, flight)) = self.prepare_flight.lock().as_ref().cloned() {
            return flight.await;
        }
        let id = self.next_flight_id.fetch_add(1, Ordering::Relaxed);
        let this = self
            .weak
            .upgrade()
            .expect("file session index weak self is unavailable");
        let flight = {
            let this = this.clone();
            async move {
                let status = this.do_prepare(options.and_then(|options| options.deadline_ms)).await;
                let mut current = this.prepare_flight.lock();
                if current.as_ref().is_some_and(|(current_id, _)| *current_id == id) {
                    *current = None;
                }
                status
            }
            .boxed()
            .shared()
        };
        *self.prepare_flight.lock() = Some((id, flight.clone()));
        flight.await
    }

    pub fn status(&self) -> SessionIndexStatus {
        let state = self.state.lock();
        SessionIndexStatus {
            state: if self.read_model_enabled() {
                state.state
            } else {
                SessionIndexState::Uninitialized
            },
            generation: state.generation,
            reason: state.status_reason.clone(),
            degraded_count: state.degraded_count,
        }
    }

    async fn do_prepare(&self, deadline_ms: Option<u64>) -> SessionIndexStatus {
        if self.state.lock().state == SessionIndexState::Ready {
            return self.status();
        }
        self.state.lock().state = SessionIndexState::Preparing;
        let result = async {
            if self.primary_snapshot.lock().is_none() {
                let primaries = self.primary_by_workspace().await?;
                *self.primary_snapshot.lock() = Some(primaries);
            }
            let manifest = self
                .query_store
                .get_checkpoint(SESSION_INDEX_MANIFEST)
                .await?;
            if manifest
                .as_ref()
                .is_none_or(|manifest| manifest.schema_version != Some(SESSION_INDEX_SCHEMA_VERSION))
            {
                self.wait_for_projection(deadline_ms).await;
            } else if let Some(manifest) = manifest {
                self.state.lock().generation = Some(manifest.seq);
                self.ensure_schema(manifest.seq).await?;
                if !self.manifest_primaries_fresh(&manifest).await? {
                    self.wait_for_projection(deadline_ms).await;
                } else if !self.manifest_fresh(&manifest).await {
                    let reconciliation = self.wait_for_reconciliation(manifest.seq, deadline_ms).await;
                    if let Err(error) = reconciliation {
                        let published = self
                            .query_store
                            .get_checkpoint(SESSION_INDEX_MANIFEST)
                            .await
                            .ok()
                            .flatten();
                        let Some(published) = published else {
                            return Err(error);
                        };
                        self.state.lock().generation = Some(published.seq);
                        self.log.warn(
                            "session index startup reconciliation failed; serving the published generation",
                            Some(json!({ "error": error.to_string() })),
                        );
                    }
                }
            }
            if let Some(published) = self
                .query_store
                .get_checkpoint(SESSION_INDEX_MANIFEST)
                .await?
            {
                self.state.lock().generation = Some(published.seq);
                self.mark_ready();
            }
            Ok::<(), WireAsyncError>(())
        }
        .await;
        if let Err(error) = result {
            self.mark_degraded("prepare failed", Some(&error));
        }
        self.status()
    }

    async fn wait_for_projection(&self, deadline_ms: Option<u64>) {
        let projection = self.ensure_projection();
        if let Some(deadline_ms) = deadline_ms {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(deadline_ms),
                projection,
            )
            .await;
        } else {
            projection.await;
        }
    }

    async fn wait_for_reconciliation(
        &self,
        generation: u64,
        deadline_ms: Option<u64>,
    ) -> Result<(), WireAsyncError> {
        let projector = self.projector.clone();
        let reconciliation = async move {
            projector
                .reconcile(generation)
                .await
                .map(|_| ())
                .map_err(SharedPrepareError::new)
        }
        .boxed()
        .shared();
        tokio::spawn(reconciliation.clone());
        let result = if let Some(deadline_ms) = deadline_ms {
            tokio::time::timeout(
                std::time::Duration::from_millis(deadline_ms),
                reconciliation.clone(),
            )
            .await
            .unwrap_or_else(|_| Ok(()))
        } else {
            reconciliation.await
        };
        result.map_err(SharedPrepareError::boxed)
    }

    async fn manifest_primaries_fresh(
        &self,
        manifest: &Checkpoint,
    ) -> Result<bool, WireAsyncError> {
        Ok(same_primary_by_workspace(
            manifest.workspace_primaries.as_ref(),
            &self.primary_by_workspace().await?,
        ))
    }

    async fn manifest_fresh(&self, manifest: &Checkpoint) -> bool {
        let Some(signals) = manifest.workspace_signals.as_ref() else {
            return false;
        };
        let result = async {
            let (marks, workspace_ids, primaries) = tokio::join!(
                list_dirty_marks(self.storage.as_ref(), &self.sessions_scope()),
                list_workspace_ids(self.storage.as_ref(), &self.sessions_scope()),
                self.primary_by_workspace()
            );
            let marks = marks?;
            let primaries = primaries?;
            if !same_primary_by_workspace(manifest.workspace_primaries.as_ref(), &primaries) {
                return Ok::<bool, WireAsyncError>(false);
            }
            if !marks.is_empty() || workspace_ids.len() != signals.len() {
                return Ok(false);
            }
            for workspace_id in workspace_ids {
                let Some(recorded) = signals.get(&workspace_id) else {
                    return Ok(false);
                };
                let current = self
                    .storage
                    .mtime(&self.sessions_scope(), &workspace_id)
                    .await?;
                if current.and_then(|value| u64::try_from(value).ok()).as_ref() != Some(recorded) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        .await;
        match result {
            Ok(fresh) => fresh,
            Err(error) => {
                self.log.warn(
                    "session index freshness check failed; treating the index as stale",
                    Some(json!({ "error": error.to_string() })),
                );
                false
            }
        }
    }

    fn ensure_projection(&self) -> Shared<BoxFuture<'static, ()>> {
        if let Some((_, flight)) = self.project_flight.lock().as_ref().cloned() {
            return flight;
        }
        let id = self.next_flight_id.fetch_add(1, Ordering::Relaxed);
        let this = self
            .weak
            .upgrade()
            .expect("file session index weak self is unavailable");
        let flight = {
            let this = this.clone();
            async move {
                this.run_projection().await;
                let mut current = this.project_flight.lock();
                if current.as_ref().is_some_and(|(current_id, _)| *current_id == id) {
                    *current = None;
                }
            }
            .boxed()
            .shared()
        };
        *self.project_flight.lock() = Some((id, flight.clone()));
        tokio::spawn(flight.clone());
        flight
    }

    async fn run_projection(&self) {
        let started_at = chrono::Utc::now().timestamp_millis();
        let result = async {
            let manifest = self
                .query_store
                .get_checkpoint(SESSION_INDEX_MANIFEST)
                .await?;
            let next = manifest.map_or(0, |manifest| manifest.seq) + 1;
            self.projector.project(next).await
        }
        .await;
        match result {
            Ok(result) => {
                self.state.lock().generation = Some(result.generation);
                self.mark_ready();
                let properties = Map::from_iter([
                    (
                        "duration_ms".to_string(),
                        (chrono::Utc::now().timestamp_millis() - started_at).into(),
                    ),
                    ("session_count".to_string(), (result.sessions as u64).into()),
                    ("generation".to_string(), result.generation.into()),
                ]);
                self.telemetry.track2("session_index_projected", Some(properties));
            }
            Err(error) => {
                let published = self
                    .query_store
                    .get_checkpoint(SESSION_INDEX_MANIFEST)
                    .await
                    .ok()
                    .flatten();
                if let Some(published) = published {
                    self.state.lock().generation = Some(published.seq);
                    self.mark_ready();
                    self.log.warn(
                        "session index re-projection failed; staying on the previous generation",
                        Some(json!({
                            "generation": published.seq,
                            "error": error.to_string(),
                        })),
                    );
                } else {
                    self.mark_degraded("projection failed", Some(&error));
                }
            }
        }
    }

    pub async fn reconcile_now(&self) -> Result<(), WireAsyncError> {
        if !self.read_model_enabled() {
            return Ok(());
        }
        let Some(manifest) = self
            .query_store
            .get_checkpoint(SESSION_INDEX_MANIFEST)
            .await?
        else {
            return Ok(());
        };
        self.state.lock().generation = Some(manifest.seq);
        self.projector.reconcile(manifest.seq).await.map(|_| ())
    }

    pub async fn reproject_now(&self) -> Result<(), WireAsyncError> {
        if self.read_model_enabled() {
            self.ensure_projection().await;
        }
        Ok(())
    }

    pub fn stop_reconcile_loop(&self) {
        self.reconcile_timer.dispose();
        self.reconcile_timer_set.store(false, Ordering::Release);
    }

    async fn tick(&self) {
        if !self.read_model_enabled() {
            return;
        }
        match self.state.lock().state {
            SessionIndexState::Degraded => {
                self.spawn_prepare();
                return;
            }
            SessionIndexState::Ready => {}
            _ => return,
        }
        let result = async {
            let manifest = self
                .query_store
                .get_checkpoint(SESSION_INDEX_MANIFEST)
                .await?;
            let Some(manifest) = manifest else {
                self.mark_degraded("published generation lost", None);
                self.spawn_prepare();
                return Ok::<(), WireAsyncError>(());
            };
            self.state.lock().generation = Some(manifest.seq);
            if self.manifest_fresh(&manifest).await {
                return Ok(());
            }
            self.projector.reconcile(manifest.seq).await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.log.warn(
                "session index reconciliation failed",
                Some(json!({ "error": error.to_string() })),
            );
        }
    }

    fn mark_ready(&self) {
        let degraded = {
            let mut state = self.state.lock();
            let degraded = state.state == SessionIndexState::Degraded;
            let degraded_count = state.degraded_count;
            state.state = SessionIndexState::Ready;
            state.status_reason = None;
            state.last_degraded_key = None;
            drop(state);
            if degraded {
                self.log.info(
                    "session index read model recovered",
                    Some(json!({ "degradedCount": degraded_count })),
                );
            }
        };
        let _ = degraded;
        self.ensure_reconcile_timer();
    }

    fn mark_degraded(&self, reason: &str, error: Option<&WireAsyncError>) {
        let detail = error.map(|error| error.to_string());
        let episode_key = format!("{reason}:{}", detail.as_deref().unwrap_or(""));
        let degraded_count = {
            let mut state = self.state.lock();
            state.state = SessionIndexState::Degraded;
            state.status_reason = Some(reason.to_string());
            state.degraded_count += 1;
            state.next_prepare_retry_at =
                chrono::Utc::now().timestamp_millis() + DEGRADED_RETRY_MS;
            if state.last_degraded_key.as_deref() == Some(episode_key.as_str()) {
                return;
            }
            state.last_degraded_key = Some(episode_key);
            state.degraded_count
        };
        self.ensure_reconcile_timer();
        let mut log_properties = Map::from_iter([
            ("reason".to_string(), reason.into()),
            ("degradedCount".to_string(), degraded_count.into()),
        ]);
        if let Some(detail) = &detail {
            log_properties.insert("error".to_string(), detail.clone().into());
        }
        self.log.warn(
            "session index read model degraded; serving authoritative reads",
            Some(Value::Object(log_properties)),
        );
        let mut properties = Map::from_iter([
            ("reason".to_string(), reason.into()),
            ("degraded_count".to_string(), degraded_count.into()),
        ]);
        if let Some(error) = error {
            let error_type = error
                .downcast_ref::<Error2>()
                .map(|error| error.code().as_str().to_string())
                .unwrap_or_else(|| "Unknown".to_string());
            properties.insert("error_type".to_string(), error_type.into());
        }
        self.telemetry
            .track2("session_index_degraded", Some(properties));
    }

    async fn ensure_schema(&self, generation: u64) -> Result<(), WireAsyncError> {
        self.query_store
            .ensure_index(
                &session_collection(generation),
                IndexDef::Value {
                    name: PARENT_INDEX_NAME.to_string(),
                    field: format!("custom.{PARENT_SESSION_ID_KEY}"),
                    unique: None,
                },
            )
            .await
    }

    pub async fn get(&self, id: &str) -> Result<Option<SessionSummary>, WireAsyncError> {
        self.with_read_model(
            |generation| self.get_from_read_model(generation, id),
            || self.get_legacy(id),
        )
        .await
    }

    pub async fn list_recent(
        &self,
        query: SessionListQuery,
    ) -> Result<Page<SessionSummary>, WireAsyncError> {
        self.with_read_model(
            |generation| self.list_recent_from_read_model(generation, &query),
            || self.list_legacy(&query),
        )
        .await
    }

    pub async fn count(&self, query: SessionCountQuery) -> Result<i64, WireAsyncError> {
        self.with_read_model(
            |generation| self.count_from_read_model(generation, &query),
            || self.count_legacy(&query),
        )
        .await
    }

    pub async fn remove(&self, id: &str) -> Result<(), WireAsyncError> {
        self.mirror.evict(id).await?;
        self.with_read_model(
            |generation| async move {
                self.query_store
                    .delete(&session_collection(generation), id)
                    .await
            },
            || async move { Ok(()) },
        )
        .await?;
        if let Err(error) = mark_session_dirty(self.storage.as_ref(), &self.sessions_scope(), id).await {
            self.log.warn(
                "session index dirty mark failed",
                Some(json!({ "error": error.to_string() })),
            );
        }
        Ok(())
    }

    async fn with_read_model<T, F, Fut, L, Lut>(
        &self,
        op: F,
        legacy: L,
    ) -> Result<T, WireAsyncError>
    where
        T: Send,
        F: FnOnce(u64) -> Fut + Send,
        Fut: Future<Output = Result<T, WireAsyncError>> + Send,
        L: FnOnce() -> Lut + Send,
        Lut: Future<Output = Result<T, WireAsyncError>> + Send,
    {
        if !self.read_model_enabled() {
            return legacy().await;
        }
        match self.state.lock().state {
            SessionIndexState::Uninitialized => {
                self.spawn_prepare();
                return legacy().await;
            }
            SessionIndexState::Preparing => return legacy().await,
            SessionIndexState::Degraded => {
                if chrono::Utc::now().timestamp_millis()
                    >= self.state.lock().next_prepare_retry_at
                {
                    self.spawn_prepare();
                }
                return legacy().await;
            }
            SessionIndexState::Ready => {}
        }
        let manifest = match self
            .query_store
            .get_checkpoint(SESSION_INDEX_MANIFEST)
            .await
        {
            Ok(manifest) => manifest,
            Err(error) => {
                self.mark_degraded("read model read failed", Some(&error));
                return legacy().await;
            }
        };
        let Some(manifest) = manifest else {
            self.mark_degraded("published generation lost", None);
            self.spawn_prepare();
            return legacy().await;
        };
        self.state.lock().generation = Some(manifest.seq);
        match op(manifest.seq).await {
            Ok(value) => Ok(value),
            Err(error) => {
                self.mark_degraded("read model read failed", Some(&error));
                legacy().await
            }
        }
    }

    async fn get_from_read_model(
        &self,
        generation: u64,
        id: &str,
    ) -> Result<Option<SessionSummary>, WireAsyncError> {
        if let Some(queued) = self
            .mirror
            .pending()
            .into_iter()
            .find(|summary| summary.id == id)
        {
            return Ok(Some(queued));
        }
        let cached = self
            .query_store
            .get::<Value>(&session_collection(generation), id)
            .await?;
        if let Some(cached) = cached
            && is_session_summary_shape(&cached)
            && let Ok(summary) = serde_json::from_value::<SessionSummary>(cached)
        {
            return Ok(Some(strip_recency_field(generation, summary)));
        }
        let summary = self.get_legacy(id).await?;
        if let Some(summary) = &summary {
            self.mirror.record(summary.clone())?;
        }
        Ok(summary)
    }

    async fn list_recent_from_read_model(
        &self,
        generation: u64,
        query: &SessionListQuery,
    ) -> Result<Page<SessionSummary>, WireAsyncError> {
        if let Some(session_id) = &query.session_id {
            let summary = self.get_from_read_model(generation, session_id).await?;
            let mut items = summary
                .filter(|summary| !summary.archived || query.include_archived == Some(true))
                .into_iter()
                .collect::<Vec<_>>();
            if let Some(limit) = query.limit {
                items.truncate(limit);
            }
            return Ok(Page {
                items,
                next_cursor: None,
            });
        }
        let Some(cursor) = self.resolve_cursor(generation, query).await? else {
            return Ok(Page {
                items: Vec::new(),
                next_cursor: None,
            });
        };
        let queued = self.mirror.pending();
        let queued_ids = queued
            .iter()
            .map(|summary| summary.id.clone())
            .collect::<HashSet<_>>();
        let limit = query.limit.unwrap_or(UNBOUNDED);
        let mut filter = self.base_filter(query);
        filter.extend(cursor.filter.clone());
        let page = self
            .windowed_page(
                generation,
                session_collection(generation),
                recency_column(generation),
                filter,
                query.child_of.is_some(),
                &queued_ids,
                cursor.bounds,
                limit,
            )
            .await?;
        Ok(self.merge_pending(page, query, &queued, cursor.position.as_ref()))
    }

    async fn fetch_page_rows(
        &self,
        collection: &str,
        column: &str,
        filter: &QueryFilter,
        child_query: bool,
        bounds: ColumnBounds,
        row_limit: usize,
    ) -> Result<Vec<SessionSummary>, WireAsyncError> {
        if child_query {
            let mut query = self.query_store.query::<SessionSummary>(collection);
            query.where_(filter.clone());
            query.order_by("updatedAt".to_string(), Some(SortDir::Desc));
            query.limit(row_limit);
            if bounds.gt.is_some()
                || bounds.gte.is_some()
                || bounds.lt.is_some()
                || bounds.lte.is_some()
            {
                query.where_column(column.to_string(), bounds);
            }
            return Ok(query.execute().await?.items);
        }
        Ok(self
            .query_store
            .page_by_column::<SessionSummary>(
                collection,
                ColumnPageQuery {
                    column: column.to_string(),
                    dir: Some(SortDir::Desc),
                    filter: Some(filter.clone()),
                    bounds: Some(bounds),
                    limit: row_limit,
                },
            )
            .await?
            .items)
    }

    async fn fetch_visible(
        &self,
        generation: u64,
        collection: &str,
        column: &str,
        filter: &QueryFilter,
        child_query: bool,
        queued_ids: &HashSet<String>,
        bounds: ColumnBounds,
        fetch_limit: usize,
    ) -> Result<Vec<SessionSummary>, WireAsyncError> {
        let rows = self
            .fetch_page_rows(collection, column, filter, child_query, bounds, fetch_limit)
            .await?;
        let visible = rows
            .iter()
            .filter(|record| !queued_ids.contains(&record.id))
            .cloned()
            .collect::<Vec<_>>();
        if visible.len() == rows.len() || rows.len() < fetch_limit {
            return Ok(visible
                .into_iter()
                .map(|record| strip_recency_field(generation, record))
                .collect());
        }
        let widened = self
            .fetch_page_rows(
                collection,
                column,
                filter,
                child_query,
                bounds,
                fetch_limit + queued_ids.len(),
            )
            .await?;
        Ok(widened
            .into_iter()
            .filter(|record| !queued_ids.contains(&record.id))
            .take(fetch_limit)
            .map(|record| strip_recency_field(generation, record))
            .collect())
    }

    async fn windowed_page(
        &self,
        generation: u64,
        collection: String,
        column: String,
        filter: QueryFilter,
        child_query: bool,
        queued_ids: &HashSet<String>,
        bounds: ColumnBounds,
        limit: usize,
    ) -> Result<Page<SessionSummary>, WireAsyncError> {
        let raw = self
            .fetch_visible(
                generation,
                &collection,
                &column,
                &filter,
                child_query,
                queued_ids,
                bounds,
                limit + 1,
            )
            .await?;
        if raw.len() <= limit {
            let mut items = raw;
            items.sort_by(canonical_order);
            return Ok(Page {
                items,
                next_cursor: None,
            });
        }
        let min_updated_at = raw
            .iter()
            .map(|summary| summary.updated_at)
            .min()
            .expect("a page beyond its limit is not empty");
        let tie = self
            .fetch_visible(
                generation,
                &collection,
                &column,
                &filter,
                child_query,
                queued_ids,
                ColumnBounds {
                    gte: Some(min_updated_at as f64),
                    lte: Some(min_updated_at as f64),
                    ..Default::default()
                },
                TIE_REPAIR_LIMIT,
            )
            .await?;
        let mut merged = HashMap::new();
        for summary in tie.iter().cloned().chain(raw) {
            merged.insert(summary.id.clone(), summary);
        }
        let mut items = merged.into_values().collect::<Vec<_>>();
        items.sort_by(canonical_order);
        let kept = items.iter().take(limit).cloned().collect::<Vec<_>>();
        let has_more = items.len() > limit || tie.len() >= TIE_REPAIR_LIMIT;
        let next_cursor = has_more.then(|| {
            kept.last()
                .expect("a limited page with more rows has a cursor")
                .id
                .clone()
        });
        Ok(Page {
            items: kept,
            next_cursor,
        })
    }

    fn merge_pending(
        &self,
        page: Page<SessionSummary>,
        query: &SessionListQuery,
        queued: &[SessionSummary],
        position: Option<&CursorPosition>,
    ) -> Page<SessionSummary> {
        let pending = queued
            .iter()
            .filter(|summary| {
                query.workspace_ids.as_ref().is_none_or(|workspace_ids| {
                    workspace_ids.contains(&summary.workspace_id)
                }) && (query.include_archived == Some(true) || !summary.archived)
                    && summary_matches_child_of(summary, query.child_of.as_deref())
                    && position.is_none_or(|position| {
                        if position.before {
                            summary.updated_at < position.u
                                || (summary.updated_at == position.u && summary.id < position.id)
                        } else {
                            summary.updated_at > position.u
                                || (summary.updated_at == position.u && summary.id > position.id)
                        }
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return page;
        }
        let mut merged = HashMap::new();
        for summary in pending {
            merged.insert(summary.id.clone(), summary);
        }
        for summary in page.items {
            merged.entry(summary.id.clone()).or_insert(summary);
        }
        let mut items = merged.into_values().collect::<Vec<_>>();
        items.sort_by(canonical_order);
        let Some(limit) = query.limit else {
            return Page {
                items,
                next_cursor: None,
            };
        };
        let kept = items.iter().take(limit).cloned().collect::<Vec<_>>();
        let has_more = page.next_cursor.is_some() || items.len() > limit;
        let next_cursor = has_more.then(|| {
            kept.last()
                .expect("a merged page with more rows has a cursor")
                .id
                .clone()
        });
        Page {
            items: kept,
            next_cursor,
        }
    }

    async fn resolve_cursor(
        &self,
        generation: u64,
        query: &SessionListQuery,
    ) -> Result<Option<CursorResolution>, WireAsyncError> {
        let Some(id) = query.before.as_ref().or(query.after.as_ref()) else {
            return Ok(Some(CursorResolution::default()));
        };
        let stored_value = self
            .query_store
            .get::<Value>(&session_collection(generation), id)
            .await?;
        let stored = stored_value
            .filter(is_session_summary_shape)
            .and_then(|value| serde_json::from_value::<SessionSummary>(value).ok());
        let cursor = match stored {
            Some(stored) => Some(stored),
            None => self
                .mirror
                .pending()
                .into_iter()
                .find(|summary| &summary.id == id),
        };
        let Some(cursor) = cursor else {
            return Ok(None);
        };
        let u = cursor.updated_at;
        if query.before.is_some() {
            return Ok(Some(CursorResolution {
                bounds: ColumnBounds {
                    lte: Some(u as f64),
                    ..Default::default()
                },
                filter: json!({
                    "$or": [
                        { "updatedAt": { "$lt": u } },
                        { "updatedAt": u, "id": { "$lt": id } }
                    ]
                })
                .as_object()
                .expect("cursor filter is an object")
                .clone(),
                position: Some(CursorPosition {
                    u,
                    id: id.clone(),
                    before: true,
                }),
            }));
        }
        Ok(Some(CursorResolution {
            bounds: ColumnBounds {
                gte: Some(u as f64),
                ..Default::default()
            },
            filter: json!({
                "$or": [
                    { "updatedAt": { "$gt": u } },
                    { "updatedAt": u, "id": { "$gt": id } }
                ]
            })
            .as_object()
            .expect("cursor filter is an object")
            .clone(),
            position: Some(CursorPosition {
                u,
                id: id.clone(),
                before: false,
            }),
        }))
    }

    fn base_filter(&self, query: &SessionListQuery) -> QueryFilter {
        let mut filter = Map::new();
        if let Some(workspace_ids) = &query.workspace_ids {
            filter.insert(
                "workspaceId".to_string(),
                if workspace_ids.len() == 1 {
                    workspace_ids[0].clone().into()
                } else {
                    json!({ "$in": workspace_ids })
                },
            );
        }
        if let Some(child_of) = &query.child_of {
            filter.insert(
                format!("custom.{PARENT_SESSION_ID_KEY}"),
                child_of.clone().into(),
            );
            filter.insert(
                format!("custom.{CHILD_SESSION_KIND_KEY}"),
                CHILD_SESSION_KIND.into(),
            );
        }
        if query.include_archived != Some(true) {
            filter.insert("archived".to_string(), json!({ "$ne": true }));
        }
        filter
    }

    fn sessions_scope(&self) -> String {
        self.bootstrap.scope("sessions")
    }

    async fn list_legacy(
        &self,
        query: &SessionListQuery,
    ) -> Result<Page<SessionSummary>, WireAsyncError> {
        if let Some(session_id) = &query.session_id {
            let summary = self.get_legacy(session_id).await?;
            let mut items = summary
                .filter(|summary| !summary.archived || query.include_archived == Some(true))
                .into_iter()
                .collect::<Vec<_>>();
            if let Some(limit) = query.limit {
                items.truncate(limit);
            }
            return Ok(Page {
                items,
                next_cursor: None,
            });
        }
        let mut items = self
            .collect_authoritative(query.workspace_ids.as_ref())
            .await?
            .into_iter()
            .filter(|summary| {
                (query.include_archived == Some(true) || !summary.archived)
                    && summary_matches_child_of(summary, query.child_of.as_deref())
            })
            .collect::<Vec<_>>();
        items.sort_by(canonical_order);
        let mut start = 0;
        let mut end = items.len();
        if let Some(cursor_id) = query.before.as_ref().or(query.after.as_ref()) {
            let Some(index) = items.iter().position(|summary| &summary.id == cursor_id) else {
                return Ok(Page {
                    items: Vec::new(),
                    next_cursor: None,
                });
            };
            if query.before.is_some() {
                start = index + 1;
            } else {
                end = index;
            }
        }
        let window = items[start..end].to_vec();
        let Some(limit) = query.limit else {
            return Ok(Page {
                items: window,
                next_cursor: None,
            });
        };
        let kept = window.iter().take(limit).cloned().collect::<Vec<_>>();
        let next_cursor = (window.len() > limit).then(|| {
            kept.last()
                .expect("a legacy page with more rows has a cursor")
                .id
                .clone()
        });
        Ok(Page {
            items: kept,
            next_cursor,
        })
    }

    async fn refresh_primaries(&self) -> Result<(), WireAsyncError> {
        let next = self.primary_by_workspace().await?;
        let previous = self.primary_snapshot.lock().replace(next.clone());
        if previous
            .as_ref()
            .is_some_and(|previous| same_primary_by_workspace(Some(previous), &next))
        {
            return Ok(());
        }
        if !self.read_model_enabled()
            || matches!(
                self.state.lock().state,
                SessionIndexState::Uninitialized | SessionIndexState::Preparing
            )
        {
            return Ok(());
        }
        self.ensure_projection().await;
        Ok(())
    }

    async fn primary_by_workspace(&self) -> Result<HashMap<String, String>, WireAsyncError> {
        let Some(workspaces) = self.workspaces.current() else {
            return Ok(HashMap::new());
        };
        Ok(workspaces
            .list()
            .await?
            .into_iter()
            .map(|workspace: Workspace| (workspace.id, workspace.root))
            .collect())
    }

    async fn get_legacy(&self, id: &str) -> Result<Option<SessionSummary>, WireAsyncError> {
        let primary_by_workspace = self.primary_by_workspace().await?;
        let hinted = self.workspace_hint_for(id).await;
        if let Some(hinted) = &hinted {
            if let Some(summary) = read_session_summary(
                self.docs.as_ref(),
                &self.sessions_scope(),
                hinted,
                id,
                primary_by_workspace.get(hinted).map(String::as_str),
            )
            .await
            {
                return Ok(Some(summary));
            }
        }
        for workspace_id in list_workspace_ids(self.storage.as_ref(), &self.sessions_scope()).await {
            if hinted.as_deref() == Some(workspace_id.as_str()) {
                continue;
            }
            if let Some(summary) = read_session_summary(
                self.docs.as_ref(),
                &self.sessions_scope(),
                &workspace_id,
                id,
                primary_by_workspace.get(&workspace_id).map(String::as_str),
            )
            .await
            {
                return Ok(Some(summary));
            }
        }
        Ok(None)
    }

    async fn workspace_hint_for(&self, id: &str) -> Option<String> {
        let mtime_ms = match self.storage.mtime(SESSION_INDEX_SCOPE, SESSION_INDEX_KEY).await {
            Ok(mtime_ms) => mtime_ms,
            Err(_) => return None,
        };
        if let Some(cached) = self.session_index_hint.lock().as_ref()
            && cached.mtime_ms == mtime_ms
        {
            return cached.by_id.get(id).cloned();
        }
        let mut by_id = HashMap::new();
        if mtime_ms.is_some() {
            let entries = match read_session_index_entries(self.storage.as_ref()).await {
                Ok(entries) => entries,
                Err(_) => return None,
            };
            for entry in entries {
                if let Some(workspace_id) = self.workspace_id_from_session_dir(&entry.session_dir) {
                    by_id.insert(entry.session_id, workspace_id);
                }
            }
        }
        let result = by_id.get(id).cloned();
        *self.session_index_hint.lock() = Some(SessionIndexHint { mtime_ms, by_id });
        result
    }

    fn workspace_id_from_session_dir(&self, session_dir: &str) -> Option<String> {
        let root = self.storage.path_for(&self.sessions_scope(), "")?;
        let root_slash = root.replace('\\', "/");
        let prefix = if root_slash.ends_with('/') {
            root_slash
        } else {
            format!("{root_slash}/")
        };
        let dir_slash = session_dir.replace('\\', "/");
        if !dir_slash.starts_with(&prefix) {
            return None;
        }
        let rest = &dir_slash[prefix.len()..];
        let slash = rest.find('/')?;
        if slash == 0 {
            return None;
        }
        Some(rest[..slash].to_string())
    }

    async fn count_legacy(&self, query: &SessionCountQuery) -> Result<i64, WireAsyncError> {
        Ok(self
            .collect_authoritative(query.workspace_ids.as_ref())
            .await?
            .into_iter()
            .filter(|summary| query.include_archived == Some(true) || !summary.archived)
            .count() as i64)
    }

    async fn count_from_read_model(
        &self,
        generation: u64,
        query: &SessionCountQuery,
    ) -> Result<i64, WireAsyncError> {
        let counters = session_counters_collection(generation);
        let restricted = query.workspace_ids.as_ref();
        let workspace_ids = match restricted {
            Some(workspace_ids) => workspace_ids.clone(),
            None => self.query_store.list_keys(&counters).await?,
        };
        let counts = self
            .query_store
            .get_many::<SessionWorkspaceCounts>(&counters, &workspace_ids)
            .await?;
        let mut total = 0_i64;
        for entry in counts.values() {
            total += if query.include_archived == Some(true) {
                entry.active + entry.archived
            } else {
                entry.active
            };
        }
        let pending = self
            .mirror
            .pending()
            .into_iter()
            .filter(|summary| {
                restricted.is_none_or(|restricted| restricted.contains(&summary.workspace_id))
            })
            .collect::<Vec<_>>();
        if pending.is_empty() {
            return Ok(total);
        }
        let stored = self
            .query_store
            .get_many::<SessionSummary>(
                &session_collection(generation),
                &pending
                    .iter()
                    .map(|summary| summary.id.clone())
                    .collect::<Vec<_>>(),
            )
            .await?;
        let weight = |archived: bool| {
            if query.include_archived == Some(true) || !archived {
                1
            } else {
                0
            }
        };
        for summary in pending {
            total += weight(summary.archived)
                - stored
                    .get(&summary.id)
                    .map_or(0, |old| weight(old.archived));
        }
        Ok(total)
    }

    async fn collect_authoritative(
        &self,
        workspace_ids: Option<&Vec<String>>,
    ) -> Result<Vec<SessionSummary>, WireAsyncError> {
        let primary_by_workspace = self.primary_by_workspace().await?;
        let mut collected = if self.read_model_enabled()
            && matches!(
                self.state.lock().state,
                SessionIndexState::Uninitialized | SessionIndexState::Preparing
            ) {
            let scan = self
                .projector
                .shared_scan_for_read()
                .await
                .map_err(|error| error.boxed())?;
            scan.summaries
                .into_iter()
                .filter(|summary| {
                    workspace_ids.is_none_or(|workspace_ids| {
                        workspace_ids.contains(&summary.workspace_id)
                    })
                })
                .collect()
        } else {
            let ids = match workspace_ids {
                Some(workspace_ids) => workspace_ids.clone(),
                None => list_workspace_ids(self.storage.as_ref(), &self.sessions_scope()).await,
            };
            let mut collected = Vec::new();
            for workspace_id in ids {
                for session_id in list_session_ids(
                    self.storage.as_ref(),
                    &self.sessions_scope(),
                    &workspace_id,
                )
                .await
                {
                    if let Some(summary) = read_session_summary(
                        self.docs.as_ref(),
                        &self.sessions_scope(),
                        &workspace_id,
                        &session_id,
                        primary_by_workspace.get(&workspace_id).map(String::as_str),
                    )
                    .await
                    {
                        collected.push(summary);
                    }
                }
            }
            collected
        };
        let pending = self.mirror.pending();
        if pending.is_empty() {
            return Ok(collected);
        }
        let mut by_id = collected
            .drain(..)
            .map(|summary| (summary.id.clone(), summary))
            .collect::<HashMap<_, _>>();
        for summary in pending {
            if workspace_ids.is_some_and(|workspace_ids| {
                !workspace_ids.contains(&summary.workspace_id)
            }) {
                continue;
            }
            by_id.insert(summary.id.clone(), summary);
        }
        Ok(by_id.into_values().collect())
    }

    fn read_model_enabled(&self) -> bool {
        database_base_enabled(self.config.as_ref())
    }
}

#[async_trait]
impl ISessionIndex for FileSessionIndex {
    async fn prepare(&self, options: Option<SessionIndexPrepareOptions>) -> SessionIndexStatus {
        FileSessionIndex::prepare(self, options).await
    }

    fn status(&self) -> SessionIndexStatus {
        FileSessionIndex::status(self)
    }

    async fn get(&self, id: &str) -> Result<Option<SessionSummary>, WireAsyncError> {
        FileSessionIndex::get(self, id).await
    }

    async fn list_recent(&self, query: SessionListQuery) -> Result<Page<SessionSummary>, WireAsyncError> {
        FileSessionIndex::list_recent(self, query).await
    }

    async fn count(&self, query: SessionCountQuery) -> Result<i64, WireAsyncError> {
        FileSessionIndex::count(self, query).await
    }

    async fn remove(&self, id: &str) -> Result<(), WireAsyncError> {
        FileSessionIndex::remove(self, id).await
    }
}

impl IDisposable for FileSessionIndex {
    fn dispose(&self) {
        self.stop_reconcile_loop();
        self.workspace_persistence_subscription.lock().take();
        self.subscriptions.lock().clear();
    }
}

fn canonical_order(a: &SessionSummary, b: &SessionSummary) -> std::cmp::Ordering {
    b.updated_at
        .cmp(&a.updated_at)
        .then_with(|| b.id.cmp(&a.id))
}

fn is_session_summary_shape(value: &Value) -> bool {
    value.as_object().is_some_and(|summary| {
        summary.get("id").is_some_and(Value::is_string)
            && summary.get("workspaceId").is_some_and(Value::is_string)
            && summary.get("createdAt").is_some_and(Value::is_number)
            && summary.get("updatedAt").is_some_and(Value::is_number)
            && summary.get("archived").is_some_and(Value::is_boolean)
    })
}

fn same_primary_by_workspace(
    left: Option<&HashMap<String, String>>,
    right: &HashMap<String, String>,
) -> bool {
    let Some(left) = left else {
        return false;
    };
    left.len() == right.len()
        && left
            .iter()
            .all(|(id, root)| right.get(id) == Some(root))
}
