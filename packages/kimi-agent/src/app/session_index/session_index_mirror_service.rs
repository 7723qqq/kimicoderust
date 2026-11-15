use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared, join_all};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::{Map, json};

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::log::ILogService;
use crate::_base::utils::timer::IntervalTimer;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::persistence::config_section::database_base_enabled;
use crate::persistence::interface::query_store::{IQueryStore, IQueryStoreExt, WriteOp};
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;

use super::session_index::{ISessionIndexMirror, SessionSummary};
use super::session_index_dirty_journal::mark_session_dirty;
use super::session_index_model::{
    SESSION_INDEX_MANIFEST, SessionWorkspaceCounts, recency_column, session_collection,
    session_counters_collection, with_recency_field,
};

const FLUSH_INTERVAL_MS: i64 = 100;
const FLUSH_BATCH_SIZE: usize = 500;
const MAX_PENDING: usize = 10_000;
const MAX_CONSECUTIVE_FAILURES: u64 = 5;

static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);
static PENDING_DRAINS: Lazy<Mutex<HashMap<u64, Shared<BoxFuture<'static, ()>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub async fn drain_session_index_mirror() {
    let drains = PENDING_DRAINS.lock().values().cloned().collect::<Vec<_>>();
    join_all(drains).await;
}

#[derive(Clone)]
struct PendingSummary {
    revision: u64,
    summary: SessionSummary,
}

#[derive(Default)]
struct MirrorState {
    pending_map: IndexMap<String, PendingSummary>,
    flushing: Option<Shared<BoxFuture<'static, ()>>>,
    consecutive_failures: u64,
    give_up_tracked: bool,
    overflow_logged: bool,
}

pub struct SessionIndexMirror {
    query_store: Arc<dyn IQueryStore>,
    config: Arc<dyn IConfigService>,
    telemetry: Arc<dyn ITelemetryService>,
    log: Arc<dyn ILogService>,
    storage: Arc<dyn IFileSystemStorageService>,
    state: Mutex<MirrorState>,
    pending_marks: Mutex<HashMap<u64, Shared<BoxFuture<'static, ()>>>>,
    timer: IntervalTimer,
    timer_set: AtomicBool,
    disposed: AtomicBool,
    sessions_scope: String,
    weak: Weak<Self>,
}

impl SessionIndexMirror {
    pub fn new(
        query_store: Arc<dyn IQueryStore>,
        config: Arc<dyn IConfigService>,
        telemetry: Arc<dyn ITelemetryService>,
        log: Arc<dyn ILogService>,
        storage: Arc<dyn IFileSystemStorageService>,
        bootstrap: Arc<dyn IBootstrapService>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            query_store,
            config,
            telemetry,
            log,
            storage,
            state: Mutex::new(MirrorState::default()),
            pending_marks: Mutex::new(HashMap::new()),
            timer: IntervalTimer::new(true),
            timer_set: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            sessions_scope: bootstrap.scope("sessions"),
            weak: weak.clone(),
        })
    }

    pub fn record(&self, summary: SessionSummary) -> Result<(), WireAsyncError> {
        if self.disposed.load(Ordering::Acquire) {
            return Ok(());
        }
        let mark_id = NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed);
        let storage = self.storage.clone();
        let sessions_scope = self.sessions_scope.clone();
        let session_id = summary.id.clone();
        let log = self.log.clone();
        let mark = async move {
            if let Err(error) = mark_session_dirty(
                storage.as_ref(),
                &sessions_scope,
                &session_id,
            )
            .await
            {
                log.debug(
                    "session index dirty mark failed",
                    Some(json!({ "error": error.to_string() })),
                );
            }
        }
        .boxed()
        .shared();
        self.pending_marks.lock().insert(mark_id, mark.clone());
        let weak = self.weak.clone();
        tokio::spawn(async move {
            mark.await;
            if let Some(this) = weak.upgrade() {
                this.pending_marks.lock().remove(&mark_id);
            }
        });
        if !database_base_enabled(self.config.as_ref()) {
            return Ok(());
        }
        let revision = NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed);
        let (should_flush, overflow) = {
            let mut state = self.state.lock();
            if state.pending_map.len() >= MAX_PENDING && !state.pending_map.contains_key(&summary.id) {
                let overflow = if state.overflow_logged {
                    None
                } else {
                    state.overflow_logged = true;
                    Some(state.pending_map.len())
                };
                (false, overflow)
            } else {
                state.overflow_logged = false;
                state.pending_map.insert(
                    summary.id.clone(),
                    PendingSummary { revision, summary },
                );
                (state.pending_map.len() >= FLUSH_BATCH_SIZE, None)
            }
        };
        if let Some(pending) = overflow {
            self.log.warn(
                "session index mirror queue full; dropping summaries until it drains",
                Some(json!({ "pending": pending })),
            );
            return Ok(());
        }
        if should_flush {
            self.spawn_flush();
        } else if !self.timer_set.load(Ordering::Acquire) {
            self.start_timer();
        }
        Ok(())
    }

    pub fn pending(&self) -> Vec<SessionSummary> {
        self.state
            .lock()
            .pending_map
            .values()
            .map(|pending| pending.summary.clone())
            .collect()
    }

    pub async fn evict(&self, id: &str) -> Result<(), WireAsyncError> {
        self.state.lock().pending_map.shift_remove(id);
        let flushing = self.state.lock().flushing.clone();
        if let Some(flushing) = flushing {
            flushing.await;
        }
        self.state.lock().pending_map.shift_remove(id);
        Ok(())
    }

    pub async fn drain(&self) -> Result<(), WireAsyncError> {
        self.timer.dispose();
        self.timer_set.store(false, Ordering::Release);
        loop {
            let before = self.state.lock().pending_map.len();
            if before == 0 {
                return Ok(());
            }
            self.flush().await;
            let pending = self.state.lock().pending_map.len();
            if pending >= before {
                self.log.warn(
                    "session index mirror drain made no progress; leaving the rest dirty",
                    Some(json!({ "pending": pending })),
                );
                return Ok(());
            }
        }
    }

    fn start_timer(&self) {
        self.timer_set.store(true, Ordering::Release);
        let weak = self.weak.clone();
        self.timer.cancel_and_set(
            move || {
                if let Some(this) = weak.upgrade() {
                    this.spawn_flush();
                }
            },
            FLUSH_INTERVAL_MS,
        );
    }

    fn spawn_flush(&self) {
        let flush = self.flush();
        tokio::spawn(flush);
    }

    fn flush(&self) -> Shared<BoxFuture<'static, ()>> {
        if let Some(flushing) = self.state.lock().flushing.clone() {
            return flushing;
        }
        let this = self
            .weak
            .upgrade()
            .expect("session index mirror weak self is unavailable");
        let flushing = {
            let this = this.clone();
            async move {
                this.flush_chunk().await;
                let restart = {
                    let mut state = this.state.lock();
                    state.flushing = None;
                    !state.pending_map.is_empty()
                        && state.consecutive_failures < MAX_CONSECUTIVE_FAILURES
                };
                if restart {
                    this.start_timer();
                }
            }
            .boxed()
            .shared()
        };
        self.state.lock().flushing = Some(flushing.clone());
        flushing
    }

    async fn flush_chunk(&self) {
        let chunk = self
            .state
            .lock()
            .pending_map
            .iter()
            .take(FLUSH_BATCH_SIZE)
            .map(|(id, pending)| (id.clone(), pending.revision, pending.summary.clone()))
            .collect::<Vec<_>>();
        if chunk.is_empty() {
            return;
        }
        let manifest = match self
            .query_store
            .get_checkpoint(SESSION_INDEX_MANIFEST)
            .await
        {
            Ok(Some(manifest)) => manifest,
            Ok(None) => {
                self.state.lock().consecutive_failures += 1;
                return;
            }
            Err(error) => {
                self.flush_failed(&error);
                return;
            }
        };
        let result = async {
            let collection = session_collection(manifest.seq);
            let counters = session_counters_collection(manifest.seq);
            let ids = chunk
                .iter()
                .map(|(id, _, _)| id.clone())
                .collect::<Vec<_>>();
            let olds = self
                .query_store
                .get_many::<SessionSummary>(&collection, &ids)
                .await?;
            let mut deltas = HashMap::<String, SessionWorkspaceCounts>::new();
            for (id, _, summary) in &chunk {
                match olds.get(id) {
                    None => bump_count(
                        &mut deltas,
                        &summary.workspace_id,
                        summary.archived,
                        1,
                    ),
                    Some(old) if old.workspace_id != summary.workspace_id => {
                        bump_count(&mut deltas, &old.workspace_id, old.archived, -1);
                        bump_count(
                            &mut deltas,
                            &summary.workspace_id,
                            summary.archived,
                            1,
                        );
                    }
                    Some(old) if old.archived != summary.archived => {
                        bump_count(&mut deltas, &summary.workspace_id, old.archived, -1);
                        bump_count(
                            &mut deltas,
                            &summary.workspace_id,
                            summary.archived,
                            1,
                        );
                    }
                    _ => {}
                }
            }
            let current = self
                .query_store
                .get_many::<SessionWorkspaceCounts>(
                    &counters,
                    &deltas.keys().cloned().collect::<Vec<_>>(),
                )
                .await?;
            let mut ops = chunk
                .iter()
                .map(|(id, _, summary)| {
                    Ok(WriteOp::Put {
                        collection: collection.clone(),
                        key: id.clone(),
                        value: serde_json::to_value(with_recency_field(
                            manifest.seq,
                            summary.clone(),
                        ))?,
                        columns: Some(HashMap::from([(
                            recency_column(manifest.seq),
                            summary.updated_at as f64,
                        )])),
                    })
                })
                .collect::<Result<Vec<_>, WireAsyncError>>()?;
            for (workspace_id, delta) in deltas {
                let base = current.get(&workspace_id).copied().unwrap_or_default();
                let value = SessionWorkspaceCounts {
                    active: (base.active + delta.active).max(0),
                    archived: (base.archived + delta.archived).max(0),
                };
                ops.push(WriteOp::Put {
                    collection: counters.clone(),
                    key: workspace_id,
                    value: serde_json::to_value(value)?,
                    columns: None,
                });
            }
            self.query_store.batch(ops).await
        }
        .await;
        if let Err(error) = result {
            self.flush_failed(&error);
            return;
        }
        let recovered = {
            let mut state = self.state.lock();
            for (id, revision, _) in &chunk {
                if state
                    .pending_map
                    .get(id)
                    .is_some_and(|pending| pending.revision == *revision)
                {
                    state.pending_map.shift_remove(id);
                }
            }
            let recovered = state.consecutive_failures > 0;
            let after_failures = state.consecutive_failures;
            state.consecutive_failures = 0;
            state.give_up_tracked = false;
            (recovered, after_failures, state.pending_map.len())
        };
        let (recovered, after_failures, pending) = recovered;
        if recovered {
            self.log.info(
                "session index mirror flush recovered",
                Some(json!({
                    "afterFailures": after_failures,
                    "pending": pending,
                })),
            );
        }
    }

    fn flush_failed(&self, error: &WireAsyncError) {
        let (failures, pending, track_give_up) = {
            let mut state = self.state.lock();
            state.consecutive_failures += 1;
            let failures = state.consecutive_failures;
            let track_give_up = failures >= MAX_CONSECUTIVE_FAILURES && !state.give_up_tracked;
            if track_give_up {
                state.give_up_tracked = true;
            }
            (failures, state.pending_map.len(), track_give_up)
        };
        if failures == 1 {
            self.log.warn(
                "failed to flush session index mirror chunk",
                Some(json!({
                    "pending": pending,
                    "error": error.to_string(),
                })),
            );
        }
        if failures >= MAX_CONSECUTIVE_FAILURES {
            self.log.warn(
                "session index mirror giving up until the next record; reconciliation will heal",
                Some(json!({
                    "pending": pending,
                    "failures": failures,
                })),
            );
            if track_give_up {
                let properties = Map::from_iter([
                    ("pending_count".to_string(), (pending as u64).into()),
                    ("consecutive_failures".to_string(), failures.into()),
                ]);
                self.telemetry
                    .track2("session_index_mirror_give_up", Some(properties));
            }
        }
    }
}

#[async_trait]
impl ISessionIndexMirror for SessionIndexMirror {
    fn record(&self, summary: SessionSummary) -> Result<(), WireAsyncError> {
        SessionIndexMirror::record(self, summary)
    }

    fn pending(&self) -> Vec<SessionSummary> {
        SessionIndexMirror::pending(self)
    }

    async fn evict(&self, id: &str) -> Result<(), WireAsyncError> {
        SessionIndexMirror::evict(self, id).await
    }

    async fn drain(&self) -> Result<(), WireAsyncError> {
        SessionIndexMirror::drain(self).await
    }
}

impl IDisposable for SessionIndexMirror {
    fn dispose(&self) {
        if self.disposed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.timer.dispose();
        self.timer_set.store(false, Ordering::Release);
        let marks = self.pending_marks.lock().values().cloned().collect::<Vec<_>>();
        let Some(this) = self.weak.upgrade() else {
            return;
        };
        let id = NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed);
        let pending = async move {
            join_all(marks).await;
            let _ = this.drain().await;
        }
        .boxed()
        .shared();
        PENDING_DRAINS.lock().insert(id, pending.clone());
        tokio::spawn(async move {
            pending.await;
            PENDING_DRAINS.lock().remove(&id);
        });
    }
}

fn bump_count(
    deltas: &mut HashMap<String, SessionWorkspaceCounts>,
    workspace_id: &str,
    archived: bool,
    by: i64,
) {
    let entry = deltas.entry(workspace_id.to_string()).or_default();
    if archived {
        entry.archived += by;
    } else {
        entry.active += by;
    }
}
