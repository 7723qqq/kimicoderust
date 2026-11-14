use std::error::Error;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::stream::{self, Stream};
use futures_util::{FutureExt, StreamExt, pin_mut};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::log::log::ILogService;
use crate::agent::blob::agent_blob_service::IAgentBlobService;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::persistence::interface::append_log_store::{
    AppendLogOptions, AppendLogReadOptions, AppendLogTruncation, IAppendLogStore,
};
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::persistence::interface::storage::{
    IFileSystemStorageService, STORAGE_CORRUPTED, StorageError,
};
use crate::session::session_context::session_context::ISessionContext;

use super::errors::{
    RECORDS_WRITE_FAILED, WIRE_UNKNOWN_RECORD, WireError,
};
use super::human::is_human_record_type;
use super::journal::{
    AgentJournalRef, IAgentJournal, SwitchBranchInput, SwitchedBranch,
};
use super::migration::migration::{
    MIGRATE_V1_4_TO_V1_5, WIRE_MIN_READER_VERSION, WIRE_PROTOCOL_VERSION,
    WireMigration, assert_wire_reader_version, is_newer_wire_version, migrate_wire_record,
    resolve_wire_migrations,
};
use super::record::{
    AGENT_WIRE_RECORD_KEY, PartsTransformer, RecordDehydrator, WireAsyncError, WireRecord,
    create_wire_metadata_record, is_wire_metadata_record, is_wire_record,
};
use super::repair::{WireJournalRepairServices, repair_wire_journal};
use super::tree::fork::{
    UndoSwitchRecords, build_undo_switch_records, compute_fork_line, restorable_chain,
};
use super::tree::tree::{
    AGENT_SWITCHED_TYPE, BranchBase, MAIN_BRANCH, WireLine, WireTree, active_chain,
    branch_for_line, parse_tree,
};
use super::wire::{IWireService, WireRestoreChains};

type AppendListener = Arc<dyn Fn(&WireRecord) + Send + Sync>;
type PersistQueue = Shared<BoxFuture<'static, ()>>;
type RecordStream = Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send>>;
type EntryStream = Pin<Box<dyn Stream<Item = Result<WireLine, WireAsyncError>> + Send>>;

struct PendingRepair {
    records: Vec<Value>,
    truncation: AppendLogTruncation,
}

#[derive(Default)]
struct WireServiceState {
    lines: u64,
    last_clear_line: Option<u64>,
    append_listeners: IndexMap<u64, AppendListener>,
    next_listener_id: u64,
    persist_queue: Option<PersistQueue>,
    queue_generation: u64,
    pending_repair: Option<PendingRepair>,
    pending_rewrite: Option<Vec<WireRecord>>,
    persist_error: Option<WireAsyncError>,
    tree_snapshot: Option<WireTree>,
    write_generation: u64,
    last_read_line_count: u64,
    mode_two_entries: Vec<WireLine>,
}

struct WireServiceInner {
    wire_scope: String,
    agent_id: String,
    log: Arc<dyn IAppendLogStore>,
    blob_service: Arc<dyn IAgentBlobService>,
    storage: Arc<dyn IFileSystemStorageService>,
    logger: Arc<dyn ILogService>,
    telemetry: Arc<dyn ITelemetryService>,
    session_locks: Arc<dyn ISessionLockService>,
    session_context: Arc<dyn ISessionContext>,
    state: Mutex<WireServiceState>,
    _registrations: Vec<Box<dyn IDisposable>>,
}

#[derive(Clone)]
pub struct WireService {
    inner: Arc<WireServiceInner>,
}

impl WireService {
    pub fn new(
        scope_context: Arc<dyn IAgentScopeContext>,
        log: Arc<dyn IAppendLogStore>,
        blob_service: Arc<dyn IAgentBlobService>,
        storage: Arc<dyn IFileSystemStorageService>,
        logger: Arc<dyn ILogService>,
        telemetry: Arc<dyn ITelemetryService>,
        session_locks: Arc<dyn ISessionLockService>,
        session_context: Arc<dyn ISessionContext>,
    ) -> Self {
        let wire_scope = scope_context.scope();
        let agent_id = scope_context.agent_id();
        let mut registrations = Vec::new();
        registrations.push(log.acquire(&wire_scope, AGENT_WIRE_RECORD_KEY));
        let guard_locks = session_locks.clone();
        let guard_context = session_context.clone();
        registrations.push(log.bind_write_guard(
            &wire_scope,
            AGENT_WIRE_RECORD_KEY,
            Arc::new(move || guard_locks.try_hold(&guard_context.session_id())),
        ));
        Self {
            inner: Arc::new(WireServiceInner {
                wire_scope,
                agent_id,
                log,
                blob_service,
                storage,
                logger,
                telemetry,
                session_locks,
                session_context,
                state: Mutex::new(WireServiceState::default()),
                _registrations: registrations,
            }),
        }
    }

    pub async fn seal(&self) -> Result<(), WireAsyncError> {
        let source = self.inner.log.read(
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
            Some(tolerate_truncation()),
        );
        pin_mut!(source);
        if let Some(record) = source.next().await {
            record?;
            return Ok(());
        }
        drop(source);
        self.append_record_low(create_wire_metadata_record(None))
    }

    pub fn append_record(&self, record: WireRecord, dehydrate: Option<RecordDehydrator>) {
        let fast = {
            let state = self.inner.state.lock();
            state.pending_repair.is_none()
                && state.pending_rewrite.is_none()
                && dehydrate.is_none()
                && state.persist_queue.is_none()
        };
        if fast {
            if let Err(error) = self.append_record_low(record) {
                self.log_append_error(&error);
                on_unexpected_error(error);
            }
            return;
        }

        let blob_service = self.inner.blob_service.clone();
        let transform: PartsTransformer = Arc::new(move |parts| {
            let blob_service = blob_service.clone();
            async move { blob_service.offload_parts(parts).await }.boxed()
        });
        let (previous, generation) = {
            let mut state = self.inner.state.lock();
            state.queue_generation += 1;
            (state.persist_queue.clone(), state.queue_generation)
        };
        let this = self.clone();
        let queued = async move {
            if let Some(previous) = previous {
                previous.await;
            }
            if let Err(error) = this
                .run_queued_append(record, dehydrate, transform)
                .await
            {
                this.log_append_error(&error);
                on_unexpected_error(error);
            }
            let mut state = this.inner.state.lock();
            if state.queue_generation == generation {
                state.persist_queue = None;
            }
        }
        .boxed()
        .shared();
        self.inner.state.lock().persist_queue = Some(queued.clone());
        tokio::spawn(queued);
    }

    async fn run_queued_append(
        &self,
        record: WireRecord,
        dehydrate: Option<RecordDehydrator>,
        transform: PartsTransformer,
    ) -> Result<(), WireAsyncError> {
        if self.inner.state.lock().pending_repair.is_some() {
            self.repair_pending_journal().await?;
        }
        if self.inner.state.lock().pending_rewrite.is_some() {
            self.rewrite_pending_journal().await?;
        }
        let output = match dehydrate {
            Some(dehydrate) => dehydrate(record, transform).await?,
            None => record,
        };
        self.append_record_low(output)
    }

    fn log_append_error(&self, error: &WireAsyncError) {
        self.inner.logger.error(
            "wire append failed",
            Some(json!({ "agentId": self.inner.agent_id, "error": error.to_string() })),
        );
    }

    pub fn read_journal(&self) -> RecordStream {
        Box::pin(self.read_entries().map(|entry| {
            entry.map(|entry| entry.record)
        }))
    }

    pub fn journal_ref(&self) -> AgentJournalRef {
        let state = self.inner.state.lock();
        AgentJournalRef {
            tree: self.inner.wire_scope.clone(),
            branch: state
                .tree_snapshot
                .as_ref()
                .map_or(MAIN_BRANCH, |tree| tree.active_branch.as_str())
                .to_string(),
        }
    }

    pub fn append(&self, record: WireRecord, dehydrate: Option<RecordDehydrator>) {
        self.append_record(record, dehydrate);
    }

    pub fn on_did_append_record(&self, listener: AppendListener) -> Box<dyn IDisposable> {
        let id = {
            let mut state = self.inner.state.lock();
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.append_listeners.insert(id, listener);
            id
        };
        let inner = self.inner.clone();
        to_disposable(move || {
            inner.state.lock().append_listeners.remove(&id);
        })
    }

    pub fn read(&self) -> RecordStream {
        let this = self.clone();
        Box::pin(
            stream::once(async move {
                let entries = this.read_stable_entries().await?;
                let last_line = entries.last().map_or(0, |entry| entry.line);
                let tree = parse_tree(&entries, last_line);
                this.inner.state.lock().tree_snapshot = Some(tree.clone());
                this.report_tree_diagnostics(&tree);
                let records = active_chain(&entries, &tree)?
                    .into_iter()
                    .map(|entry| Ok(entry.record))
                    .collect::<Vec<_>>();
                Ok::<Vec<Result<WireRecord, WireAsyncError>>, WireAsyncError>(records)
            })
            .map(|result| match result {
                Ok(records) => stream::iter(records).boxed(),
                Err(error) => stream::iter(vec![Err(error)]).boxed(),
            })
            .flatten(),
        )
    }

    pub fn read_raw(&self) -> RecordStream {
        self.read_journal()
    }

    pub fn read_human_chain(&self) -> Result<Vec<WireLine>, Box<dyn Error + Send + Sync>> {
        let (entries, lines) = {
            let state = self.inner.state.lock();
            (state.mode_two_entries.clone(), state.lines)
        };
        let tree = parse_tree(&entries, lines);
        active_chain(&entries, &tree)
    }

    pub fn read_restorable(&self) -> RecordStream {
        let this = self.clone();
        Box::pin(
            stream::once(async move {
                let entries = this.read_stable_entries().await?;
                let last_line = entries.last().map_or(0, |entry| entry.line);
                let tree = parse_tree(&entries, last_line);
                this.inner.state.lock().tree_snapshot = Some(tree.clone());
                this.report_tree_diagnostics(&tree);
                let records = restorable_chain(&entries, &tree)?
                    .into_iter()
                    .map(|entry| Ok(entry.record))
                    .collect::<Vec<_>>();
                Ok::<Vec<Result<WireRecord, WireAsyncError>>, WireAsyncError>(records)
            })
            .map(|result| match result {
                Ok(records) => stream::iter(records).boxed(),
                Err(error) => stream::iter(vec![Err(error)]).boxed(),
            })
            .flatten(),
        )
    }

    pub async fn read_restore_chains(&self) -> Result<WireRestoreChains, WireAsyncError> {
        let entries = self.read_stable_entries().await?;
        let last_line = entries.last().map_or(0, |entry| entry.line);
        let tree = parse_tree(&entries, last_line);
        self.inner.state.lock().tree_snapshot = Some(tree.clone());
        self.report_tree_diagnostics(&tree);
        Ok(WireRestoreChains {
            restorable: restorable_chain(&entries, &tree)?
                .into_iter()
                .map(|entry| entry.record)
                .collect(),
            journal: entries.into_iter().map(|entry| entry.record).collect(),
        })
    }

    pub async fn switch_branch(
        &self,
        input: SwitchBranchInput,
    ) -> Result<SwitchedBranch, WireAsyncError> {
        let mut entries = None;
        for _ in 0..2 {
            if entries.is_some() {
                break;
            }
            self.drain_persisted().await?;
            let read = self.read_stable_entries().await?;
            let state = self.inner.state.lock();
            if state.lines == state.last_read_line_count {
                drop(state);
                entries = Some(read);
            }
        }
        let Some(entries) = entries else {
            let state = self.inner.state.lock();
            let mut details = Map::new();
            details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
            details.insert("lines".to_string(), state.lines.into());
            details.insert("read".to_string(), state.last_read_line_count.into());
            return Err(Box::new(WireError::with_options(
                RECORDS_WRITE_FAILED,
                "Wire journal changed while switching branches",
                options(details),
            )));
        };
        let last_line = entries.last().map_or(0, |entry| entry.line);
        let tree = parse_tree(&entries, last_line);
        self.report_tree_diagnostics(&tree);
        let fork_line = compute_fork_line(
            &active_chain(&entries, &tree)?,
            &tree.paired_legacy_undo_lines,
            input.turns,
        )?;
        let base = BranchBase {
            branch: branch_for_line(&tree, fork_line),
            line: fork_line,
        };
        let branch = format!("b{}", tree.edges.len() + 1);
        let edge_line = self.inner.state.lock().lines + 1;
        let records = build_undo_switch_records(super::tree::fork::BuildUndoSwitchRecordsInput {
            agent_id: self.inner.agent_id.clone(),
            branch: branch.clone(),
            reason: input.reason.unwrap_or_else(|| "undo".to_string()),
            base: base.clone(),
            turns: input.turns,
            edge_line,
            from_turn_id: input.from_turn_id,
            time: chrono::Utc::now().timestamp_millis(),
        });
        self.append_record(records.switched.clone(), None);
        self.append_record(records.legacy_undo.clone(), None);
        self.append_record(records.undone.clone(), None);
        self.flush().await?;
        self.assert_switch_triple_appended(&records, edge_line)
            .await?;
        let appended = [
            WireLine {
                record: records.switched.clone(),
                line: edge_line,
            },
            WireLine {
                record: records.legacy_undo.clone(),
                line: edge_line + 1,
            },
            WireLine {
                record: records.undone.clone(),
                line: edge_line + 2,
            },
        ];
        let mut all_entries = entries;
        all_entries.extend(appended);
        self.inner.state.lock().tree_snapshot = Some(parse_tree(&all_entries, edge_line + 2));
        Ok(SwitchedBranch {
            branch,
            base,
            edge_line,
            fork_line,
        })
    }

    async fn assert_switch_triple_appended(
        &self,
        records: &UndoSwitchRecords,
        edge_line: u64,
    ) -> Result<(), WireAsyncError> {
        let source = self.inner.log.read(
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
            Some(tolerate_truncation()),
        );
        pin_mut!(source);
        let mut tail = Vec::new();
        let mut total = 0;
        while let Some(record) = source.next().await {
            total += 1;
            tail.push(record?);
            if tail.len() > 3 {
                tail.remove(0);
            }
        }
        let expected = [
            Value::Object(records.switched.clone()),
            Value::Object(records.legacy_undo.clone()),
            Value::Object(records.undone.clone()),
        ];
        let matches = total == edge_line + 2
            && tail.len() == 3
            && tail
                .iter()
                .zip(expected.iter())
                .all(|(record, expected)| values_match(record, expected));
        if matches {
            return Ok(());
        }
        let mut details = Map::new();
        details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
        details.insert("lines".to_string(), total.into());
        details.insert("edgeLine".to_string(), edge_line.into());
        Err(Box::new(WireError::with_options(
            RECORDS_WRITE_FAILED,
            "Wire journal changed while the undo switch triple was appended",
            options(details),
        )))
    }

    async fn read_stable_entries(&self) -> Result<Vec<WireLine>, WireAsyncError> {
        for _ in 0..2 {
            let generation = self.inner.state.lock().write_generation;
            let source = self.read_entries();
            pin_mut!(source);
            let mut entries = Vec::new();
            while let Some(entry) = source.next().await {
                entries.push(entry?);
            }
            if self.inner.state.lock().write_generation == generation {
                return Ok(entries);
            }
        }
        let mut details = Map::new();
        details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
        Err(Box::new(WireError::with_options(
            RECORDS_WRITE_FAILED,
            "Wire journal kept rewriting while being read",
            options(details),
        )))
    }

    fn report_tree_diagnostics(&self, tree: &WireTree) {
        for line in &tree.diagnostics.malformed_switch_lines {
            let mut details = Map::new();
            details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
            details.insert("type".to_string(), AGENT_SWITCHED_TYPE.into());
            details.insert("line".to_string(), (*line).into());
            on_unexpected_error(Box::new(WireError::with_options(
                WIRE_UNKNOWN_RECORD,
                "Malformed agent.switched record ignored during tree projection",
                options(details),
            )));
        }
        for branch in &tree.diagnostics.duplicate_branches {
            let mut details = Map::new();
            details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
            details.insert("type".to_string(), AGENT_SWITCHED_TYPE.into());
            details.insert("branch".to_string(), branch.clone().into());
            on_unexpected_error(Box::new(WireError::with_options(
                WIRE_UNKNOWN_RECORD,
                format!(
                    "Duplicate agent.switched branch '{branch}' ignored during tree projection"
                ),
                options(details),
            )));
        }
    }

    pub fn branches(&self) -> Vec<String> {
        let state = self.inner.state.lock();
        match &state.tree_snapshot {
            None => vec![MAIN_BRANCH.to_string()],
            Some(tree) => tree
                .segments
                .iter()
                .map(|segment| segment.branch.clone())
                .collect(),
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.inner.state.lock().lines + 1
    }

    pub async fn settled(&self) -> Result<(), WireAsyncError> {
        self.drain_persisted().await
    }

    fn read_entries(&self) -> EntryStream {
        let this = self.clone();
        Box::pin(
            stream::once(async move {
                let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
                tokio::spawn(async move {
                    if let Err(error) = this.read_entries_inner(sender.clone()).await {
                        let _ = sender.send(Err(error)).await;
                    }
                });
                let stream: EntryStream = Box::pin(stream::poll_fn(move |context| receiver.poll_recv(context)));
                stream
            })
            .flatten(),
        )
    }

    async fn read_entries_inner(
        &self,
        sender: tokio::sync::mpsc::Sender<Result<WireLine, WireAsyncError>>,
    ) -> Result<(), WireAsyncError> {
        let truncation = Arc::new(Mutex::new(None::<AppendLogTruncation>));
        let truncation_sink = truncation.clone();
        let source = self.inner.log.read(
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
            Some(AppendLogReadOptions {
                on_truncate: Some(Arc::new(move |info| {
                    *truncation_sink.lock() = Some(info);
                })),
            }),
        );
        pin_mut!(source);
        let mut migrations: Vec<&'static dyn WireMigration> = Vec::new();
        let mut rewritten_records = None::<Vec<WireRecord>>;
        let mut newer_wire_version = false;
        let mut record_index = 0;
        let mut line_count = 0;
        let mut has_records = false;
        let mut legacy_plan_revision_migrated = false;
        let mut mode_two_entries = Vec::new();
        let mode_two_length_at_start = self.inner.state.lock().mode_two_entries.len();

        while let Some(candidate) = source.next().await {
            let candidate = candidate?;
            line_count += 1;
            self.inner.state.lock().lines = line_count;
            if !is_wire_record(&candidate) {
                self.report_skipped_record(None, record_index, true);
                record_index += 1;
                continue;
            }
            let source_record = candidate.as_object().cloned().unwrap();
            if source_record.get("type").and_then(Value::as_str) == Some("context.clear") {
                self.inner.state.lock().last_clear_line = Some(line_count);
            }
            if !has_records {
                has_records = true;
                if source_record.get("type").and_then(Value::as_str) != Some("metadata") {
                    rewritten_records = Some(vec![create_wire_metadata_record(None)]);
                    migrations = vec![&MIGRATE_V1_4_TO_V1_5];
                } else if !is_wire_metadata_record(&source_record) {
                    let mut details = Map::new();
                    details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
                    details.insert("key".to_string(), AGENT_WIRE_RECORD_KEY.into());
                    return Err(Box::new(StorageError::with_options(
                        STORAGE_CORRUPTED,
                        "Agent wire metadata is malformed",
                        options(details),
                    )));
                } else {
                    assert_wire_reader_version(
                        source_record
                            .get("min_protocol_version")
                            .map(|value| value.as_str().unwrap()),
                    )?;
                    let protocol_version = source_record
                        .get("protocol_version")
                        .and_then(Value::as_str)
                        .unwrap();
                    if is_newer_wire_version(protocol_version) {
                        newer_wire_version = true;
                    } else {
                        migrations = resolve_wire_migrations(protocol_version)?;
                        if protocol_version != WIRE_PROTOCOL_VERSION {
                            rewritten_records = Some(Vec::new());
                        }
                    }
                }
            }

            let migrated_record = migrate_wire_record(source_record, &migrations);
            let mut record = migrated_record;
            if !newer_wire_version
                && record.get("type").and_then(Value::as_str) == Some("metadata")
            {
                record.insert(
                    "protocol_version".to_string(),
                    WIRE_PROTOCOL_VERSION.into(),
                );
                record.insert(
                    "min_protocol_version".to_string(),
                    WIRE_MIN_READER_VERSION.into(),
                );
            }
            let record_type = record.get("type").and_then(Value::as_str).map(str::to_string);
            let has_path = record.contains_key("path");
            let has_key = record.contains_key("key");
            let normalized = if newer_wire_version {
                Some(record)
            } else {
                self.normalize_plan_revision_record(record, record_index, true)
            };
            if !newer_wire_version
                && record_type.as_deref() == Some("plan.revision")
                && normalized.is_some()
                && has_path
                && !has_key
            {
                legacy_plan_revision_migrated = true;
            }
            let Some(normalized) = normalized else {
                if record_type.as_deref() == Some("plan.revision") {
                    record_index += 1;
                }
                continue;
            };
            if let Some(records) = rewritten_records.as_mut() {
                records.push(normalized.clone());
            }
            if is_human_record_type(
                normalized.get("type").and_then(Value::as_str).unwrap(),
            ) {
                mode_two_entries.push(WireLine {
                    record: normalized.clone(),
                    line: line_count,
                });
            }
            if sender
                .send(Ok(WireLine {
                    record: normalized.clone(),
                    line: line_count,
                }))
                .await
                .is_err()
            {
                return Ok(());
            }
            if normalized.get("type").and_then(Value::as_str) != Some("metadata") {
                record_index += 1;
            }
        }
        drop(source);

        if legacy_plan_revision_migrated && rewritten_records.is_none() {
            rewritten_records = Some(
                self.rebuild_rewrite_records(&migrations, newer_wire_version)
                    .await?,
            );
        }
        if !has_records {
            rewritten_records = Some(vec![create_wire_metadata_record(None)]);
        }
        let truncation = truncation.lock().take();
        if let Some(truncation) = truncation {
            self.inner.state.lock().pending_rewrite = None;
            self.repair_journal(
                truncation,
                rewritten_records.map(|records| {
                    records.into_iter().map(Value::Object).collect::<Vec<_>>()
                }),
            )
            .await?;
        } else if let Some(records) = rewritten_records {
            if !self.rewrite_migrated_journal(records.clone()).await? {
                self.inner.state.lock().pending_rewrite = Some(records);
            }
        }
        let appended = {
            let state = self.inner.state.lock();
            state
                .mode_two_entries
                .get(mode_two_length_at_start..)
                .unwrap_or(&[])
                .to_vec()
        };
        self.merge_mode_two_entries(mode_two_entries, appended, line_count);
        self.inner.state.lock().last_read_line_count = line_count;
        Ok(())
    }

    fn merge_mode_two_entries(
        &self,
        fresh: Vec<WireLine>,
        appended: Vec<WireLine>,
        line_count: u64,
    ) {
        let mut line = line_count;
        let mut merged = fresh;
        for entry in appended {
            if merged
                .iter()
                .any(|candidate| records_match(&candidate.record, &entry.record))
            {
                continue;
            }
            line += 1;
            merged.push(WireLine {
                record: entry.record,
                line,
            });
        }
        self.inner.state.lock().mode_two_entries = merged;
    }

    pub fn line_count(&self) -> u64 {
        self.inner.state.lock().lines
    }

    pub fn last_context_clear_line(&self) -> Option<u64> {
        self.inner.state.lock().last_clear_line
    }

    pub fn journal_path(&self) -> Option<String> {
        self.inner
            .storage
            .path_for(&self.inner.wire_scope, AGENT_WIRE_RECORD_KEY)
    }

    pub fn externally_modified(&self) -> bool {
        self.inner.log.externally_modified(
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
        )
    }

    async fn rewrite_migrated_journal(
        &self,
        records: Vec<WireRecord>,
    ) -> Result<bool, WireAsyncError> {
        if !self
            .inner
            .session_locks
            .try_hold(&self.inner.session_context.session_id())?
            || self.externally_modified()
        {
            return Ok(false);
        }
        let values = records
            .iter()
            .cloned()
            .map(Value::Object)
            .collect::<Vec<_>>();
        self.inner
            .log
            .rewrite(
                &self.inner.wire_scope,
                AGENT_WIRE_RECORD_KEY,
                &values,
                None,
            )
            .await?;
        let mut state = self.inner.state.lock();
        state.pending_rewrite = None;
        state.write_generation += 1;
        state.lines = records.len() as u64;
        state.last_clear_line = last_context_clear_line_of(&values);
        Ok(true)
    }

    async fn rewrite_pending_journal(&self) -> Result<(), WireAsyncError> {
        let pending = self.inner.state.lock().pending_rewrite.clone();
        let Some(pending) = pending else {
            return Ok(());
        };
        if self.rewrite_migrated_journal(pending).await? {
            return Ok(());
        }
        let mut details = Map::new();
        details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
        details.insert("key".to_string(), AGENT_WIRE_RECORD_KEY.into());
        let error = WireError::with_options(
            RECORDS_WRITE_FAILED,
            "Wire journal migration did not complete; record was not appended",
            options(details),
        );
        self.inner.state.lock().persist_error = Some(Box::new(error));
        let mut details = Map::new();
        details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
        details.insert("key".to_string(), AGENT_WIRE_RECORD_KEY.into());
        Err(Box::new(WireError::with_options(
            RECORDS_WRITE_FAILED,
            "Wire journal migration did not complete; record was not appended",
            options(details),
        )))
    }

    async fn repair_journal(
        &self,
        truncation: AppendLogTruncation,
        rewritten_records: Option<Vec<Value>>,
    ) -> Result<(), WireAsyncError> {
        let records = match rewritten_records {
            Some(records) => records,
            None => {
                let source = self.inner.log.read(
                    &self.inner.wire_scope,
                    AGENT_WIRE_RECORD_KEY,
                    Some(tolerate_truncation()),
                );
                pin_mut!(source);
                let mut records = Vec::new();
                while let Some(record) = source.next().await {
                    records.push(record?);
                }
                records
            }
        };
        if !self
            .inner
            .session_locks
            .try_hold(&self.inner.session_context.session_id())?
            || self.externally_modified()
        {
            self.inner.state.lock().pending_repair = Some(PendingRepair {
                records,
                truncation,
            });
            return Ok(());
        }
        let outcome = repair_wire_journal(
            &WireJournalRepairServices {
                append_log: self.inner.log.clone(),
                storage: self.inner.storage.clone(),
                log: self.inner.logger.clone(),
                telemetry: self.inner.telemetry.clone(),
            },
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
            &records,
            &truncation,
        )
        .await;
        let mut state = self.inner.state.lock();
        state.pending_repair = if outcome == "failed" {
            Some(PendingRepair {
                records: records.clone(),
                truncation,
            })
        } else {
            None
        };
        if outcome != "failed" {
            state.write_generation += 1;
            state.lines = records.len() as u64;
            state.last_clear_line = last_context_clear_line_of(&records);
        }
        Ok(())
    }

    async fn repair_pending_journal(&self) -> Result<(), WireAsyncError> {
        let pending = self.inner.state.lock().pending_repair.as_ref().map(|pending| {
            (pending.records.clone(), pending.truncation.clone())
        });
        let Some((records, truncation)) = pending else {
            return Ok(());
        };
        let line_number = truncation.line_number;
        self.repair_journal(truncation, Some(records)).await?;
        if self.inner.state.lock().pending_repair.is_some() {
            let mut details = Map::new();
            details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
            details.insert("key".to_string(), AGENT_WIRE_RECORD_KEY.into());
            details.insert("lineNumber".to_string(), line_number.into());
            let error = WireError::with_options(
                RECORDS_WRITE_FAILED,
                "Wire journal repair did not complete; record was not appended",
                options(details),
            );
            self.inner.state.lock().persist_error = Some(Box::new(error));
            let mut details = Map::new();
            details.insert("scope".to_string(), self.inner.wire_scope.clone().into());
            details.insert("key".to_string(), AGENT_WIRE_RECORD_KEY.into());
            details.insert("lineNumber".to_string(), line_number.into());
            return Err(Box::new(WireError::with_options(
                RECORDS_WRITE_FAILED,
                "Wire journal repair did not complete; record was not appended",
                options(details),
            )));
        }
        Ok(())
    }

    pub async fn drain_persisted(&self) -> Result<(), WireAsyncError> {
        let queue = self.inner.state.lock().persist_queue.clone();
        if let Some(queue) = queue {
            queue.await;
        }
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), WireAsyncError> {
        self.drain_persisted().await?;
        let persist_error = self.inner.state.lock().persist_error.take();
        if let Some(error) = persist_error {
            return Err(error);
        }
        self.inner
            .log
            .flush_log(&self.inner.wire_scope, AGENT_WIRE_RECORD_KEY)
            .await
    }

    async fn rebuild_rewrite_records(
        &self,
        migrations: &[&dyn WireMigration],
        newer_wire_version: bool,
    ) -> Result<Vec<WireRecord>, WireAsyncError> {
        let source = self.inner.log.read(
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
            Some(tolerate_truncation()),
        );
        pin_mut!(source);
        let mut records = Vec::new();
        while let Some(candidate) = source.next().await {
            let candidate = candidate?;
            if !is_wire_record(&candidate) {
                continue;
            }
            let mut record = migrate_wire_record(
                candidate.as_object().cloned().unwrap(),
                migrations,
            );
            if !newer_wire_version
                && record.get("type").and_then(Value::as_str) == Some("metadata")
            {
                record.insert(
                    "protocol_version".to_string(),
                    WIRE_PROTOCOL_VERSION.into(),
                );
            }
            let normalized = if newer_wire_version {
                Some(record)
            } else {
                self.normalize_plan_revision_record(record, 0, false)
            };
            if let Some(normalized) = normalized {
                records.push(normalized);
            }
        }
        Ok(records)
    }

    fn normalize_plan_revision_record(
        &self,
        mut record: WireRecord,
        index: u64,
        report: bool,
    ) -> Option<WireRecord> {
        if record.get("type").and_then(Value::as_str) != Some("plan.revision")
            || record.contains_key("key")
        {
            return Some(record);
        }
        let path = record.get("path").and_then(Value::as_str);
        let Some(path) = path else {
            if report {
                let mut properties = Map::new();
                properties.insert("record_type".to_string(), "plan.revision".into());
                properties.insert("legacy_field".to_string(), "path".into());
                properties.insert("migration_outcome".to_string(), "skipped".into());
                self.inner
                    .telemetry
                    .track2("wire_plan_revision_migrated", Some(properties));
                self.report_skipped_record(Some("plan.revision"), index, true);
            }
            return None;
        };
        let key = extract_legacy_plan_revision_key(path, &self.inner.agent_id);
        if report {
            let mut properties = Map::new();
            properties.insert("record_type".to_string(), "plan.revision".into());
            properties.insert("legacy_field".to_string(), "path".into());
            properties.insert(
                "migration_outcome".to_string(),
                if key.is_some() { "migrated" } else { "skipped" }.into(),
            );
            self.inner
                .telemetry
                .track2("wire_plan_revision_migrated", Some(properties));
        }
        let Some(key) = key else {
            if report {
                self.report_skipped_record(Some("plan.revision"), index, true);
            }
            return None;
        };
        record.remove("path");
        record.insert("key".to_string(), key.into());
        Some(record)
    }

    fn report_skipped_record(&self, record_type: Option<&str>, index: u64, malformed: bool) {
        let message = match record_type {
            None => "Malformed wire record skipped during restore".to_string(),
            Some(record_type) if malformed => format!(
                "Malformed wire record type '{record_type}' skipped during restore"
            ),
            Some(record_type) => {
                format!("Unknown wire record type '{record_type}' skipped during restore")
            }
        };
        let mut details = Map::new();
        if let Some(record_type) = record_type {
            details.insert("type".to_string(), record_type.into());
        }
        details.insert("index".to_string(), index.into());
        on_unexpected_error(Box::new(WireError::with_options(
            WIRE_UNKNOWN_RECORD,
            message,
            options(details),
        )));
    }

    fn append_record_low(&self, record: WireRecord) -> Result<(), WireAsyncError> {
        self.inner.log.append(
            &self.inner.wire_scope,
            AGENT_WIRE_RECORD_KEY,
            Value::Object(record.clone()),
            Some(AppendLogOptions {
                on_error: Some(Arc::new(on_unexpected_error)),
            }),
        )?;
        let listeners = {
            let mut state = self.inner.state.lock();
            state.lines += 1;
            let line = state.lines;
            if is_human_record_type(record.get("type").and_then(Value::as_str).unwrap()) {
                state.mode_two_entries.push(WireLine {
                    record: record.clone(),
                    line,
                });
            }
            if record.get("type").and_then(Value::as_str) == Some("context.clear") {
                state.last_clear_line = Some(line);
            }
            state.append_listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                listener(&record);
            }));
            if let Err(error) = result {
                on_unexpected_error(format!("{error:?}").into());
            }
        }
        Ok(())
    }
}

#[async_trait]
impl IAgentJournal for WireService {
    fn journal_ref(&self) -> AgentJournalRef {
        WireService::journal_ref(self)
    }

    fn append(&self, record: WireRecord, dehydrate: Option<RecordDehydrator>) {
        WireService::append(self, record, dehydrate);
    }

    fn read(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>> {
        WireService::read(self)
    }

    fn read_raw(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>> {
        WireService::read_raw(self)
    }

    async fn switch_branch(
        &self,
        input: SwitchBranchInput,
    ) -> Result<SwitchedBranch, WireAsyncError> {
        WireService::switch_branch(self, input).await
    }

    fn branches(&self) -> Vec<String> {
        WireService::branches(self)
    }

    fn next_seq(&self) -> u64 {
        WireService::next_seq(self)
    }

    async fn settled(&self) -> Result<(), WireAsyncError> {
        WireService::settled(self).await
    }
}

#[async_trait]
impl IWireService for WireService {
    async fn seal(&self) -> Result<(), WireAsyncError> {
        WireService::seal(self).await
    }

    fn append_record(&self, record: WireRecord, dehydrate: Option<RecordDehydrator>) {
        WireService::append_record(self, record, dehydrate);
    }

    fn on_did_append_record(&self, listener: AppendListener) -> Box<dyn IDisposable> {
        WireService::on_did_append_record(self, listener)
    }

    fn read_journal(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>> {
        WireService::read_journal(self)
    }

    fn read_restorable(
        &self,
    ) -> Pin<Box<dyn Stream<Item = Result<WireRecord, WireAsyncError>> + Send + '_>> {
        WireService::read_restorable(self)
    }

    async fn read_restore_chains(&self) -> Result<WireRestoreChains, WireAsyncError> {
        WireService::read_restore_chains(self).await
    }

    fn read_human_chain(&self) -> Result<Vec<WireLine>, Box<dyn Error + Send + Sync>> {
        WireService::read_human_chain(self)
    }

    async fn flush(&self) -> Result<(), WireAsyncError> {
        WireService::flush(self).await
    }

    async fn drain_persisted(&self) -> Result<(), WireAsyncError> {
        WireService::drain_persisted(self).await
    }

    fn line_count(&self) -> u64 {
        WireService::line_count(self)
    }

    fn last_context_clear_line(&self) -> Option<u64> {
        WireService::last_context_clear_line(self)
    }

    fn journal_path(&self) -> Option<String> {
        WireService::journal_path(self)
    }

    fn externally_modified(&self) -> bool {
        WireService::externally_modified(self)
    }
}

fn options(details: Map<String, Value>) -> kimi_core::primitives::errors::Error2Options {
    kimi_core::primitives::errors::Error2Options {
        details: Some(details),
        ..Default::default()
    }
}

fn tolerate_truncation() -> AppendLogReadOptions {
    AppendLogReadOptions {
        on_truncate: Some(Arc::new(|_| {})),
    }
}

fn values_match(a: &Value, b: &Value) -> bool {
    serde_json::to_string(a).unwrap() == serde_json::to_string(b).unwrap()
}

fn records_match(a: &WireRecord, b: &WireRecord) -> bool {
    values_match(&Value::Object(a.clone()), &Value::Object(b.clone()))
}

fn last_context_clear_line_of(records: &[Value]) -> Option<u64> {
    records.iter().rposition(|record| {
        record.get("type").and_then(Value::as_str) == Some("context.clear")
    }).map(|index| index as u64 + 1)
}

fn extract_legacy_plan_revision_key(path: &str, agent_id: &str) -> Option<String> {
    if path.contains('\\') {
        return None;
    }
    let segments = path.split('/').collect::<Vec<_>>();
    if segments.len() < 8
        || segments[0] != "sessions"
        || segments[3] != "agents"
        || segments[4] != agent_id
        || segments[1..3]
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return None;
    }
    let key = segments[5..].join("/");
    let mut components = key.split('/');
    let valid = matches!(components.next(), Some("plan"))
        && components.next().is_some_and(|segment| {
            !segment.is_empty() && !segment.contains('/')
        })
        && components.next().is_some_and(|segment| {
            segment
                .strip_prefix('v')
                .and_then(|segment| segment.strip_suffix(".md"))
                .is_some_and(|version| {
                    !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
        && components.next().is_none();
    valid.then_some(key)
}
