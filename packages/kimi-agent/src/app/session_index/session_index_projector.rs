use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde_json::json;

use crate::_base::log::log::ILogService;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::query_store::{
    Checkpoint, IQueryStore, IQueryStoreExt, IndexDef, WriteOp,
};
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;

use super::session_index::{PARENT_SESSION_ID_KEY, SessionSummary};
use super::session_index_dirty_journal::{
    clear_dirty_marks, dirty_mark_session_ids, list_dirty_marks,
};
use super::session_index_model::{
    PARENT_INDEX_NAME, SESSION_INDEX_MANIFEST, SESSION_INDEX_SCHEMA_VERSION,
    SessionWorkspaceCounts, recency_column, session_collection, session_counters_collection,
    with_recency_field,
};
use super::session_index_source::{
    SessionScanCacheEntry, list_session_ids, list_workspace_ids, map_bounded,
    read_session_scan_cache, read_session_summary, stat_session_state_file, summary_equals,
    write_session_scan_cache,
};

const WRITE_CHUNK: usize = 4096;
const SCAN_CONCURRENCY: usize = 16;
const STAT_CONCURRENCY: usize = 64;
const SHARED_SCAN_REUSE_MS: i64 = 30_000;

pub type PrimaryByWorkspace = Arc<
    dyn Fn() -> BoxFuture<'static, Result<HashMap<String, String>, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct SessionIndexProjectorDeps {
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub docs: Arc<dyn IAtomicDocumentStore>,
    pub query_store: Arc<dyn IQueryStore>,
    pub log: Arc<dyn ILogService>,
    pub sessions_scope: String,
    pub primary_by_workspace: PrimaryByWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionResult {
    pub generation: u64,
    pub sessions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    pub sessions: usize,
    pub upserted: usize,
    pub removed: usize,
}

#[derive(Debug, Clone)]
pub struct AuthoritativeScan {
    pub summaries: Vec<SessionSummary>,
    pub counts: HashMap<String, SessionWorkspaceCounts>,
    pub source_session_count: u64,
    pub workspace_signals: Option<HashMap<String, u64>>,
    pub primary_by_workspace: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct SharedScanError(Arc<String>);

impl SharedScanError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    pub fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

impl Error for SharedScanError {}

type ScanFuture = Shared<BoxFuture<'static, Result<AuthoritativeScan, SharedScanError>>>;

#[derive(Clone)]
struct ScanSlot {
    id: u64,
    promise: ScanFuture,
    reusable_until: i64,
    settled: Arc<AtomicBool>,
}

struct SessionCheck {
    session_id: String,
    stat: super::session_index_source::SessionStateFileStat,
    cached: Option<SessionScanCacheEntry>,
}

struct ScanFound {
    cache_entry: SessionScanCacheEntry,
    summary: SessionSummary,
}

struct SessionRead {
    session_id: String,
    summary: Option<SessionSummary>,
}

static NEXT_SCAN_SLOT_ID: AtomicU64 = AtomicU64::new(1);

pub struct SessionIndexProjector {
    deps: Arc<SessionIndexProjectorDeps>,
    scan_slot: Mutex<Option<ScanSlot>>,
}

impl SessionIndexProjector {
    pub fn new(deps: SessionIndexProjectorDeps) -> Arc<Self> {
        Arc::new(Self {
            deps: Arc::new(deps),
            scan_slot: Mutex::new(None),
        })
    }

    pub fn shared_scan(&self) -> ScanFuture {
        self.scan_slot_for(false).promise
    }

    pub fn shared_scan_for_read(&self) -> ScanFuture {
        self.scan_slot_for(true).promise
    }

    fn scan_slot_for(&self, read_only_in_flight: bool) -> ScanSlot {
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(slot) = self.scan_slot.lock().as_ref().cloned()
            && (!slot.settled.load(Ordering::Acquire)
                || (!read_only_in_flight && now < slot.reusable_until))
        {
            return slot;
        }
        self.start_scan()
    }

    fn start_scan(&self) -> ScanSlot {
        let settled = Arc::new(AtomicBool::new(false));
        let task_settled = settled.clone();
        let deps = self.deps.clone();
        let promise = async move {
            let result = Self::scan_authoritative(deps)
                .await
                .map_err(SharedScanError::new);
            task_settled.store(true, Ordering::Release);
            result
        }
        .boxed()
        .shared();
        let slot = ScanSlot {
            id: NEXT_SCAN_SLOT_ID.fetch_add(1, Ordering::Relaxed),
            promise,
            reusable_until: chrono::Utc::now().timestamp_millis() + SHARED_SCAN_REUSE_MS,
            settled,
        };
        *self.scan_slot.lock() = Some(slot.clone());
        slot
    }

    pub async fn project(&self, generation: u64) -> Result<ProjectionResult, WireAsyncError> {
        let slot = self.scan_slot_for(false);
        let result = self.do_project(generation, slot.promise.clone()).await;
        let mut current = self.scan_slot.lock();
        if current.as_ref().is_some_and(|current| current.id == slot.id) {
            *current = None;
        }
        drop(current);
        result
    }

    async fn do_project(
        &self,
        generation: u64,
        scan: ScanFuture,
    ) -> Result<ProjectionResult, WireAsyncError> {
        let epoch = self.deps.query_store.store_epoch();
        let collection = session_collection(generation);
        let counters = session_counters_collection(generation);
        self.deps.query_store.drop_collection(&collection).await?;
        self.deps.query_store.drop_collection(&counters).await?;
        self.deps
            .query_store
            .ensure_index(
                &collection,
                IndexDef::Value {
                    name: PARENT_INDEX_NAME.to_string(),
                    field: format!("custom.{PARENT_SESSION_ID_KEY}"),
                    unique: None,
                },
            )
            .await?;
        let scan = scan.await.map_err(SharedScanError::boxed)?;
        let AuthoritativeScan {
            summaries,
            counts,
            source_session_count,
            workspace_signals,
            primary_by_workspace,
        } = scan;
        let ops = summaries
            .iter()
            .cloned()
            .map(|summary| {
                Ok(WriteOp::Put {
                    collection: collection.clone(),
                    key: summary.id.clone(),
                    value: serde_json::to_value(with_recency_field(
                        generation,
                        summary.clone(),
                    ))?,
                    columns: Some(HashMap::from([(
                        recency_column(generation),
                        summary.updated_at as f64,
                    )])),
                })
            })
            .collect::<Result<Vec<_>, WireAsyncError>>()?;
        self.batch_chunks(&ops).await?;
        self.write_counters(&counters, &counts).await?;
        self.deps
            .query_store
            .set_checkpoint(
                SESSION_INDEX_MANIFEST,
                Checkpoint {
                    seq: generation,
                    source_session_count: Some(source_session_count),
                    schema_version: Some(SESSION_INDEX_SCHEMA_VERSION),
                    workspace_signals,
                    workspace_primaries: Some(primary_by_workspace),
                },
                Some(epoch),
            )
            .await?;
        self.deps.log.info(
            "session index generation published",
            Some(json!({
                "generation": generation,
                "sessions": summaries.len(),
            })),
        );
        if generation > 1 {
            let query_store = self.deps.query_store.clone();
            let log = self.deps.log.clone();
            let stale_session = session_collection(generation - 1);
            let stale_counters = session_counters_collection(generation - 1);
            tokio::spawn(async move {
                let result = async {
                    query_store.drop_collection(&stale_session).await?;
                    query_store.drop_collection(&stale_counters).await
                }
                .await;
                if let Err(error) = result {
                    log.warn(
                        "failed to drop previous session index generation",
                        Some(json!({
                            "generation": generation - 1,
                            "error": error.to_string(),
                        })),
                    );
                }
            });
        }
        Ok(ProjectionResult {
            generation,
            sessions: summaries.len(),
        })
    }

    pub async fn reconcile(&self, generation: u64) -> Result<ReconcileResult, WireAsyncError> {
        let epoch = self.deps.query_store.store_epoch();
        let collection = session_collection(generation);
        let counters = session_counters_collection(generation);
        let marks = list_dirty_marks(
            self.deps.storage.as_ref(),
            &self.deps.sessions_scope,
        )
        .await?;
        let mut changed = dirty_mark_session_ids(&marks);
        let workspace_ids = list_workspace_ids(
            self.deps.storage.as_ref(),
            &self.deps.sessions_scope,
        )
        .await;
        let primary_by_workspace = (self.deps.primary_by_workspace)().await?;
        let mut authoritative = HashMap::new();
        let mut workspace_signals = HashMap::new();
        let mut signals_complete = true;
        for workspace_id in workspace_ids {
            match self
                .deps
                .storage
                .mtime(&self.deps.sessions_scope, &workspace_id)
                .await?
            {
                Some(signal) if signal >= 0 => {
                    workspace_signals.insert(workspace_id.clone(), signal as u64);
                }
                _ => signals_complete = false,
            }
            for session_id in list_session_ids(
                self.deps.storage.as_ref(),
                &self.deps.sessions_scope,
                &workspace_id,
            )
            .await
            {
                authoritative.insert(session_id, workspace_id.clone());
            }
        }
        let stored_keys = self.deps.query_store.list_keys(&collection).await?;
        let stored = stored_keys.iter().cloned().collect::<HashSet<_>>();
        for session_id in authoritative.keys() {
            if !stored.contains(session_id) {
                changed.insert(session_id.clone());
            }
        }
        for key in &stored_keys {
            if !authoritative.contains_key(key) {
                changed.insert(key.clone());
            }
        }
        let changed = changed.into_iter().collect::<Vec<_>>();
        let olds = self
            .deps
            .query_store
            .get_many::<SessionSummary>(&collection, &changed)
            .await?;
        let docs = self.deps.docs.clone();
        let sessions_scope = self.deps.sessions_scope.clone();
        let reads = map_bounded(changed, SCAN_CONCURRENCY, |session_id| {
            let workspace_id = authoritative.get(&session_id).cloned();
            let primary_dir = workspace_id
                .as_ref()
                .and_then(|workspace_id| primary_by_workspace.get(workspace_id))
                .cloned();
            let docs = docs.clone();
            let sessions_scope = sessions_scope.clone();
            async move {
                let summary = match workspace_id {
                    Some(workspace_id) => {
                        read_session_summary(
                            docs.as_ref(),
                            &sessions_scope,
                            &workspace_id,
                            &session_id,
                            primary_dir.as_deref(),
                        )
                        .await
                    }
                    None => None,
                };
                Some(SessionRead {
                    session_id,
                    summary,
                })
            }
        })
        .await;
        let mut upserts = Vec::new();
        let mut removals = Vec::new();
        let mut applied = HashMap::new();
        let mut removed = HashSet::new();
        for read in reads {
            let Some(summary) = read.summary else {
                if olds.contains_key(&read.session_id) {
                    removals.push(WriteOp::Delete {
                        collection: collection.clone(),
                        key: read.session_id.clone(),
                    });
                    removed.insert(read.session_id);
                }
                continue;
            };
            let existing = olds.get(&read.session_id);
            if existing.is_none_or(|existing| !summary_equals(existing, &summary)) {
                upserts.push(WriteOp::Put {
                    collection: collection.clone(),
                    key: read.session_id.clone(),
                    value: serde_json::to_value(with_recency_field(
                        generation,
                        summary.clone(),
                    ))?,
                    columns: Some(HashMap::from([(
                        recency_column(generation),
                        summary.updated_at as f64,
                    )])),
                });
            }
            applied.insert(read.session_id, summary);
        }
        let mut deltas = HashMap::<String, SessionWorkspaceCounts>::new();
        for summary in applied.values() {
            match olds.get(&summary.id) {
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
        for session_id in &removed {
            if let Some(old) = olds.get(session_id) {
                bump_count(&mut deltas, &old.workspace_id, old.archived, -1);
            }
        }
        let delta_keys = deltas.keys().cloned().collect::<Vec<_>>();
        let current = self
            .deps
            .query_store
            .get_many::<SessionWorkspaceCounts>(&counters, &delta_keys)
            .await?;
        let mut counter_ops = Vec::new();
        for (workspace_id, delta) in &deltas {
            let base = current.get(workspace_id).copied().unwrap_or_default();
            let value = SessionWorkspaceCounts {
                active: (base.active + delta.active).max(0),
                archived: (base.archived + delta.archived).max(0),
            };
            counter_ops.push(WriteOp::Put {
                collection: counters.clone(),
                key: workspace_id.clone(),
                value: serde_json::to_value(value)?,
                columns: None,
            });
        }
        let mut totals = HashMap::<String, usize>::new();
        for workspace_id in authoritative.values() {
            *totals.entry(workspace_id.clone()).or_default() += 1;
        }
        for key in self.deps.query_store.list_keys(&counters).await? {
            if !totals.contains_key(&key) {
                counter_ops.push(WriteOp::Delete {
                    collection: counters.clone(),
                    key,
                });
            }
        }
        let ops = [upserts, removals, counter_ops].concat();
        self.batch_chunks(&ops).await?;
        let manifest = self
            .deps
            .query_store
            .get_checkpoint(SESSION_INDEX_MANIFEST)
            .await?;
        if manifest.as_ref().is_some_and(|manifest| manifest.seq == generation) {
            self.deps
                .query_store
                .set_checkpoint(
                    SESSION_INDEX_MANIFEST,
                    Checkpoint {
                        seq: generation,
                        source_session_count: Some(authoritative.len() as u64),
                        schema_version: Some(SESSION_INDEX_SCHEMA_VERSION),
                        workspace_signals: signals_complete.then_some(workspace_signals),
                        workspace_primaries: Some(primary_by_workspace),
                    },
                    Some(epoch),
                )
                .await?;
        }
        clear_dirty_marks(
            self.deps.storage.as_ref(),
            &self.deps.sessions_scope,
            &marks,
        )
        .await;
        let result = ReconcileResult {
            sessions: authoritative.len(),
            upserted: upserted_count(&ops, &collection),
            removed: removals_count(&ops, &collection),
        };
        if result.upserted > 0 || result.removed > 0 {
            self.deps.log.info(
                "session index reconciliation repaired drift",
                Some(json!({
                    "generation": generation,
                    "sessions": result.sessions,
                    "upserted": result.upserted,
                    "removed": result.removed,
                })),
            );
        }
        Ok(result)
    }

    async fn scan_authoritative(
        deps: Arc<SessionIndexProjectorDeps>,
    ) -> Result<AuthoritativeScan, WireAsyncError> {
        let primary_by_workspace = (deps.primary_by_workspace)().await?;
        let cache = read_session_scan_cache(deps.storage.as_ref(), &deps.sessions_scope).await;
        let mut next_cache = HashMap::new();
        let mut summaries = Vec::new();
        let mut counts = HashMap::<String, SessionWorkspaceCounts>::new();
        let mut workspace_signals = HashMap::new();
        let mut signals_complete = true;
        let mut source_session_count = 0_u64;
        for workspace_id in list_workspace_ids(deps.storage.as_ref(), &deps.sessions_scope).await {
            match deps
                .storage
                .mtime(&deps.sessions_scope, &workspace_id)
                .await?
            {
                Some(signal) if signal >= 0 => {
                    workspace_signals.insert(workspace_id.clone(), signal as u64);
                }
                _ => signals_complete = false,
            }
            let session_ids = list_session_ids(
                deps.storage.as_ref(),
                &deps.sessions_scope,
                &workspace_id,
            )
            .await;
            source_session_count += session_ids.len() as u64;
            let primary_dir = primary_by_workspace.get(&workspace_id).cloned();
            let checks = map_bounded(session_ids, STAT_CONCURRENCY, |session_id| {
                let cached = cache.get(&session_id).cloned();
                let storage = deps.storage.clone();
                let sessions_scope = deps.sessions_scope.clone();
                let workspace_id = workspace_id.clone();
                let primary_dir = primary_dir.clone();
                async move {
                    let stat = stat_session_state_file(
                        storage.as_ref(),
                        &sessions_scope,
                        &workspace_id,
                        &session_id,
                    )
                    .await?;
                    let hit = cached.as_ref().is_some_and(|cached| {
                        cached.ws == workspace_id
                            && cached.meta == stat.meta
                            && cached.mtime_ms == stat.mtime_ms
                            && cached.size == stat.size
                            && cached.primary_dir == primary_dir
                    });
                    Some(SessionCheck {
                        session_id,
                        stat,
                        cached: if hit { cached } else { None },
                    })
                }
            })
            .await;
            let mut workspace_count = counts.get(&workspace_id).copied().unwrap_or_default();
            let found = map_bounded(checks, SCAN_CONCURRENCY, |check| {
                let docs = deps.docs.clone();
                let sessions_scope = deps.sessions_scope.clone();
                let workspace_id = workspace_id.clone();
                let primary_dir = primary_dir.clone();
                async move {
                    if let Some(cached) = check.cached {
                        return Some(ScanFound {
                            summary: cached.summary.clone(),
                            cache_entry: cached,
                        });
                    }
                    let summary = read_session_summary(
                        docs.as_ref(),
                        &sessions_scope,
                        &workspace_id,
                        &check.session_id,
                        primary_dir.as_deref(),
                    )
                    .await?;
                    let cache_entry = SessionScanCacheEntry {
                        ws: workspace_id,
                        meta: check.stat.meta,
                        mtime_ms: check.stat.mtime_ms,
                        size: check.stat.size,
                        primary_dir,
                        summary: summary.clone(),
                    };
                    Some(ScanFound {
                        cache_entry,
                        summary,
                    })
                }
            })
            .await;
            for found_entry in found {
                next_cache.insert(found_entry.cache_entry.summary.id.clone(), found_entry.cache_entry);
                if found_entry.summary.archived {
                    workspace_count.archived += 1;
                } else {
                    workspace_count.active += 1;
                }
                summaries.push(found_entry.summary);
            }
            counts.insert(workspace_id, workspace_count);
        }
        if let Err(error) = write_session_scan_cache(
            deps.storage.as_ref(),
            &deps.sessions_scope,
            &next_cache,
        )
        .await
        {
            deps.log.warn(
                "session index scan cache write failed",
                Some(json!({ "error": error.to_string() })),
            );
        }
        Ok(AuthoritativeScan {
            summaries,
            counts,
            source_session_count,
            workspace_signals: signals_complete.then_some(workspace_signals),
            primary_by_workspace,
        })
    }

    async fn write_counters(
        &self,
        counters: &str,
        counts: &HashMap<String, SessionWorkspaceCounts>,
    ) -> Result<(), WireAsyncError> {
        let mut ops = counts
            .iter()
            .map(|(workspace_id, value)| {
                Ok(WriteOp::Put {
                    collection: counters.to_string(),
                    key: workspace_id.clone(),
                    value: serde_json::to_value(value)?,
                    columns: None,
                })
            })
            .collect::<Result<Vec<_>, WireAsyncError>>()?;
        for key in self.deps.query_store.list_keys(counters).await? {
            if !counts.contains_key(&key) {
                ops.push(WriteOp::Delete {
                    collection: counters.to_string(),
                    key,
                });
            }
        }
        self.batch_chunks(&ops).await
    }

    async fn batch_chunks(&self, ops: &[WriteOp]) -> Result<(), WireAsyncError> {
        for chunk in ops.chunks(WRITE_CHUNK) {
            self.deps.query_store.batch(chunk.to_vec()).await?;
        }
        Ok(())
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

fn upserted_count(ops: &[WriteOp], collection: &str) -> usize {
    ops.iter()
        .filter(|op| {
            matches!(op, WriteOp::Put { collection: current, .. } if current == collection)
        })
        .count()
}

fn removals_count(ops: &[WriteOp], collection: &str) -> usize {
    ops.iter()
        .filter(|op| {
            matches!(op, WriteOp::Delete { collection: current, .. } if current == collection)
        })
        .count()
}
