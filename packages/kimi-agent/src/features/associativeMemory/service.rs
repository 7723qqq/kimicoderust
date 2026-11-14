use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::fs;
use ulid::Ulid;

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::injection::rendered_lines::{already_rendered, mark_rendered};
use crate::features::associativeMemory::activate::activate;
use crate::features::associativeMemory::accessibility::accessibility;
use crate::features::associativeMemory::activity::recent_activity;
use crate::features::associativeMemory::audit::{AuditEntry, AuditKind, append_audit, audit_path, read_audit};
use crate::features::associativeMemory::cluster::{cluster_incremental, cooc_cluster_threshold};
use crate::features::associativeMemory::code::defaults::default_code;
use crate::features::associativeMemory::code::load::load_code_file;
use crate::features::associativeMemory::code::proposals::{approve_proposal, propose_rule};
use crate::features::associativeMemory::code::{
    apply_write_rules, code_allows_embeddings, filter_recall_items, order_conflict_groups,
};
use crate::features::associativeMemory::decay::{
    apply_decay, apply_motivated_forgetting, apply_sleep_downscale, prune_cooccurrences,
    prune_edges, restore_captured,
};
use crate::features::associativeMemory::dedupe::note_fingerprint;
use crate::features::associativeMemory::embeddings::{
    EmbeddingsClient, HttpEmbeddingsClient, resolve_embeddings_options,
};
use crate::features::associativeMemory::encode::encode_atoms;
use crate::features::associativeMemory::flag::{ASSOCIATIVE_MEMORY_FLAG_ENV, ASSOCIATIVE_MEMORY_FLAG_ID};
use crate::features::associativeMemory::gating::{GatingInput, GatingMode, classify_gating};
use crate::features::associativeMemory::metacognition::update_confidence;
use crate::features::associativeMemory::params::associative_memory_params;
use crate::features::associativeMemory::procedure::distill_procedures;
use crate::features::associativeMemory::render::{MEMORY_DATA_BOUNDARY, render_recall};
use crate::features::associativeMemory::scheduler::{interleave, rank_by_need_gain};
use crate::features::associativeMemory::schema::collect_schema_links;
use crate::features::associativeMemory::separation::{
    MemorySeparationDecision, memory_separation_decision, shares_content_term,
};
use crate::features::associativeMemory::store::AssociativeMemoryStore;
use crate::features::associativeMemory::terms::compare_by_text;
use crate::features::associativeMemory::train::hebbian_connect;
use crate::features::associativeMemory::types::{
    Atom, Cooccurrence, Edge, EdgeKind, HardRule, ListOptions, MemoryCode, MemoryScopeSelector,
    MemorySource, NoteRecord, RecallDrop, RecallItem, RecallQuery, RecallResult, RecallTraceNode,
    ReconsolidateResult, ReplayResult, WriteDecision, WriteResult, AssociativeMemoryExport,
    AssociativeMemoryList, AssociativeMemoryStats,
};
use crate::features::associativeMemory::vector::{compute_df, cosine};
use crate::features::associativeMemory::workingBuffer::WorkingBuffer;
use crate::tools::project_identity::{legacy_project_id_from_cwd, resolve_project_identity};

const STORE_ROOT_NAME: &str = "associative-memory";
const CODE_FILE_NAME: &str = "code.md";
const GLOBAL_SCOPE_KEY: &str = "global";
const RECONSOLIDATE_THRESHOLD: f64 = 0.5;
const EMBEDDINGS_RETRY_MS: i64 = 60_000;
const MAINTAIN_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;
const MAINTAIN_EVERY_WRITES: i64 = 8;
const CONFIDENCE_RATE: f64 = 0.2;
const COOC_CONTEXT_LIMIT: usize = 24;
const ACTIVATION_COOC_LIMIT: usize = 8;
const AUDIT_ID_LIMIT: usize = 50;
const CONSOLIDATION_POOL: usize = 16;
const CONSOLIDATION_PAIR_BUDGET: usize = 96;
const MAX_ATOMS: usize = 4000;
const AUDIT_TEXT_LIMIT: usize = 400;
const MAX_NOTES: usize = 8000;
const MAX_OPEN_STORES: usize = 8;
const WORKING_CAPACITY: usize = 24;
const WORKING_TTL_MS: i64 = 6 * 60 * 60 * 1000;
const ASSOCIATIVE_MEMORY_SIGNAL_VARIANT: &str = "associative_memory_signal";

pub trait MemoryLogger: Send + Sync {
    fn warn(&self, message: &str, data: &Value);
    fn debug(&self, message: &str, data: &Value);
}

pub trait MemoryFlags: Send + Sync {
    fn enabled(&self, id: &str) -> bool;
}

pub trait MemoryReminders: Send + Sync {
    fn register(
        &self,
        variant: &str,
        provider: Arc<
            dyn Fn(bool) -> crate::rpc::types::BoxFuture<'static, Option<String>> + Send + Sync,
        >,
    );
}

#[derive(Debug, Clone)]
pub struct LegacyMemoryEntry {
    pub path: String,
    pub scope: String,
    pub body: String,
}

pub trait MemoryLegacyStore: Send + Sync {
    fn list(&self) -> Vec<String>;
    fn get(&self, path: &str) -> Option<LegacyMemoryEntry>;
    fn set_read_only(&self, value: bool);
}

pub struct EnvMemoryFlags;

impl MemoryFlags for EnvMemoryFlags {
    fn enabled(&self, id: &str) -> bool {
        id == ASSOCIATIVE_MEMORY_FLAG_ID
            && crate::env::env_switch_default_off(ASSOCIATIVE_MEMORY_FLAG_ENV)
    }
}

pub struct MemoryDeps {
    pub home_dir: PathBuf,
    pub store_dir: PathBuf,
    pub cwd: PathBuf,
    pub session_id: String,
    pub session_cwd: PathBuf,
    pub logger: Option<Arc<dyn MemoryLogger>>,
    pub flags: Option<Arc<dyn MemoryFlags>>,
    pub reminders: Option<Arc<dyn MemoryReminders>>,
    pub history: Option<Arc<dyn IAgentContextMemoryService>>,
    pub legacy_store: Option<Arc<dyn MemoryLegacyStore>>,
}

struct ServiceState {
    code: MemoryCode,
    code_loaded: bool,
    code_mtime_ms: f64,
    migrated_project_scope: bool,
    legacy_memory_imported: bool,
    embeddings_client: Option<Arc<dyn EmbeddingsClient>>,
    embeddings_resolved: bool,
    embeddings_resolved_at: i64,
    embeddings_blocked_logged: bool,
    last_maintained_at: i64,
    last_write_text: Option<String>,
    last_recall_cue: Option<String>,
    last_gating_mode: GatingMode,
    writes_since_maintain: i64,
    last_recall_ids: Vec<String>,
    previous_recall_ids: Vec<String>,
    working: WorkingBuffer,
}

impl ServiceState {
    fn new() -> Self {
        Self {
            code: default_code(),
            code_loaded: false,
            code_mtime_ms: 0.0,
            migrated_project_scope: false,
            legacy_memory_imported: false,
            embeddings_client: None,
            embeddings_resolved: false,
            embeddings_resolved_at: 0,
            embeddings_blocked_logged: false,
            last_maintained_at: 0,
            last_write_text: None,
            last_recall_cue: None,
            last_gating_mode: GatingMode::Retrieve,
            writes_since_maintain: 0,
            last_recall_ids: Vec::new(),
            previous_recall_ids: Vec::new(),
            working: WorkingBuffer::new(WORKING_CAPACITY, WORKING_TTL_MS),
        }
    }
}

type SharedStore = Arc<tokio::sync::Mutex<AssociativeMemoryStore>>;

struct StoreCacheEntry {
    store: SharedStore,
    refs: usize,
    idle_since: Option<i64>,
}

pub struct StoreGuard {
    key: String,
    store: SharedStore,
    cache: Arc<Mutex<HashMap<String, StoreCacheEntry>>>,
}

impl StoreGuard {
    async fn lock(&self) -> tokio::sync::MutexGuard<'_, AssociativeMemoryStore> {
        self.store.lock().await
    }
}

impl Drop for StoreGuard {
    fn drop(&mut self) {
        let mut cache = self.cache.lock().unwrap();
        if let Some(entry) = cache.get_mut(&self.key) {
            entry.refs = entry.refs.saturating_sub(1);
            if entry.refs == 0 {
                entry.idle_since = Some(system_now_ms());
            }
        }
    }
}

pub struct AssociativeMemoryService {
    deps: MemoryDeps,
    state: tokio::sync::Mutex<ServiceState>,
    stores: Arc<Mutex<HashMap<String, StoreCacheEntry>>>,
    queue: tokio::sync::Mutex<()>,
    now_override: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    dispose: AtomicBool,
}

fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn most_recent(atoms: &[Atom], limit: usize) -> Vec<Atom> {
    let mut kept: Vec<Atom> = Vec::new();
    for atom in atoms {
        let mut index = kept.len();
        while index > 0 {
            let previous = &kept[index - 1];
            let newer = atom.last_activated_at > previous.last_activated_at
                || (atom.last_activated_at == previous.last_activated_at
                    && compare_by_text(
                        (atom.id.as_str(), atom.text.as_str()),
                        (previous.id.as_str(), previous.text.as_str()),
                    ) == std::cmp::Ordering::Less);
            if !newer {
                break;
            }
            index -= 1;
        }
        if index >= limit {
            continue;
        }
        kept.insert(index, atom.clone());
        if kept.len() > limit {
            kept.pop();
        }
    }
    kept
}

fn sparse_to_map(vector: &serde_json::Map<String, Value>) -> HashMap<String, f64> {
    vector
        .iter()
        .filter_map(|(term, value)| value.as_f64().map(|number| (term.clone(), number)))
        .collect()
}

async fn file_mtime_ms(path: &Path) -> f64 {
    match fs::metadata(path).await {
        Ok(metadata) => metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .unwrap_or(0.0),
        Err(_) => 0.0,
    }
}

fn cwd_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn effective_cwd(session_cwd: &Path, fallback: &Path) -> String {
    if session_cwd.as_os_str().is_empty() {
        cwd_text(fallback)
    } else {
        cwd_text(session_cwd)
    }
}

pub struct Explanation {
    pub trace: Vec<RecallTraceNode>,
    pub dropped: Vec<RecallDrop>,
}

impl AssociativeMemoryService {
    pub fn new(deps: MemoryDeps) -> Self {
        Self {
            deps,
            state: tokio::sync::Mutex::new(ServiceState::new()),
            stores: Arc::new(Mutex::new(HashMap::new())),
            queue: tokio::sync::Mutex::new(()),
            now_override: None,
            dispose: AtomicBool::new(false),
        }
    }

    pub fn with_now_override(
        mut self,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        self.now_override = Some(now);
        self
    }

    pub fn register_reminders(self: &Arc<Self>) {
        let Some(reminders) = self.deps.reminders.clone() else {
            return;
        };
        let weak = Arc::downgrade(self);
        reminders.register(
            ASSOCIATIVE_MEMORY_SIGNAL_VARIANT,
            Arc::new(move |is_new_turn| {
                let weak = weak.clone();
                Box::pin(async move {
                    if !is_new_turn {
                        return None;
                    }
                    let service = weak.upgrade()?;
                    let text = service.last_user_text();
                    let block = service.signal_block(&text).await;
                    if block.is_empty() {
                        None
                    } else {
                        Some(block)
                    }
                })
            }),
        );
    }

    pub fn dispose(&self) {
        self.dispose.store(true, Ordering::SeqCst);
        let mut cache = self.stores.lock().unwrap();
        cache.clear();
    }

    fn now(&self) -> i64 {
        self.now_override.as_ref().map_or_else(system_now_ms, |now| now())
    }

    fn enabled(&self) -> bool {
        self.deps
            .flags
            .as_ref()
            .is_some_and(|flags| flags.enabled(ASSOCIATIVE_MEMORY_FLAG_ID))
    }

    fn log_warn(&self, message: &str, data: Value) {
        if let Some(logger) = &self.deps.logger {
            logger.warn(message, &data);
        }
    }

    fn log_debug(&self, message: &str, data: Value) {
        if let Some(logger) = &self.deps.logger {
            logger.debug(message, &data);
        }
    }

    fn last_user_text(&self) -> String {
        let Some(history) = self.deps.history.as_ref() else {
            return String::new();
        };
        for message in history.get().iter().rev() {
            if message.role == "user" && !message.content.is_empty() {
                return message.content.clone();
            }
        }
        String::new()
    }

    async fn enqueue<F, T>(&self, job: F) -> Result<T, String>
    where
        F: std::future::Future<Output = Result<T, String>>,
    {
        let _guard = self.queue.lock().await;
        job.await
    }

    async fn import_legacy_memory(&self) -> Result<(), String> {
        if self.state.lock().await.legacy_memory_imported {
            return Ok(());
        }
        self.state.lock().await.legacy_memory_imported = true;
        if !self.enabled() {
            return Ok(());
        }
        let Some(store) = self.deps.legacy_store.as_ref() else {
            return Ok(());
        };
        let paths = store.list();
        for path in paths {
            let entry = store.get(&path);
            let Some(entry) = entry else {
                continue;
            };
            let scope = if entry.scope == "global" {
                MemoryScopeSelector::Global
            } else {
                MemoryScopeSelector::Project
            };
            if let Err(error) =
                Box::pin(self.write_note(&entry.body, scope, MemorySource::Legacy)).await
            {
                self.log_warn(
                    "associativeMemory.importLegacy.entryFailed",
                    json!({"path": entry.path, "error": error}),
                );
            }
        }
        store.set_read_only(true);
        Ok(())
    }

    pub async fn write_note(
        &self,
        text: &str,
        scope: MemoryScopeSelector,
        source: MemorySource,
    ) -> Result<WriteResult, String> {
        if !self.enabled() || trim_js(text).is_empty() {
            return Ok(WriteResult {
                note_id: String::new(),
                atom_ids: Vec::new(),
                blocked_by_code: false,
                scope,
                decision: Some(WriteDecision::Noop),
            });
        }
        self.import_legacy_memory().await?;
        let code = self.ensure_code().await?;
        let (allowed, matches) = apply_write_rules(text, &code.rules);
        if !allowed {
            let gating = classify_gating(GatingInput {
                same_cue_repeated: false,
                unstable_recall: false,
                new_scope_or_topic: false,
                positive_feedback: false,
            });
            self.record_audit(
                AuditKind::WriteBlocked,
                json!({
                    "scope": scope.as_str(),
                    "rules": matches.iter().map(|rule| rule.id.clone()).collect::<Vec<_>>(),
                    "gating": gating.mode,
                }),
            )
            .await;
            return Ok(WriteResult {
                note_id: String::new(),
                atom_ids: Vec::new(),
                blocked_by_code: true,
                scope,
                decision: Some(WriteDecision::Blocked),
            });
        }
        let key = if scope == MemoryScopeSelector::Global {
            GLOBAL_SCOPE_KEY.to_string()
        } else {
            self.project_scope_key().await?
        };
        let store = self.ensure_store(&key).await?;
        let mut store_guard = store.lock().await;
        let now = self.now();
        let scope_key = store_guard.scope_key().to_string();
        let gating = {
            let state = self.state.lock().await;
            classify_gating(GatingInput {
                same_cue_repeated: state
                    .last_write_text
                    .as_deref()
                    .is_some_and(|previous| note_fingerprint(previous) == note_fingerprint(text)),
                unstable_recall: state.last_recall_ids.is_empty() && !store_guard.list_notes().is_empty(),
                new_scope_or_topic: store_guard.list_notes().is_empty(),
                positive_feedback: false,
            })
        };
        self.state.lock().await.last_write_text = Some(text.to_string());

        if let Some(candidate) = self.find_merge_candidate(&store_guard, text) {
            self.reinforce_note(&mut store_guard, &candidate, now).await?;
            self.record_audit(
                AuditKind::WriteNoop,
                json!({
                    "scope": scope.as_str(),
                    "noteId": candidate.id,
                    "discarded": String::from_utf16_lossy(&text.encode_utf16().take(AUDIT_TEXT_LIMIT).collect::<Vec<_>>()),
                    "gating": gating.mode,
                }),
            )
            .await;
            return Ok(WriteResult {
                note_id: candidate.id,
                atom_ids: Vec::new(),
                blocked_by_code: false,
                scope,
                decision: Some(WriteDecision::Noop),
            });
        }

        let note = NoteRecord {
            id: format!("note_{}", Ulid::new()),
            scope_key,
            text: text.to_string(),
            created_at: now,
            source,
        };
        store_guard.put_note(note.clone()).await?;
        let atom_ids = self
            .enqueue(async { self.train_note(&mut store_guard, &note).await })
            .await?;
        self.record_audit(
            AuditKind::WriteAllowed,
            json!({
                "scope": scope.as_str(),
                "noteId": note.id,
                "atoms": atom_ids.len(),
                "gating": gating.mode,
            }),
        )
        .await;
        Ok(WriteResult {
            note_id: note.id,
            atom_ids,
            blocked_by_code: false,
            scope,
            decision: Some(WriteDecision::Added),
        })
    }

    pub async fn recall(&self, query: &RecallQuery) -> Result<RecallResult, String> {
        if !self.enabled() {
            return Ok(RecallResult {
                items: Vec::new(),
                trace: Vec::new(),
                dropped: None,
            });
        }
        self.import_legacy_memory().await?;
        let code = self.ensure_code().await?;
        let stores = self.stores_for(query.scope).await?;
        self.maybe_maintain(&stores).await?;
        let gating = {
            let state = self.state.lock().await;
            classify_gating(GatingInput {
                same_cue_repeated: state.last_recall_cue.as_deref() == Some(query.cue.as_str()),
                unstable_recall: !state.previous_recall_ids.is_empty()
                    && (state.previous_recall_ids.len() != state.last_recall_ids.len()
                        || state
                            .previous_recall_ids
                            .iter()
                            .zip(state.last_recall_ids.iter())
                            .any(|(left, right)| left != right)),
                new_scope_or_topic: false,
                positive_feedback: false,
            })
        };
        {
            let mut state = self.state.lock().await;
            state.last_recall_cue = Some(query.cue.clone());
            state.last_gating_mode = gating.mode;
        }
        let cue_dense = match self.ensure_embeddings().await? {
            Some(embeddings) => match embeddings.embed(vec![query.cue.clone()]).await {
                Ok(vectors) => vectors.into_iter().next(),
                Err(error) => {
                    self.log_warn(
                        "associative memory cue embedding failed; using sparse matching",
                        json!({"error": error}),
                    );
                    None
                }
            },
            None => None,
        };
        let params = associative_memory_params();
        let mut atoms = Vec::new();
        let mut concepts = Vec::new();
        let mut edges = Vec::new();
        let mut procedures = Vec::new();
        for store in &stores {
            let guard = store.lock().await;
            atoms.extend(guard.list_atoms());
            concepts.extend(guard.list_concepts());
            edges.extend(guard.list_edges());
            procedures.extend(guard.list_procedures());
        }
        let result = activate(
            query,
            &atoms,
            &concepts,
            &edges,
            &params,
            self.now(),
            &procedures,
            cue_dense.as_deref(),
        );
        self.persist_schema_links(&stores, &result.trace, self.now())
            .await?;
        let (kept, dropped) = filter_recall_items(&result.items, &code.rules);
        let items = order_conflict_groups(&kept, &code.rules);
        {
            let mut state = self.state.lock().await;
            state.previous_recall_ids = state.last_recall_ids.clone();
            state.last_recall_ids = items.iter().map(|item| item.id.clone()).collect();
        }
        if query.reinforce == Some(true) {
            self.reinforce_recalled(&stores, &items, self.now()).await?;
            self.record_activation_cooccurrence(&stores, &items, self.now())
                .await?;
        }
        if !dropped.is_empty() {
            self.record_audit(AuditKind::RecallFiltered, json!({"removed": dropped.len()}))
                .await;
        }
        Ok(RecallResult {
            items,
            trace: result.trace,
            dropped: Some(dropped),
        })
    }

    pub async fn signal_block(&self, message: &str) -> String {
        if !self.enabled() || trim_js(message).is_empty() {
            return String::new();
        }
        let params = associative_memory_params();
        let result = match self
            .recall(&RecallQuery {
                cue: message.to_string(),
                limit: Some(params.top_k),
                max_tokens: Some(params.injection_max_tokens),
                include_history: false,
                scope: Some(MemoryScopeSelector::All),
                reinforce: None,
            })
            .await
        {
            Ok(result) => result,
            Err(error) => {
                self.log_warn("associative memory signal block failed", json!({"error": error}));
                return String::new();
            }
        };
        let RecallResult { items, trace, .. } = result;
        let session_key = self.deps.session_id.clone();
        let injected_at = self.now();
        {
            let mut state = self.state.lock().await;
            state.working.touch(message, injected_at);
        }
        let mut sources: HashMap<String, MemorySource> = HashMap::new();
        let stores = match self.stores_for(Some(MemoryScopeSelector::All)).await {
            Ok(stores) => stores,
            Err(error) => {
                self.log_warn("associative memory signal block failed", json!({"error": error}));
                return String::new();
            }
        };
        for store in &stores {
            let guard = store.lock().await;
            for note in guard.list_notes() {
                sources.insert(note.id.clone(), note.source);
            }
        }
        let total = items.len();
        let trusted: Vec<RecallItem> = items
            .into_iter()
            .filter(|item| {
                sources
                    .get(&item.note_id)
                    .is_some_and(|source| matches!(source, MemorySource::User | MemorySource::Legacy))
            })
            .collect();
        if trusted.len() != total {
            self.record_audit(
                AuditKind::InjectionFiltered,
                json!({"removed": total - trusted.len()}),
            )
            .await;
        }
        let fresh: Vec<RecallItem> = trusted
            .into_iter()
            .filter(|item| !already_rendered(&session_key, &item.text))
            .collect();
        if fresh.is_empty() {
            let silent = {
                let mut state = self.state.lock().await;
                state.working.reactivate(message, injected_at)
            };
            if silent.is_empty() {
                return String::new();
            }
            let lines = silent
                .iter()
                .take(3)
                .map(|item| format!("- {}", item.text))
                .collect::<Vec<_>>()
                .join("\n");
            return format!("{MEMORY_DATA_BOUNDARY}\n[short-term context]\n{lines}");
        }
        mark_rendered(
            &session_key,
            &fresh
                .iter()
                .map(|item| item.text.clone())
                .collect::<Vec<_>>(),
        );
        let injected_stores = match self.stores_for(Some(MemoryScopeSelector::All)).await {
            Ok(stores) => stores,
            Err(error) => {
                self.log_warn("associative memory signal block failed", json!({"error": error}));
                return String::new();
            }
        };
        if let Err(error) = self.touch_injected(&injected_stores, &fresh, injected_at).await {
            self.log_warn("associative memory signal block failed", json!({"error": error}));
            return String::new();
        }
        if let Err(error) = self
            .record_activation_cooccurrence(&injected_stores, &fresh, injected_at)
            .await
        {
            self.log_warn("associative memory signal block failed", json!({"error": error}));
            return String::new();
        }
        render_recall(
            &RecallResult {
                items: fresh,
                trace,
                dropped: None,
            },
            params.injection_max_tokens,
        )
    }

    pub async fn pin(&self, id: &str) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        let stores = self.stores_for(Some(MemoryScopeSelector::All)).await?;
        for store in &stores {
            let mut guard = store.lock().await;
            if let Some(atom) = guard.get_atom(id) {
                guard
                    .put_atom(Atom {
                        pinned: true,
                        base_weight: atom.base_weight + 1.0,
                        confidence: update_confidence(atom.confidence, true, CONFIDENCE_RATE),
                        ..atom
                    })
                    .await?;
                self.record_audit(AuditKind::Pin, json!({"id": id})).await;
                return Ok(());
            }
        }
        for store in &stores {
            let mut guard = store.lock().await;
            if let Some(procedure) = guard.get_procedure(id) {
                guard
                    .put_procedure(crate::features::associativeMemory::types::Procedure {
                        pinned: true,
                        ..procedure
                    })
                    .await?;
                self.record_audit(AuditKind::Pin, json!({"id": id})).await;
                return Ok(());
            }
        }
        for store in &stores {
            let mut guard = store.lock().await;
            if let Some(concept) = guard.get_concept(id) {
                guard
                    .put_concept(crate::features::associativeMemory::types::Concept {
                        pinned: true,
                        ..concept
                    })
                    .await?;
                self.record_audit(AuditKind::Pin, json!({"id": id})).await;
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn forget(&self, id: &str) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        let stores = self.stores_for(Some(MemoryScopeSelector::All)).await?;
        for store in &stores {
            let mut guard = store.lock().await;
            let Some(atom) = guard.get_atom(id) else {
                continue;
            };
            self.detach_atom_links(&mut guard, &HashSet::from([id.to_string()]))
                .await?;
            guard
                .put_atom(Atom {
                    is_deleted: true,
                    suppressed: true,
                    confidence: update_confidence(atom.confidence, false, CONFIDENCE_RATE),
                    ..atom
                })
                .await?;
            self.record_audit(AuditKind::Forget, json!({"id": id})).await;
            return Ok(());
        }
        for store in &stores {
            let mut guard = store.lock().await;
            if guard.get_procedure(id).is_none() {
                continue;
            }
            self.forget_derived(&mut guard, id).await?;
            self.record_audit(AuditKind::Forget, json!({"id": id})).await;
            return Ok(());
        }
        for store in &stores {
            let mut guard = store.lock().await;
            if guard.get_concept(id).is_none() {
                continue;
            }
            self.forget_derived(&mut guard, id).await?;
            self.record_audit(AuditKind::Forget, json!({"id": id})).await;
            return Ok(());
        }
        Ok(())
    }

    pub async fn stats(&self, scope: Option<MemoryScopeSelector>) -> AssociativeMemoryStats {
        let scope = scope.unwrap_or(MemoryScopeSelector::All);
        if !self.enabled() {
            return AssociativeMemoryStats {
                atoms: 0,
                concepts: 0,
                procedures: 0,
                edges: 0,
                notes: 0,
                suppressed: 0,
                pinned: 0,
                mean_confidence: 0.0,
                working_items: 0,
                source_counts: empty_source_counts(),
                gating_mode: Some(gating_mode_name(self.state.lock().await.last_gating_mode)),
            };
        }
        let stores = match self.stores_for(Some(scope)).await {
            Ok(stores) => stores,
            Err(error) => {
                self.log_warn("associative memory stats failed", json!({"error": error}));
                return AssociativeMemoryStats {
                    atoms: 0,
                    concepts: 0,
                    procedures: 0,
                    edges: 0,
                    notes: 0,
                    suppressed: 0,
                    pinned: 0,
                    mean_confidence: 0.0,
                    working_items: 0,
                    source_counts: empty_source_counts(),
                    gating_mode: Some(gating_mode_name(
                        self.state.lock().await.last_gating_mode,
                    )),
                };
            }
        };
        let mut atoms = Vec::new();
        let mut concepts = Vec::new();
        let mut edges = 0_usize;
        let mut notes = 0_usize;
        let mut procedures = 0_usize;
        let mut source_counts = empty_source_counts();
        for store in &stores {
            let guard = store.lock().await;
            atoms.extend(guard.list_atoms());
            concepts.extend(guard.list_concepts());
            edges += guard.list_edges().len();
            notes += guard.list_notes().len();
            procedures += guard.list_procedures().len();
            for note in guard.list_notes() {
                if let Some(count) = source_counts.get_mut(note.source.as_str()) {
                    *count = Value::from(count.as_i64().unwrap_or(0) + 1);
                }
            }
        }
        let live: Vec<&Atom> = atoms.iter().filter(|atom| !atom.is_deleted).collect();
        let suppressed = atoms
            .iter()
            .filter(|atom| atom.suppressed)
            .count()
            + concepts.iter().filter(|concept| concept.suppressed).count();
        let pinned = atoms.iter().filter(|atom| atom.pinned).count()
            + concepts.iter().filter(|concept| concept.pinned).count();
        let working_items = {
            let mut state = self.state.lock().await;
            state.working.snapshot(self.now()).size
        };
        AssociativeMemoryStats {
            atoms: live.len() as i64,
            concepts: concepts.len() as i64,
            procedures: procedures as i64,
            edges: edges as i64,
            notes: notes as i64,
            suppressed: suppressed as i64,
            pinned: pinned as i64,
            mean_confidence: if live.is_empty() {
                0.0
            } else {
                live.iter().map(|atom| atom.confidence).sum::<f64>() / live.len() as f64
            },
            working_items: working_items as i64,
            source_counts,
            gating_mode: Some(gating_mode_name(self.state.lock().await.last_gating_mode)),
        }
    }

    pub async fn list(&self, options: &ListOptions) -> Result<AssociativeMemoryList, String> {
        if !self.enabled() {
            return Ok(AssociativeMemoryList {
                atoms: Vec::new(),
                concepts: Vec::new(),
                procedures: Vec::new(),
            });
        }
        self.import_legacy_memory().await?;
        let stores = self.stores_for(options.scope).await?;
        let requested_limit = options.limit.unwrap_or(associative_memory_params().list_limit);
        let mut atoms = Vec::new();
        let mut concepts = Vec::new();
        let mut procedures = Vec::new();
        for store in &stores {
            let guard = store.lock().await;
            atoms.extend(guard.list_atoms());
            concepts.extend(guard.list_concepts());
            procedures.extend(guard.list_procedures());
        }
        let select_atoms = options.kind != Some(crate::features::associativeMemory::types::NodeType::Concept)
            && options.kind != Some(crate::features::associativeMemory::types::NodeType::Procedure);
        let select_concepts = options.kind != Some(crate::features::associativeMemory::types::NodeType::Atom)
            && options.kind != Some(crate::features::associativeMemory::types::NodeType::Procedure);
        let select_procedures = options.kind != Some(crate::features::associativeMemory::types::NodeType::Atom)
            && options.kind != Some(crate::features::associativeMemory::types::NodeType::Concept);
        let mut atoms = if select_atoms {
            atoms.retain(|atom| !atom.is_deleted);
            atoms.sort_by_key(|atom| std::cmp::Reverse(atom.last_activated_at));
            atoms.truncate(js_slice_limit(atoms.len(), requested_limit));
            atoms
        } else {
            Vec::new()
        };
        let mut concepts = if select_concepts {
            concepts.sort_by_key(|concept| std::cmp::Reverse(concept.last_activated_at));
            concepts.truncate(js_slice_limit(concepts.len(), requested_limit));
            concepts
        } else {
            Vec::new()
        };
        let mut procedures = if select_procedures {
            procedures.sort_by_key(|procedure| std::cmp::Reverse(procedure.last_activated_at));
            procedures.truncate(js_slice_limit(procedures.len(), requested_limit));
            procedures
        } else {
            Vec::new()
        };
        let _ = (&mut atoms, &mut concepts, &mut procedures);
        Ok(AssociativeMemoryList {
            atoms,
            concepts,
            procedures,
        })
    }

    pub async fn export(&self, scope: Option<MemoryScopeSelector>) -> Result<AssociativeMemoryExport, String> {
        if !self.enabled() {
            return Ok(AssociativeMemoryExport {
                path: String::new(),
                atoms: 0,
                concepts: 0,
                notes: 0,
            });
        }
        let scope = scope.unwrap_or(MemoryScopeSelector::All);
        let stores = self.stores_for(Some(scope)).await?;
        let mut atoms = Vec::new();
        let mut concepts = Vec::new();
        let mut notes = Vec::new();
        let mut edges = Vec::new();
        let mut procedures = Vec::new();
        for store in &stores {
            let guard = store.lock().await;
            atoms.extend(guard.list_atoms());
            concepts.extend(guard.list_concepts());
            notes.extend(guard.list_notes());
            edges.extend(guard.list_edges());
            procedures.extend(guard.list_procedures());
        }
        let audit = self.audit(Some(200)).await.unwrap_or_default();
        let path = Path::new(&effective_cwd(&self.deps.session_cwd, &self.deps.cwd))
            .join("memory-export.json");
        let body = json!({
            "exportedAt": js_iso_time(self.now()),
            "atoms": atoms,
            "concepts": concepts,
            "procedures": procedures,
            "edges": edges,
            "notes": notes,
            "audit": audit,
        });
        fs::write(&path, format!("{body}\n"))
            .await
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        Ok(AssociativeMemoryExport {
            path: cwd_text(&path),
            atoms: atoms.len() as i64,
            concepts: concepts.len() as i64,
            notes: notes.len() as i64,
        })
    }

    pub async fn replay(&self, limit: Option<i64>) -> Result<ReplayResult, String> {
        if !self.enabled() {
            return Ok(ReplayResult { replayed: 0 });
        }
        let params = associative_memory_params();
        let limit = limit.unwrap_or(10).max(0) as usize;
        let key = self.project_scope_key().await?;
        let store = self.ensure_store(&key).await?;
        let now = self.now();
        let mut guard = store.lock().await;
        let edges = guard.list_edges();
        let pending: Vec<Atom> = guard
            .list_atoms()
            .into_iter()
            .filter(|atom| atom.replay_priority > 0)
            .collect();
        let ranked = rank_by_need_gain(
            &pending,
            |atom| accessibility(atom, &edges, now, &params),
            |atom| atom.replay_priority as f64,
            |left, right| {
                compare_by_text(
                    (left.id.as_str(), left.text.as_str()),
                    (right.id.as_str(), right.text.as_str()),
                )
            },
        );
        let candidates = interleave(&ranked, limit, params.replay_strata.max(0) as usize);
        if candidates.is_empty() {
            return Ok(ReplayResult { replayed: 0 });
        }
        let selected: HashMap<String, Atom> = candidates
            .iter()
            .map(|atom| (atom.id.clone(), atom.clone()))
            .collect();
        let mut neighbors: HashMap<String, Vec<Atom>> = HashMap::new();
        for edge in &edges {
            if !selected.contains_key(&edge.from) || !selected.contains_key(&edge.to) {
                continue;
            }
            neighbors
                .entry(edge.from.clone())
                .or_default()
                .push(selected[&edge.to].clone());
        }
        let mut current = edges.clone();
        for atom in &candidates {
            let related = match neighbors.get(&atom.id) {
                Some(related) if !related.is_empty() => related.clone(),
                _ => continue,
            };
            let activity = recent_activity(&guard.list_edges(), now, params.context_window_ms);
            let trained = hebbian_connect(
                std::slice::from_ref(atom),
                &related,
                &current,
                &params,
                &activity,
                now,
                |candidate, existing| {
                    memory_separation_decision(candidate, existing, &params)
                        == MemorySeparationDecision::Separate
                },
            );
            for id in &trained.deleted {
                guard.delete_edge(id).await?;
            }
            for edge in &trained.edges {
                guard.put_edge(edge.clone()).await?;
            }
            current = trained.edges;
        }
        for atom in &candidates {
            guard
                .put_atom(Atom {
                    replay_priority: (atom.replay_priority - 1).max(0),
                    last_activated_at: now,
                    ..atom.clone()
                })
                .await?;
        }
        self.record_audit(AuditKind::Replay, json!({"replayed": candidates.len()}))
            .await;
        Ok(ReplayResult {
            replayed: candidates.len() as i64,
        })
    }

    pub async fn propose_rule(&self, rule: &Value) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        propose_rule(&self.deps.home_dir, STORE_ROOT_NAME, rule).await;
        self.record_audit(AuditKind::CodeProposed, json!({})).await;
        Ok(())
    }

    pub async fn approve_proposal(&self, index: i64) -> Result<HardRule, String> {
        if !self.enabled() {
            return Err("associative memory is disabled".to_string());
        }
        let rule = approve_proposal(
            &self.deps.home_dir,
            STORE_ROOT_NAME,
            index.max(0) as usize,
            &default_code(),
        )
        .await?;
        {
            let mut state = self.state.lock().await;
            state.code_loaded = false;
        }
        self.ensure_code().await?;
        self.record_audit(
            AuditKind::CodeApproved,
            json!({"index": index, "id": rule.id}),
        )
        .await;
        Ok(rule)
    }

    pub async fn label_concept(&self, id: &str, label: &str) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        let stores = self.stores_for(Some(MemoryScopeSelector::All)).await?;
        for store in &stores {
            let mut guard = store.lock().await;
            if let Some(concept) = guard.get_concept(id) {
                guard
                    .put_concept(crate::features::associativeMemory::types::Concept {
                        label: label.to_string(),
                        label_source: crate::features::associativeMemory::types::LabelSource::Llm,
                        ..concept
                    })
                    .await?;
                self.record_audit(AuditKind::Label, json!({"id": id})).await;
                return Ok(());
            }
        }
        Ok(())
    }

    pub async fn audit(&self, limit: Option<i64>) -> Result<Vec<AuditEntry>, String> {
        let path = audit_path(&cwd_text(&self.deps.store_dir), STORE_ROOT_NAME);
        let limit = limit.unwrap_or(20);
        let entries = read_audit(&path, usize::MAX).await?;
        let limit = js_slice_limit(entries.len(), limit);
        Ok(entries.into_iter().take(limit).collect())
    }

    pub async fn reconsolidate(
        &self,
        target_id: Option<&str>,
        text: &str,
    ) -> Result<ReconsolidateResult, String> {
        if !self.enabled() || trim_js(text).is_empty() {
            return Ok(ReconsolidateResult {
                note_id: String::new(),
                atom_ids: Vec::new(),
                superseded: false,
            });
        }
        let params = associative_memory_params();
        let code = self.ensure_code().await?;
        let (allowed, matches) = apply_write_rules(text, &code.rules);
        if !allowed {
            self.record_audit(
                AuditKind::WriteBlocked,
                json!({"reason": matches.iter().map(|rule| rule.id.clone()).collect::<Vec<_>>()}),
            )
            .await;
            return Ok(ReconsolidateResult {
                note_id: String::new(),
                atom_ids: Vec::new(),
                superseded: false,
            });
        }
        let fallback = self.state.lock().await.last_recall_ids.first().cloned();
        let target_id = target_id.map(str::to_string).or(fallback);
        let Some((store, atom)) = self.find_atom_store(target_id.as_deref()).await? else {
            return Ok(ReconsolidateResult {
                note_id: String::new(),
                atom_ids: Vec::new(),
                superseded: false,
            });
        };
        let mut guard = store.lock().await;
        let now = self.now();
        let atoms = guard.list_atoms();
        let df = compute_df(&atoms);
        let mut new_atoms = encode_atoms(
            &NoteRecord {
                id: format!("note_{}", Ulid::new()),
                scope_key: atom.scope_key.clone(),
                text: text.to_string(),
                created_at: now,
                source: MemorySource::Tool,
            },
            &df,
            atoms.len() as i64,
            now,
            &params,
        );
        if let Some(embeddings) = self.ensure_embeddings().await?
            && !new_atoms.is_empty()
        {
            let texts = new_atoms
                .iter()
                .map(|atom| atom.text.clone())
                .collect::<Vec<_>>();
            match embeddings.embed(texts).await {
                Ok(vectors) => {
                    for (atom, vector) in new_atoms.iter_mut().zip(vectors) {
                        atom.dense_vector = Some(vector);
                    }
                }
                Err(error) => self.log_warn(
                    "associative memory embeddings failed during reconsolidation",
                    json!({"error": error}),
                ),
            }
        }
        let neighbors = self.neighbors_of(&guard, &atom.id);
        let mismatch = if new_atoms.is_empty() {
            0.0
        } else {
            cosine(&sparse_to_map(&new_atoms[0].vector), &sparse_to_map(&atom.vector))
        };
        let updatable = mismatch > params.reconsolidate_novel_threshold
            && mismatch < params.reconsolidate_merge_threshold;
        let context: Vec<Atom> = std::iter::once(atom.clone()).chain(neighbors.iter().cloned()).collect();
        let activity = recent_activity(&guard.list_edges(), now, params.context_window_ms);
        let trained = hebbian_connect(
            &new_atoms,
            &context,
            &guard.list_edges(),
            &params,
            &activity,
            now,
            |candidate, existing| {
                updatable
                    && existing.id == atom.id
                    && cosine(&sparse_to_map(&candidate.vector), &sparse_to_map(&existing.vector))
                        > RECONSOLIDATE_THRESHOLD
            },
        );
        let mut superseded = false;
        for (id, state) in &trained.superseded {
            if let Some(old) = guard.get_atom(id)
                && !old.pinned
            {
                guard
                    .put_atom(Atom {
                        valid_until: Some(state.valid_until),
                        suppressed: state.suppressed,
                        replay_priority: old.replay_priority + 1,
                        ..old
                    })
                    .await?;
                superseded = true;
                self.record_audit(
                    AuditKind::Supersede,
                    json!({"from": new_atoms.first().map(|atom| atom.id.clone()), "to": id}),
                )
                .await;
            }
        }
        for atom in &new_atoms {
            guard.put_atom(atom.clone()).await?;
        }
        for id in &trained.deleted {
            guard.delete_edge(id).await?;
        }
        for edge in &trained.edges {
            guard.put_edge(edge.clone()).await?;
        }
        let source = guard
            .get_note(&atom.note_id)
            .map_or(MemorySource::Tool, |note| note.source);
        guard
            .put_note(NoteRecord {
                id: format!("note_{}", Ulid::new()),
                scope_key: atom.scope_key.clone(),
                text: text.to_string(),
                created_at: now,
                source,
            })
            .await?;
        self.maintain(&mut guard, now).await?;
        self.record_audit(
            AuditKind::Reconsolidate,
            json!({
                "targetId": atom.id,
                "newAtomIds": new_atoms.iter().map(|atom| atom.id.clone()).collect::<Vec<_>>(),
            }),
        )
        .await;
        Ok(ReconsolidateResult {
            note_id: new_atoms
                .first()
                .map_or(String::new(), |atom| atom.note_id.clone()),
            atom_ids: new_atoms.iter().map(|atom| atom.id.clone()).collect(),
            superseded,
        })
    }

    pub async fn explain(&self, query: &RecallQuery) -> Result<Explanation, String> {
        let result = self.recall(query).await?;
        Ok(Explanation {
            trace: result.trace,
            dropped: result.dropped.unwrap_or_default(),
        })
    }

    pub async fn code_block(&self) -> String {
        if !self.enabled() {
            return String::new();
        }
        match self.ensure_code().await {
            Ok(code) => code.guidance,
            Err(error) => {
                self.log_warn("associative memory code block failed", json!({"error": error}));
                String::new()
            }
        }
    }

    pub async fn rebuild(&self, scope: Option<MemoryScopeSelector>) -> Result<(), String> {
        if !self.enabled() {
            return Ok(());
        }
        let stores = self.stores_for(scope).await?;
        for store in &stores {
            let mut guard = store.lock().await;
            let notes = guard.list_notes();
            guard.clear_graph().await?;
            for note in &notes {
                self.train_note(&mut guard, note).await?;
            }
        }
        self.state.lock().await.last_recall_ids.clear();
        self.record_audit(
            AuditKind::Rebuild,
            json!({"scope": scope.map_or("all", MemoryScopeSelector::as_str)}),
        )
        .await;
        Ok(())
    }

    pub fn atom_budget(&self) -> usize {
        MAX_ATOMS
    }

    pub fn note_budget(&self) -> usize {
        MAX_NOTES
    }

    fn find_merge_candidate(&self, store: &AssociativeMemoryStore, text: &str) -> Option<NoteRecord> {
        let fingerprint = note_fingerprint(text);
        store
            .list_notes()
            .into_iter()
            .find(|note| note_fingerprint(&note.text) == fingerprint)
    }

    async fn reinforce_note(
        &self,
        store: &mut AssociativeMemoryStore,
        note: &NoteRecord,
        now: i64,
    ) -> Result<(), String> {
        for atom in store.list_atoms() {
            if atom.note_id != note.id {
                continue;
            }
            store
                .put_atom(Atom {
                    last_activated_at: now,
                    ..atom
                })
                .await?;
        }
        Ok(())
    }

    async fn detach_atom_links(
        &self,
        store: &mut AssociativeMemoryStore,
        doomed: &HashSet<String>,
    ) -> Result<(), String> {
        if doomed.is_empty() {
            return Ok(());
        }
        for edge in store.list_edges() {
            if doomed.contains(&edge.from) || doomed.contains(&edge.to) {
                store.delete_edge(&edge.id).await?;
            }
        }
        for cooc in store.list_cooccurrences() {
            if doomed.contains(&cooc.a) || doomed.contains(&cooc.b) {
                store.delete_cooccurrence(&cooc.key).await?;
            }
        }
        for procedure in store.list_procedures() {
            if procedure.step_ids.iter().any(|id| doomed.contains(id)) {
                store.delete_procedure(&procedure.id).await?;
            }
        }
        for concept in store.list_concepts() {
            if concept.atom_ids.iter().any(|id| doomed.contains(id)) {
                store.delete_concept(&concept.id).await?;
            }
        }
        Ok(())
    }

    async fn retire_atoms(
        &self,
        store: &mut AssociativeMemoryStore,
        doomed: &HashSet<String>,
        now: i64,
    ) -> Result<(), String> {
        self.detach_atom_links(store, doomed).await?;
        for id in doomed {
            if let Some(atom) = store.get_atom(id) {
                store
                    .put_atom(Atom {
                        suppressed: true,
                        valid_until: Some(now),
                        ..atom
                    })
                    .await?;
            }
        }
        Ok(())
    }

    async fn forget_derived(
        &self,
        store: &mut AssociativeMemoryStore,
        id: &str,
    ) -> Result<(), String> {
        for edge in store.list_edges() {
            if edge.from == id || edge.to == id {
                store.delete_edge(&edge.id).await?;
            }
        }
        store.delete_concept(id).await?;
        store.delete_procedure(id).await?;
        Ok(())
    }

    async fn find_atom_store(
        &self,
        id: Option<&str>,
    ) -> Result<Option<(StoreGuard, Atom)>, String> {
        let Some(id) = id else {
            return Ok(None);
        };
        let stores = self.stores_for(Some(MemoryScopeSelector::All)).await?;
        for store in stores {
            let atom = {
                let guard = store.lock().await;
                guard.get_atom(id)
            };
            if let Some(atom) = atom {
                return Ok(Some((store, atom)));
            }
        }
        Ok(None)
    }

    fn neighbors_of(&self, store: &AssociativeMemoryStore, id: &str) -> Vec<Atom> {
        let mut neighbors = Vec::new();
        for edge in store.list_edges() {
            if edge.from != id {
                continue;
            }
            if let Some(target) = store.get_atom(&edge.to) {
                neighbors.push(target);
            }
        }
        neighbors
    }

    async fn record_audit(&self, kind: AuditKind, detail: Value) {
        let path = audit_path(&cwd_text(&self.deps.store_dir), STORE_ROOT_NAME);
        let entry = AuditEntry {
            at: self.now(),
            kind,
            detail: match detail {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            },
        };
        if let Err(error) = append_audit(&path, &entry).await {
            self.log_warn(
                "associative memory audit write failed",
                json!({"error": error}),
            );
        }
    }

    async fn record_activation_cooccurrence(
        &self,
        stores: &[StoreGuard],
        items: &[RecallItem],
        now: i64,
    ) -> Result<(), String> {
        let params = associative_memory_params();
        let ids: Vec<String> = items
            .iter()
            .filter(|item| item.kind == crate::features::associativeMemory::types::NodeType::Atom)
            .take(ACTIVATION_COOC_LIMIT)
            .map(|item| item.id.clone())
            .collect();
        if ids.len() < 2 {
            return Ok(());
        }
        let threshold = cooc_cluster_threshold(&params);
        for store in stores {
            let mut guard = store.lock().await;
            let owned: Vec<String> = ids
                .iter()
                .filter(|id| guard.get_atom(id).is_some())
                .cloned()
                .collect();
            if owned.len() < 2 {
                continue;
            }
            let mut crossed = false;
            for left in 0..owned.len() {
                for right in left + 1..owned.len() {
                    let (a, b) = if owned[left] < owned[right] {
                        (owned[left].clone(), owned[right].clone())
                    } else {
                        (owned[right].clone(), owned[left].clone())
                    };
                    let key = format!("{a}|{b}");
                    let previous = guard.get_cooccurrence(&key);
                    let count = previous.as_ref().map_or(1, |cooc| cooc.count + 1);
                    let crosses = count >= threshold
                        && previous.as_ref().map_or(0, |cooc| cooc.count) < threshold;
                    let next = match previous {
                        Some(previous) => Cooccurrence {
                            count,
                            last_at: now,
                            ..previous
                        },
                        None => Cooccurrence {
                            key: key.clone(),
                            a,
                            b,
                            count,
                            last_at: now,
                        },
                    };
                    guard.put_cooccurrence(next).await?;
                    if crosses {
                        crossed = true;
                    }
                }
            }
            if !crossed {
                continue;
            }
            let cluster = cluster_incremental(
                &guard.list_atoms(),
                &guard.list_cooccurrences(),
                &guard.list_concepts(),
                &params,
                now,
            );
            for concept in &cluster.concepts {
                guard.put_concept(concept.clone()).await?;
            }
            for edge in &cluster.edges {
                guard.put_edge(edge.clone()).await?;
            }
        }
        Ok(())
    }

    async fn persist_schema_links(
        &self,
        stores: &[StoreGuard],
        trace: &[RecallTraceNode],
        now: i64,
    ) -> Result<(), String> {
        let params = associative_memory_params();
        for link in collect_schema_links(trace, &params) {
            let id = format!("{}?{}", link.from, link.to);
            for store in stores {
                let mut guard = store.lock().await;
                if guard.get_atom(&link.from).is_none() || guard.get_atom(&link.to).is_none() {
                    continue;
                }
                let existing = guard.get_edge(&id);
                let edge = Edge {
                    id: id.clone(),
                    from: link.from.clone(),
                    to: link.to.clone(),
                    kind: EdgeKind::Schema,
                    weight: 1.0 / link.hops as f64,
                    strengthen_count: existing.as_ref().map_or(1, |edge| edge.strengthen_count + 1),
                    valid_from: existing.as_ref().map_or(now, |edge| edge.valid_from),
                    valid_until: None,
                    created_at: existing.as_ref().map_or(now, |edge| edge.created_at),
                    last_used_at: now,
                };
                guard.put_edge(edge).await?;
                break;
            }
        }
        Ok(())
    }

    async fn touch_injected(
        &self,
        stores: &[StoreGuard],
        items: &[RecallItem],
        now: i64,
    ) -> Result<(), String> {
        let ids: Vec<String> = items
            .iter()
            .filter(|item| item.kind == crate::features::associativeMemory::types::NodeType::Atom)
            .take(ACTIVATION_COOC_LIMIT)
            .map(|item| item.id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        for store in stores {
            let mut guard = store.lock().await;
            for id in &ids {
                if let Some(atom) = guard.get_atom(id) {
                    guard
                        .put_atom(Atom {
                            last_activated_at: now,
                            ..atom
                        })
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn reinforce_recalled(
        &self,
        stores: &[StoreGuard],
        items: &[RecallItem],
        now: i64,
    ) -> Result<(), String> {
        let params = associative_memory_params();
        let recalled: HashSet<String> = items.iter().map(|item| item.id.clone()).collect();
        if recalled.is_empty() {
            return Ok(());
        }
        for store in stores {
            let mut guard = store.lock().await;
            for edge in guard.list_edges() {
                if !recalled.contains(&edge.from) || !recalled.contains(&edge.to) {
                    continue;
                }
                let strengthen_count = edge.strengthen_count + 1;
                guard
                    .put_edge(Edge {
                        weight: params
                            .max_edge_weight
                            .min(edge.weight + params.base_learning_rate / strengthen_count as f64),
                        strengthen_count,
                        last_used_at: now,
                        ..edge
                    })
                    .await?;
            }
            for atom in guard.list_atoms() {
                if !recalled.contains(&atom.id) {
                    continue;
                }
                guard
                    .put_atom(Atom {
                        last_activated_at: now,
                        ..atom
                    })
                    .await?;
            }
        }
        Ok(())
    }

    async fn ensure_store(&self, scope_key: &str) -> Result<StoreGuard, String> {
        let key = scope_key.to_string();
        {
            let mut cache = self.stores.lock().unwrap();
            if let Some(entry) = cache.get_mut(&key) {
                entry.refs += 1;
                entry.idle_since = None;
                return Ok(StoreGuard {
                    key,
                    store: entry.store.clone(),
                    cache: self.stores.clone(),
                });
            }
        }
        let root = self.deps.store_dir.join(STORE_ROOT_NAME);
        let store = AssociativeMemoryStore::open(scope_key, &root).await?;
        let store: SharedStore = Arc::new(tokio::sync::Mutex::new(store));
        let store = {
            let mut cache = self.stores.lock().unwrap();
            let entry = cache.entry(key.clone()).or_insert_with(|| StoreCacheEntry {
                store: store.clone(),
                refs: 0,
                idle_since: None,
            });
            entry.refs += 1;
            entry.idle_since = None;
            entry.store.clone()
        };
        self.trim_stores();
        Ok(StoreGuard {
            key,
            store,
            cache: self.stores.clone(),
        })
    }

    fn trim_stores(&self) {
        let mut cache = self.stores.lock().unwrap();
        if cache.len() <= MAX_OPEN_STORES {
            return;
        }
        let mut idle: Vec<(String, i64)> = cache
            .iter()
            .filter(|(_, entry)| entry.refs == 0 && entry.idle_since.is_some())
            .map(|(key, entry)| (key.clone(), entry.idle_since.unwrap_or(0)))
            .collect();
        idle.sort_by_key(|(_, since)| *since);
        for (key, _) in idle {
            if cache.len() <= MAX_OPEN_STORES {
                return;
            }
            cache.remove(&key);
        }
    }

    async fn stores_for(
        &self,
        scope: Option<MemoryScopeSelector>,
    ) -> Result<Vec<StoreGuard>, String> {
        match scope {
            Some(MemoryScopeSelector::Global) => {
                Ok(vec![self.ensure_store(GLOBAL_SCOPE_KEY).await?])
            }
            Some(MemoryScopeSelector::Project) => {
                Ok(vec![self.ensure_store(&self.project_scope_key().await?).await?])
            }
            _ => Ok(vec![
                self.ensure_store(&self.project_scope_key().await?).await?,
                self.ensure_store(GLOBAL_SCOPE_KEY).await?,
            ]),
        }
    }

    async fn ensure_embeddings(&self) -> Result<Option<Arc<dyn EmbeddingsClient>>, String> {
        {
            let state = self.state.lock().await;
            if state.embeddings_client.is_some() {
                return Ok(state.embeddings_client.clone());
            }
        }
        let now = self.now();
        {
            let mut state = self.state.lock().await;
            if state.embeddings_resolved && now - state.embeddings_resolved_at < EMBEDDINGS_RETRY_MS {
                return Ok(None);
            }
            state.embeddings_resolved = true;
            state.embeddings_resolved_at = now;
        }
        let code = self.ensure_code().await?;
        if !code_allows_embeddings(&code) {
            let already_logged = {
                let mut state = self.state.lock().await;
                if state.embeddings_blocked_logged {
                    true
                } else {
                    state.embeddings_blocked_logged = true;
                    false
                }
            };
            if !already_logged {
                self.record_audit(AuditKind::EmbeddingsBlocked, json!({})).await;
            }
            return Ok(None);
        }
        let options = resolve_embeddings_options(|name| std::env::var(name).ok());
        let Some(options) = options else {
            return Ok(None);
        };
        let client: Arc<dyn EmbeddingsClient> = Arc::new(HttpEmbeddingsClient::new(options));
        self.state.lock().await.embeddings_client = Some(client.clone());
        Ok(Some(client))
    }

    async fn ensure_code(&self) -> Result<MemoryCode, String> {
        let path = self.deps.home_dir.join(STORE_ROOT_NAME).join(CODE_FILE_NAME);
        let mtime = file_mtime_ms(&path).await;
        {
            let state = self.state.lock().await;
            if state.code_loaded && mtime == state.code_mtime_ms {
                return Ok(state.code.clone());
            }
        }
        let code = load_code_file(&path, &default_code()).await;
        let mut state = self.state.lock().await;
        state.code = code.clone();
        state.code_loaded = true;
        state.code_mtime_ms = mtime;
        Ok(code)
    }

    async fn project_scope_key(&self) -> Result<String, String> {
        let key = self.project_key();
        {
            let mut state = self.state.lock().await;
            if state.migrated_project_scope {
                return Ok(key);
            }
            state.migrated_project_scope = true;
        }
        let cwd = effective_cwd(&self.deps.session_cwd, &self.deps.cwd);
        let mut legacy_keys = vec![
            legacy_project_id_from_cwd(&cwd),
            legacy_project_id_from_cwd(&cwd_text(&self.deps.cwd)),
            resolve_project_identity(&cwd_text(&self.deps.cwd)).id,
        ];
        legacy_keys.retain(|legacy| legacy != &key);
        for legacy in legacy_keys {
            self.rename_legacy_store(&legacy, &key).await;
        }
        Ok(key)
    }

    async fn rename_legacy_store(&self, from: &str, to: &str) {
        let root = self.deps.store_dir.join(STORE_ROOT_NAME);
        let from_key = root.join(from);
        let to_key = root.join(to);
        if !from_key.exists() || to_key.exists() {
            return;
        }
        {
            let mut cache = self.stores.lock().unwrap();
            cache.remove(from);
        }
        if let Err(error) = fs::rename(&from_key, &to_key).await {
            self.log_warn(
                "associative memory project scope migration failed",
                json!({"from": from, "to": to, "error": error.to_string()}),
            );
            return;
        }
        self.log_debug(
            "associative memory project scope migrated",
            json!({"from": from, "to": to}),
        );
    }

    fn project_key(&self) -> String {
        let cwd = effective_cwd(&self.deps.session_cwd, &self.deps.cwd);
        resolve_project_identity(&cwd).id
    }

    async fn train_note(
        &self,
        store: &mut AssociativeMemoryStore,
        note: &NoteRecord,
    ) -> Result<Vec<String>, String> {
        let params = associative_memory_params();
        let now = self.now();
        let atoms = store.list_atoms();
        let df = compute_df(&atoms);
        let mut new_atoms = encode_atoms(note, &df, atoms.len() as i64, now, &params);
        if let Some(embeddings) = self.ensure_embeddings().await?
            && !new_atoms.is_empty()
        {
            let texts = new_atoms
                .iter()
                .map(|atom| atom.text.clone())
                .collect::<Vec<_>>();
            match embeddings.embed(texts.clone()).await {
                Ok(vectors) => {
                    for (atom, vector) in new_atoms.iter_mut().zip(vectors) {
                        atom.dense_vector = Some(vector);
                    }
                }
                Err(error) => self.log_warn(
                    "associative memory embeddings failed; storing sparse vectors only",
                    json!({"error": error}),
                ),
            }
        }
        let mut context: Vec<Atom> = atoms
            .iter()
            .filter(|atom| now - atom.last_activated_at <= params.context_window_ms)
            .cloned()
            .collect();
        context.extend(new_atoms.iter().cloned());
        let new_ids: HashSet<String> = new_atoms.iter().map(|atom| atom.id.clone()).collect();
        let activity = recent_activity(&store.list_edges(), now, params.context_window_ms);
        let trained = hebbian_connect(
            &new_atoms,
            &context,
            &store.edges_from(&new_ids),
            &params,
            &activity,
            now,
            |candidate, existing| {
                memory_separation_decision(candidate, existing, &params)
                    == MemorySeparationDecision::Separate
            },
        );
        for (id, state) in &trained.superseded {
            if let Some(old) = store.get_atom(id)
                && !old.pinned
            {
                store
                    .put_atom(Atom {
                        valid_until: Some(state.valid_until),
                        suppressed: state.suppressed,
                        replay_priority: old.replay_priority + 1,
                        ..old
                    })
                    .await?;
                self.record_audit(
                    AuditKind::Supersede,
                    json!({"from": note.id, "to": id}),
                )
                .await;
            }
        }
        for atom in &new_atoms {
            store.put_atom(atom.clone()).await?;
        }
        for id in &trained.deleted {
            store.delete_edge(id).await?;
        }
        for edge in &trained.edges {
            store.put_edge(edge.clone()).await?;
        }
        self.induce_forgetting(store, &new_atoms).await?;
        self.update_cooccurrence(store, &new_atoms, &context).await?;
        let procedures = distill_procedures(
            &store.list_notes(),
            &store.list_atoms(),
            &store.list_procedures(),
            &params,
            now,
        );
        for procedure in &procedures.procedures {
            store.put_procedure(procedure.clone()).await?;
        }
        for edge in &procedures.edges {
            store.put_edge(edge.clone()).await?;
        }
        let atom_ids = new_atoms.iter().map(|atom| atom.id.clone()).collect::<Vec<_>>();
        store.put_note(note.clone()).await?;
        let reset = {
            let mut state = self.state.lock().await;
            state.writes_since_maintain += 1;
            state.writes_since_maintain >= MAINTAIN_EVERY_WRITES
        };
        if reset {
            self.state.lock().await.writes_since_maintain = 0;
            self.prune_and_enforce_budgets(store).await?;
        }
        let due = {
            let state = self.state.lock().await;
            now - state.last_maintained_at >= MAINTAIN_INTERVAL_MS
        };
        if due {
            self.maybe_maintain_store(store, now).await?;
        }
        Ok(atom_ids)
    }

    async fn maybe_maintain(&self, stores: &[StoreGuard]) -> Result<(), String> {
        let now = self.now();
        let due = {
            let mut state = self.state.lock().await;
            if now - state.last_maintained_at < MAINTAIN_INTERVAL_MS {
                false
            } else {
                state.last_maintained_at = now;
                true
            }
        };
        if !due {
            return Ok(());
        }
        for store in stores {
            let mut guard = store.lock().await;
            self.maintain(&mut guard, now).await?;
        }
        Ok(())
    }

    async fn maybe_maintain_store(
        &self,
        store: &mut AssociativeMemoryStore,
        now: i64,
    ) -> Result<(), String> {
        let due = {
            let mut state = self.state.lock().await;
            if now - state.last_maintained_at < MAINTAIN_INTERVAL_MS {
                false
            } else {
                state.last_maintained_at = now;
                true
            }
        };
        if due {
            self.maintain(store, now).await?;
        }
        Ok(())
    }

    async fn prune_and_enforce_budgets(
        &self,
        store: &mut AssociativeMemoryStore,
    ) -> Result<(), String> {
        let params = associative_memory_params();
        let edges = store.list_edges();
        let kept: HashSet<String> = prune_edges(&edges, &params)
            .into_iter()
            .map(|edge| edge.id)
            .collect();
        for edge in &edges {
            if !kept.contains(&edge.id) {
                store.delete_edge(&edge.id).await?;
            }
        }
        self.enforce_budgets(store, self.now()).await?;
        self.refresh_concepts(store).await
    }

    async fn refresh_concepts(&self, store: &mut AssociativeMemoryStore) -> Result<(), String> {
        let params = associative_memory_params();
        let cluster = cluster_incremental(
            &store.list_atoms(),
            &store.list_cooccurrences(),
            &store.list_concepts(),
            &params,
            self.now(),
        );
        for concept in &cluster.concepts {
            store.put_concept(concept.clone()).await?;
        }
        for edge in &cluster.edges {
            store.put_edge(edge.clone()).await?;
        }
        Ok(())
    }

    async fn consolidate(&self, store: &mut AssociativeMemoryStore, now: i64) -> Result<(), String> {
        let params = associative_memory_params();
        let pool = most_recent(
            &store
                .list_atoms()
                .into_iter()
                .filter(|atom| !atom.suppressed && atom.valid_until.is_none())
                .collect::<Vec<_>>(),
            CONSOLIDATION_POOL,
        );
        if pool.len() < 2 {
            return Ok(());
        }
        let mut touched = 0_usize;
        for left in 0..pool.len() {
            if touched >= CONSOLIDATION_PAIR_BUDGET {
                break;
            }
            for right in left + 1..pool.len() {
                if touched >= CONSOLIDATION_PAIR_BUDGET {
                    break;
                }
                if !shares_content_term(&pool[left], &pool[right]) {
                    continue;
                }
                let first = pool[left].id.clone();
                let second = pool[right].id.clone();
                let (a, b) = if first < second {
                    (first, second)
                } else {
                    (second, first)
                };
                let key = format!("{a}|{b}");
                let previous = store.get_cooccurrence(&key);
                let next = match previous {
                    Some(previous) => Cooccurrence {
                        count: previous.count + 1,
                        last_at: now,
                        ..previous
                    },
                    None => Cooccurrence {
                        key: key.clone(),
                        a,
                        b,
                        count: 1,
                        last_at: now,
                    },
                };
                store.put_cooccurrence(next).await?;
                touched += 1;
            }
        }
        if touched == 0 {
            return Ok(());
        }
        let cluster = cluster_incremental(
            &store.list_atoms(),
            &store.list_cooccurrences(),
            &store.list_concepts(),
            &params,
            now,
        );
        for concept in &cluster.concepts {
            store.put_concept(concept.clone()).await?;
        }
        for edge in &cluster.edges {
            store.put_edge(edge.clone()).await?;
        }
        let procedures = distill_procedures(
            &store.list_notes(),
            &store.list_atoms(),
            &store.list_procedures(),
            &params,
            now,
        );
        for procedure in &procedures.procedures {
            store.put_procedure(procedure.clone()).await?;
        }
        self.record_audit(
            AuditKind::Consolidate,
            json!({"scope": store.scope_key(), "pairs": touched}),
        )
        .await;
        Ok(())
    }

    async fn maintain(&self, store: &mut AssociativeMemoryStore, now: i64) -> Result<(), String> {
        let params = associative_memory_params();
        let mut pinned: HashSet<String> = HashSet::new();
        for atom in store.list_atoms() {
            if atom.pinned {
                pinned.insert(atom.id);
            }
        }
        for concept in store.list_concepts() {
            if concept.pinned {
                pinned.insert(concept.id);
            }
        }
        let salience_of: HashMap<String, f64> = store
            .list_atoms()
            .into_iter()
            .map(|atom| (atom.id, atom.base_weight))
            .collect();
        let neglected: HashSet<String> = store
            .list_atoms()
            .into_iter()
            .filter(|atom| now - atom.last_activated_at > params.motivated_forgetting_ms)
            .map(|atom| atom.id)
            .collect();
        let edges = store.list_edges();
        let decayed = restore_captured(
            &apply_sleep_downscale(
                &apply_decay(
                    &edges,
                    now,
                    &params,
                    Some(&pinned),
                    0.5 + params.concept_formation_rate,
                    Some(&salience_of),
                ),
                &params,
            ),
            &params,
        );
        let mut kept: HashMap<String, Edge> = HashMap::new();
        for edge in prune_edges(
            &apply_motivated_forgetting(&decayed, &neglected, &params),
            &params,
        ) {
            kept.insert(edge.id.clone(), edge);
        }
        for edge in &edges {
            if edge.kind == EdgeKind::Schema && now - edge.last_used_at > params.schema_ttl_ms {
                store.delete_edge(&edge.id).await?;
                continue;
            }
            match kept.get(&edge.id) {
                None => store.delete_edge(&edge.id).await?,
                Some(next) if next != edge => store.put_edge(next.clone()).await?,
                Some(_) => {}
            }
        }
        let coocs = store.list_cooccurrences();
        let threshold = cooc_cluster_threshold(&params) as f64;
        let kept_coocs: HashSet<String> = prune_cooccurrences(&coocs, now, &params, threshold)
            .into_iter()
            .map(|cooc| cooc.key)
            .collect();
        for cooc in &coocs {
            if !kept_coocs.contains(&cooc.key) {
                store.delete_cooccurrence(&cooc.key).await?;
            }
        }
        self.consolidate(store, now).await?;
        self.enforce_budgets(store, now).await
    }

    async fn enforce_budgets(
        &self,
        store: &mut AssociativeMemoryStore,
        now: i64,
    ) -> Result<(), String> {
        let params = associative_memory_params();
        let live: Vec<Atom> = store
            .list_atoms()
            .into_iter()
            .filter(|atom| !atom.suppressed && atom.valid_until.is_none())
            .collect();
        let max_atoms = self.atom_budget();
        if live.len() > max_atoms {
            let incident = store.list_edges();
            let mut ranked: Vec<(Atom, f64)> = live
                .iter()
                .filter(|atom| !atom.pinned)
                .map(|atom| {
                    let score = accessibility(atom, &incident, now, &params);
                    (atom.clone(), score)
                })
                .collect();
            ranked.sort_by(|left, right| {
                left.1
                    .partial_cmp(&right.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        compare_by_text(
                            (left.0.id.as_str(), left.0.text.as_str()),
                            (right.0.id.as_str(), right.0.text.as_str()),
                        )
                    })
            });
            let retired: Vec<Atom> = ranked
                .into_iter()
                .take(live.len() - max_atoms)
                .map(|(atom, _)| atom)
                .collect();
            if !retired.is_empty() {
                let retired_ids: HashSet<String> =
                    retired.iter().map(|atom| atom.id.clone()).collect();
                self.retire_atoms(store, &retired_ids, self.now()).await?;
                self.record_audit(
                    AuditKind::EvictAtoms,
                    json!({
                        "scope": store.scope_key(),
                        "count": retired_ids.len(),
                        "ids": retired_ids.iter().take(AUDIT_ID_LIMIT).cloned().collect::<Vec<_>>(),
                    }),
                )
                .await;
            }
        }
        let notes = store.list_notes();
        let notes_len = notes.len();
        let max_notes = self.note_budget();
        if notes_len <= max_notes {
            return Ok(());
        }
        let live_notes: HashSet<String> = store.list_atoms().into_iter().map(|atom| atom.note_id).collect();
        let mut removable: Vec<NoteRecord> = notes
            .into_iter()
            .filter(|note| !live_notes.contains(&note.id))
            .collect();
        removable.sort_by_key(|note| note.created_at);
        let removable: Vec<NoteRecord> = removable
            .into_iter()
            .take(notes_len - max_notes)
            .collect();
        for note in &removable {
            store.delete_note(&note.id).await?;
        }
        if !removable.is_empty() {
            self.record_audit(
                AuditKind::EvictNotes,
                json!({"scope": store.scope_key(), "count": removable.len()}),
            )
            .await;
        }
        Ok(())
    }

    async fn induce_forgetting(
        &self,
        store: &mut AssociativeMemoryStore,
        new_atoms: &[Atom],
    ) -> Result<(), String> {
        let params = associative_memory_params();
        if new_atoms.is_empty() {
            return Ok(());
        }
        let active_ids: HashSet<String> = new_atoms.iter().map(|atom| atom.id.clone()).collect();
        let mut competing: HashSet<String> = HashSet::new();
        for atom in store.list_atoms() {
            if active_ids.contains(&atom.id) {
                continue;
            }
            if !new_atoms
                .iter()
                .any(|candidate| shares_content_term(candidate, &atom))
            {
                continue;
            }
            competing.insert(atom.id);
        }
        if competing.is_empty() {
            return Ok(());
        }
        let mut edges: Vec<Edge> = store
            .list_edges()
            .into_iter()
            .filter(|edge| edge.kind == EdgeKind::Hebbian && competing.contains(&edge.from))
            .collect();
        edges.sort_by(|left, right| {
            right
                .weight
                .partial_cmp(&left.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.id.encode_utf16().cmp(right.id.encode_utf16()))
        });
        for edge in edges.into_iter().take(params.interference_max_edges as usize) {
            store
                .put_edge(Edge {
                    weight: edge.weight * params.interference_forgetting,
                    ..edge
                })
                .await?;
        }
        Ok(())
    }

    async fn update_cooccurrence(
        &self,
        store: &mut AssociativeMemoryStore,
        new_atoms: &[Atom],
        context: &[Atom],
    ) -> Result<(), String> {
        let now = self.now();
        let window = most_recent(context, COOC_CONTEXT_LIMIT);
        for a in new_atoms {
            for b in &window {
                if a.id == b.id {
                    continue;
                }
                let (x, y) = if a.id < b.id {
                    (a.id.clone(), b.id.clone())
                } else {
                    (b.id.clone(), a.id.clone())
                };
                let key = format!("{x}|{y}");
                let previous = store.get_cooccurrence(&key);
                let next = match previous {
                    Some(previous) => Cooccurrence {
                        count: previous.count + 1,
                        last_at: now,
                        ..previous
                    },
                    None => Cooccurrence {
                        key: key.clone(),
                        a: x,
                        b: y,
                        count: 1,
                        last_at: now,
                    },
                };
                store.put_cooccurrence(next).await?;
            }
        }
        Ok(())
    }
}

fn js_slice_limit(length: usize, limit: i64) -> usize {
    if limit >= 0 {
        return limit as usize;
    }
    length.saturating_sub(limit.unsigned_abs() as usize)
}

fn js_iso_time(timestamp: i64) -> String {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| {
                    ctx.eval::<String, _>(format!("new Date({timestamp}).toISOString()"))
                })
            })
        })
        .unwrap_or_else(|_| panic!("Invalid time value"))
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn empty_source_counts() -> serde_json::Map<String, Value> {
    let mut counts = serde_json::Map::new();
    for source in ["user", "tool", "external", "legacy"] {
        counts.insert(source.to_string(), Value::from(0_i64));
    }
    counts
}

fn gating_mode_name(mode: GatingMode) -> String {
    match mode {
        GatingMode::Encode => "encode",
        GatingMode::Retrieve => "retrieve",
        GatingMode::Consolidate => "consolidate",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::NodeType;
    use std::sync::atomic::AtomicI64;

    struct Flags(bool);
    impl MemoryFlags for Flags {
        fn enabled(&self, _id: &str) -> bool {
            self.0
        }
    }

    struct StubLegacyStore {
        entries: Mutex<Vec<LegacyMemoryEntry>>,
        read_only: AtomicBool,
    }
    impl StubLegacyStore {
        fn new(entries: Vec<LegacyMemoryEntry>) -> Self {
            Self {
                entries: Mutex::new(entries),
                read_only: AtomicBool::new(false),
            }
        }
    }
    impl MemoryLegacyStore for StubLegacyStore {
        fn list(&self) -> Vec<String> {
            self.entries
                .lock()
                .unwrap()
                .iter()
                .map(|entry| entry.path.clone())
                .collect()
        }
        fn get(&self, path: &str) -> Option<LegacyMemoryEntry> {
            self.entries
                .lock()
                .unwrap()
                .iter()
                .find(|entry| entry.path == path)
                .cloned()
        }
        fn set_read_only(&self, value: bool) {
            self.read_only.store(value, Ordering::SeqCst);
        }
    }

    fn clock(start: i64) -> (Arc<AtomicI64>, Arc<dyn Fn() -> i64 + Send + Sync>) {
        let now = Arc::new(AtomicI64::new(start));
        let for_fn = now.clone();
        (now, Arc::new(move || for_fn.load(Ordering::SeqCst)))
    }

    fn deps(dir: &Path, flags: bool) -> MemoryDeps {
        MemoryDeps {
            home_dir: dir.to_path_buf(),
            store_dir: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            session_id: "session-1".to_string(),
            session_cwd: dir.to_path_buf(),
            logger: None,
            flags: Some(Arc::new(Flags(flags))),
            reminders: None,
            history: None,
            legacy_store: None,
        }
    }

    fn params() -> crate::features::associativeMemory::types::AssociativeMemoryParams {
        associative_memory_params()
    }

    async fn service(dir: &Path, flags: bool, start: i64) -> (Arc<AtomicI64>, AssociativeMemoryService) {
        let (now, clock) = clock(start);
        let service = AssociativeMemoryService::new(deps(dir, flags));
        (now, service.with_now_override(clock))
    }

    async fn write(service: &AssociativeMemoryService, text: &str) -> WriteResult {
        service
            .write_note(text, MemoryScopeSelector::Project, MemorySource::User)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_disabled_service_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), false, 1_000).await;
        let result = write(&service, "alpha beta").await;
        assert_eq!(result.decision, Some(WriteDecision::Noop));
        assert!(result.note_id.is_empty());
        assert!(result.atom_ids.is_empty());

        let recalled = service
            .recall(&RecallQuery {
                cue: "alpha".to_string(),
                limit: None,
                max_tokens: None,
                include_history: false,
                scope: None,
                reinforce: None,
            })
            .await
            .unwrap();
        assert!(recalled.items.is_empty());
        assert_eq!(service.stats(None).await.atoms, 0);
    }

    #[tokio::test]
    async fn writing_trains_atoms_and_recall_surfaces_them() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        let first = write(&service, "the snapshot store writes json files").await;
        assert_eq!(first.decision, Some(WriteDecision::Added));
        assert_eq!(first.atom_ids.len(), 1);

        let recalled = service
            .recall(&RecallQuery {
                cue: "snapshot json".to_string(),
                limit: Some(4),
                max_tokens: Some(600),
                include_history: false,
                scope: None,
                reinforce: None,
            })
            .await
            .unwrap();
        assert!(!recalled.items.is_empty());
        assert_eq!(recalled.items[0].note_id, first.note_id);
        assert!(recalled.items[0].score > 0.0);
        assert!(!recalled.trace.is_empty());
    }

    #[tokio::test]
    async fn a_note_with_the_same_terms_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        let first = write(&service, "alpha beta gamma").await;
        let second = write(&service, "gamma beta alpha").await;
        assert_eq!(first.decision, Some(WriteDecision::Added));
        assert_eq!(second.decision, Some(WriteDecision::Noop));
        assert_eq!(second.note_id, first.note_id);
        assert!(second.atom_ids.is_empty());
        assert_eq!(service.stats(None).await.notes, 1);
    }

    #[tokio::test]
    async fn pin_and_forget_move_an_atom_out_of_recall() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        let written = write(&service, "the cache ttl is sixty seconds").await;
        let atom_id = written.atom_ids[0].clone();

        service.pin(&atom_id).await.unwrap();
        let pinned = service.stats(None).await;
        assert_eq!(pinned.pinned, 1);
        assert_eq!(pinned.atoms, 1);

        service.forget(&atom_id).await.unwrap();
        let stats = service.stats(None).await;
        assert_eq!(stats.atoms, 0, "a forgotten atom is excluded from the count");
        assert_eq!(stats.suppressed, 1);

        let recalled = service
            .recall(&RecallQuery {
                cue: "cache ttl".to_string(),
                limit: Some(4),
                max_tokens: Some(600),
                include_history: false,
                scope: None,
                reinforce: None,
            })
            .await
            .unwrap();
        assert!(recalled.items.is_empty());
    }

    #[tokio::test]
    async fn list_stats_and_export_report_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        write(&service, "alpha beta").await;
        write(&service, "alpha delta").await;

        let listed = service
            .list(&ListOptions {
                kind: Some(NodeType::Atom),
                limit: Some(10),
                scope: None,
            })
            .await
            .unwrap();
        assert_eq!(listed.atoms.len(), 2);
        assert!(listed.concepts.is_empty());

        let stats = service.stats(Some(MemoryScopeSelector::Project)).await;
        assert_eq!(stats.atoms, 2);
        assert_eq!(stats.notes, 2);
        assert!(stats.edges >= 1, "the shared term trains a hebbian edge");
        assert_eq!(stats.gating_mode.as_deref(), Some("retrieve"));
        assert_eq!(
            stats.source_counts.get("user").and_then(Value::as_i64),
            Some(2)
        );

        let exported = service.export(Some(MemoryScopeSelector::Project)).await.unwrap();
        assert!(exported.path.ends_with("memory-export.json"));
        assert_eq!(exported.atoms, 2);
        let body = fs::read_to_string(&exported.path).await.unwrap();
        assert!(body.contains("\"atoms\""));
        assert!(body.contains("\"audit\""));
    }

    #[tokio::test]
    async fn audit_records_the_write_path() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        write(&service, "alpha beta").await;
        write(&service, "alpha beta").await;
        let entries = service.audit(Some(20)).await.unwrap();
        let kinds = entries
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>();
        assert!(kinds.contains(&AuditKind::WriteAllowed));
        assert!(kinds.contains(&AuditKind::WriteNoop));
        assert_eq!(entries.first().unwrap().kind, AuditKind::WriteNoop);
    }

    #[tokio::test]
    async fn rebuild_replays_every_note() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        write(&service, "alpha beta gamma").await;
        write(&service, "delta epsilon zeta").await;
        service.rebuild(Some(MemoryScopeSelector::All)).await.unwrap();
        let stats = service.stats(None).await;
        assert_eq!(stats.notes, 2);
        assert_eq!(stats.atoms, 2, "each note re-encodes into one atom");
        let entries = service.audit(Some(20)).await.unwrap();
        assert!(entries.iter().any(|entry| entry.kind == AuditKind::Rebuild));
    }

    #[tokio::test]
    async fn reconsolidate_extends_a_recalled_atom() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        let written = write(&service, "the auth service retries twice").await;
        let recalled = service
            .recall(&RecallQuery {
                cue: "auth retries".to_string(),
                limit: Some(4),
                max_tokens: Some(600),
                include_history: false,
                scope: None,
                reinforce: Some(true),
            })
            .await
            .unwrap();
        assert!(!recalled.items.is_empty());
        let target = recalled.items[0].id.clone();
        let note_id = recalled.items[0].note_id.clone();

        let result = service
            .reconsolidate(
                Some(&target),
                "the auth service retries twice before failing hard",
            )
            .await
            .unwrap();
        assert!(!result.atom_ids.is_empty());
        assert_ne!(result.note_id, note_id, "the evidence lands on a new note");
        let stats = service.stats(None).await;
        assert_eq!(stats.notes, 2);
        let _ = written;
    }

    #[tokio::test]
    async fn code_block_returns_the_default_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        let block = service.code_block().await;
        assert_eq!(block, crate::features::associativeMemory::code::defaults::DEFAULT_GUIDANCE);
    }

    #[tokio::test]
    async fn legacy_memories_are_imported_once() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = Arc::new(StubLegacyStore::new(vec![
            LegacyMemoryEntry {
                path: "global/a.md".to_string(),
                scope: "global".to_string(),
                body: "legacy alpha note".to_string(),
            },
            LegacyMemoryEntry {
                path: "projects/b.md".to_string(),
                scope: "project".to_string(),
                body: "legacy beta note".to_string(),
            },
        ]));
        let (now, clock) = clock(1_000);
        let mut service_deps = deps(dir.path(), true);
        service_deps.legacy_store = Some(legacy.clone());
        let service = AssociativeMemoryService::new(service_deps).with_now_override(clock);
        let _ = now;

        write(&service, "trigger the import").await;
        let all = service
            .list(&ListOptions {
                kind: None,
                limit: Some(20),
                scope: Some(MemoryScopeSelector::All),
            })
            .await
            .unwrap();
        assert_eq!(all.atoms.len(), 3, "two legacy notes plus the trigger");
        assert!(
            legacy.read_only.load(Ordering::SeqCst),
            "the legacy store is frozen after the import"
        );
    }

    #[tokio::test]
    async fn without_env_configuration_embeddings_stay_off() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        let embeddings = service.ensure_embeddings().await.unwrap();
        assert!(embeddings.is_none());
        let entries = service.audit(Some(20)).await.unwrap();
        assert!(!entries.iter().any(|entry| entry.kind == AuditKind::EmbeddingsBlocked));
    }

    #[tokio::test]
    async fn the_store_cache_reuses_one_handle_per_scope() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path(), true, 1_000).await;
        write(&service, "alpha beta").await;
        let project = service.project_scope_key().await.unwrap();
        let first = service.ensure_store(&project).await.unwrap();
        let second = service.ensure_store(&project).await.unwrap();
        assert!(Arc::ptr_eq(&first.store, &second.store));
        drop(first);
        drop(second);
        {
            let cache = service.stores.lock().unwrap();
            assert_eq!(cache[&project].refs, 0, "both guards released");
        }
    }

    #[tokio::test]
    async fn budgets_and_params_match_the_v2_defaults() {
        assert_eq!(MAX_ATOMS, 4000);
        assert_eq!(MAX_NOTES, 8000);
        assert_eq!(MAX_OPEN_STORES, 8);
        assert_eq!(WORKING_CAPACITY, 24);
        assert_eq!(WORKING_TTL_MS, 6 * 60 * 60 * 1000);
        assert_eq!(RECONSOLIDATE_THRESHOLD, 0.5);
        assert_eq!(EMBEDDINGS_RETRY_MS, 60_000);
        assert_eq!(MAINTAIN_EVERY_WRITES, 8);
        assert_eq!(COOC_CONTEXT_LIMIT, 24);
        assert_eq!(ACTIVATION_COOC_LIMIT, 8);
        assert_eq!(AUDIT_ID_LIMIT, 50);
        assert_eq!(CONSOLIDATION_POOL, 16);
        assert_eq!(CONSOLIDATION_PAIR_BUDGET, 96);
        assert_eq!(params().replay_strata, 3);
        let _ = most_recent(&[], 8);
    }

    #[tokio::test]
    async fn most_recent_keeps_the_newest_atoms_in_v2_order() {
        let atoms = vec![
            Atom {
                id: "a".into(),
                last_activated_at: 10,
                text: "beta".into(),
                ..dummy_atom("a")
            },
            Atom {
                id: "b".into(),
                last_activated_at: 20,
                text: "alpha".into(),
                ..dummy_atom("b")
            },
            Atom {
                id: "c".into(),
                last_activated_at: 20,
                text: "alpha".into(),
                ..dummy_atom("c")
            },
        ];
        let kept = most_recent(&atoms, 2);
        assert_eq!(
            kept.iter().map(|atom| atom.id.as_str()).collect::<Vec<_>>(),
            vec!["b", "c"],
            "ties break on text then id"
        );
        assert_eq!(most_recent(&atoms, 0).len(), 0);
    }

    fn dummy_atom(id: &str) -> Atom {
        Atom {
            id: id.into(),
            note_id: "note-1".into(),
            scope_key: "scope".into(),
            node_type: NodeType::Atom,
            text: String::new(),
            span_start: 0,
            span_end: 0,
            terms: vec!["term".into()],
            vector: serde_json::Map::new(),
            dense_vector: None,
            concept_id: None,
            base_weight: 1.0,
            confidence: 0.5,
            replay_priority: 0,
            pinned: false,
            suppressed: false,
            valid_from: 0,
            valid_until: None,
            is_deleted: false,
            created_at: 0,
            last_activated_at: 0,
        }
    }

    #[test]
    fn the_reminder_variant_matches_v2() {
        assert_eq!(ASSOCIATIVE_MEMORY_SIGNAL_VARIANT, "associative_memory_signal");
        assert_eq!(STORE_ROOT_NAME, "associative-memory");
        assert_eq!(CODE_FILE_NAME, "code.md");
        assert_eq!(GLOBAL_SCOPE_KEY, "global");
    }
}
