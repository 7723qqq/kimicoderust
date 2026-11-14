use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream::BoxStream};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::human::eventStore::events::{
    ExternalEvent, InternalEvent, event_schema_for, parse_event, validate_event,
};
use crate::human::eventStore::journal::{
    JournalRecord, JournalRef, StoreJournal, SyncStoreJournal,
};
use crate::human::eventStore::slice::{Effect, Enqueue, FoldContext, Slice};
use crate::human::store::types::{
    AppendInput, BranchRef, EntryLine, StoreError,
};

pub type SliceMap = HashMap<String, Slice<Value>>;
pub type CombinedState = Map<String, Value>;

#[derive(Clone)]
pub enum Cause {
    Event {
        event: ExternalEvent,
        entry: EntryLine,
    },
    Internal {
        event: InternalEvent,
    },
    Reset {
        state: CombinedState,
    },
    SliceJoined {
        name: String,
    },
}

pub type ErrorReporter = Arc<dyn Fn(Value) + Send + Sync>;
pub type Listener = Arc<dyn Fn(CombinedState, &Cause) + Send + Sync>;

pub struct EventStoreOptions {
    pub journal: Arc<dyn StoreJournal>,
    pub slices: SliceMap,
    pub drain_limit: Option<usize>,
    pub on_error: Option<ErrorReporter>,
    pub mutable_replay: Option<bool>,
}

pub struct SyncEventStoreOptions {
    pub journal: Arc<dyn SyncStoreJournal>,
    pub slices: SliceMap,
    pub drain_limit: Option<usize>,
    pub on_error: Option<ErrorReporter>,
    pub mutable_replay: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventStorePhase {
    Open,
    Closed,
}

const EVENT_ENTRY_KIND: &str = "event";
const DEFAULT_DRAIN_LIMIT: usize = 100;

struct SyncJournalAdapter {
    inner: Arc<dyn SyncStoreJournal>,
}

#[async_trait]
impl StoreJournal for SyncJournalAdapter {
    fn ref_(&self) -> JournalRef {
        self.inner.ref_()
    }

    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError> {
        self.inner.append(input).await
    }

    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>> {
        self.inner.read()
    }

    fn next_seq(&self) -> usize {
        self.inner.next_seq()
    }

    async fn settled(&self) -> Result<(), StoreError> {
        self.inner.settled().await
    }
}

pub struct EventStoreImpl {
    journal: Mutex<Arc<dyn StoreJournal>>,
    slices: Mutex<SliceMap>,
    state: Mutex<CombinedState>,
    phase_value: Mutex<EventStorePhase>,
    operation: tokio::sync::Mutex<()>,
    drain_limit: usize,
    report: ErrorReporter,
    mutable_replay: bool,
    listeners: Mutex<IndexMap<u64, Listener>>,
    next_listener_id: AtomicU64,
}

pub async fn create_event_store(opts: EventStoreOptions) -> Result<Arc<EventStoreImpl>, StoreError> {
    let store = Arc::new(EventStoreImpl::new(opts));
    store.refold(store.journal.lock().clone()).await?;
    Ok(store)
}

pub fn create_event_store_sync(opts: SyncEventStoreOptions) -> Result<Arc<EventStoreImpl>, StoreError> {
    let sync_journal = opts.journal.clone();
    let journal: Arc<dyn StoreJournal> = Arc::new(SyncJournalAdapter {
        inner: opts.journal,
    });
    let store = Arc::new(EventStoreImpl::new(EventStoreOptions {
        journal,
        slices: opts.slices,
        drain_limit: opts.drain_limit,
        on_error: opts.on_error,
        mutable_replay: opts.mutable_replay,
    }));
    store.refold_sync(sync_journal)?;
    Ok(store)
}

impl EventStoreImpl {
    fn new(opts: EventStoreOptions) -> Self {
        Self {
            journal: Mutex::new(opts.journal),
            slices: Mutex::new(opts.slices),
            state: Mutex::new(Map::new()),
            phase_value: Mutex::new(EventStorePhase::Open),
            operation: tokio::sync::Mutex::new(()),
            drain_limit: opts.drain_limit.unwrap_or(DEFAULT_DRAIN_LIMIT),
            report: opts.on_error.unwrap_or_else(|| {
                Arc::new(|error| eprintln!("{}", Value::String(error.to_string())))
            }),
            mutable_replay: opts.mutable_replay == Some(true),
            listeners: Mutex::new(IndexMap::new()),
            next_listener_id: AtomicU64::new(0),
        }
    }

    pub fn ref_(&self) -> JournalRef {
        self.journal.lock().ref_()
    }

    pub fn phase(&self) -> EventStorePhase {
        *self.phase_value.lock()
    }

    pub fn get_state(&self) -> CombinedState {
        self.state.lock().clone()
    }

    pub fn slice(&self, name: &str) -> Option<Value> {
        self.state.lock().get(name).cloned()
    }

    pub fn select<T>(&self, selector: impl FnOnce(&CombinedState) -> T) -> T {
        selector(&self.get_state())
    }

    pub fn subscribe(self: &Arc<Self>, listener: Listener) -> Arc<dyn Fn() + Send + Sync> {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        let weak = Arc::downgrade(self);
        Arc::new(move || {
            if let Some(store) = Weak::upgrade(&weak) {
                store.listeners.lock().remove(&id);
            }
        })
    }

    fn ensure_open(&self) -> Result<(), StoreError> {
        if self.phase() != EventStorePhase::Open {
            return Err(StoreError::new("closed", "store is closed"));
        }
        Ok(())
    }

    pub async fn dispatch(
        self: &Arc<Self>,
        events: impl Into<Vec<ExternalEvent>>,
    ) -> Result<EntryLine, StoreError> {
        self.ensure_open()?;
        let events = events.into();
        for event in &events {
            if let Some(error) = validate_event(event) {
                return Err(error);
            }
        }
        let _operation = self.operation.lock().await;
        self.fold_and_append(events).await
    }

    pub async fn register_slice(
        self: &Arc<Self>,
        slice: Slice<Value>,
    ) -> Result<Arc<dyn Fn() + Send + Sync>, StoreError> {
        self.ensure_open()?;
        if self.slices.lock().contains_key(&slice.name) {
            return Err(StoreError::new(
                "duplicate-slice",
                format!("slice '{}' is already registered", slice.name),
            ));
        }
        let name = slice.name.clone();
        {
            let _operation = self.operation.lock().await;
            let previous_slices = self.slices.lock().clone();
            self.slices.lock().insert(name.clone(), slice);
            let journal = self.journal.lock().clone();
            if let Err(error) = self.refold(journal).await {
                *self.slices.lock() = previous_slices;
                return Err(error);
            }
        }
        self.notify(vec![Cause::SliceJoined { name: name.clone() }]);
        let weak = Arc::downgrade(self);
        Ok(Arc::new(move || {
            if let Some(store) = Weak::upgrade(&weak) {
                store.slices.lock().remove(&name);
                store.state.lock().remove(&name);
            }
        }))
    }

    pub async fn reset(&self, journal: Arc<dyn StoreJournal>) -> Result<(), StoreError> {
        self.ensure_open()?;
        {
            let _operation = self.operation.lock().await;
            let current = self.journal.lock().clone();
            current.settled().await?;
            self.refold(journal.clone()).await?;
            *self.journal.lock() = journal;
        }
        self.notify(vec![Cause::Reset {
            state: self.get_state(),
        }]);
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), StoreError> {
        let _operation = self.operation.lock().await;
        self.journal.lock().clone().settled().await
    }

    pub async fn close(&self) -> Result<(), StoreError> {
        self.flush().await?;
        *self.phase_value.lock() = EventStorePhase::Closed;
        self.listeners.lock().clear();
        Ok(())
    }

    async fn refold(&self, journal: Arc<dyn StoreJournal>) -> Result<(), StoreError> {
        let records = journal.read().try_collect::<Vec<_>>().await?;
        self.fold_records(records)
    }

    fn refold_sync(&self, journal: Arc<dyn SyncStoreJournal>) -> Result<(), StoreError> {
        self.fold_records(journal.read_sync())
    }

    fn fold_records(&self, records: Vec<JournalRecord>) -> Result<(), StoreError> {
        let seeded = self
            .slices
            .lock()
            .iter()
            .map(|(name, slice)| (name.clone(), (slice.initial_state)()))
            .collect::<Map<_, _>>();
        let previous = self.get_state();
        *self.state.lock() = seeded;
        let result = (|| {
            for record in records {
                if record.kind != EVENT_ENTRY_KIND {
                    continue;
                }
                self.replay_record(record)?;
            }
            Ok(())
        })();
        if result.is_err() {
            *self.state.lock() = previous;
        }
        result
    }

    fn replay_record(&self, record: JournalRecord) -> Result<(), StoreError> {
        if event_schema_for(&record.type_).is_none() {
            return Ok(());
        }
        let Some(event) = parse_event(&record.type_, &record.data) else {
            (self.report)(json!({
                "code": "schema",
                "message": format!("event '{}' at seq {} failed schema validation", record.type_, record.seq),
            }));
            return Ok(());
        };
        let ref_ = BranchRef {
            branch: record.branch,
            seq: record.seq,
        };
        let applied = self.apply_event(&event, ref_.clone(), record.ts, true)?;
        self.drain(applied.raised, ref_, record.ts, true)?;
        Ok(())
    }

    async fn fold_and_append(&self, events: Vec<ExternalEvent>) -> Result<EntryLine, StoreError> {
        let mut causes = Vec::new();
        let mut entries = Vec::new();
        for event in events {
            let journal = self.journal.lock().clone();
            let ts = event
                .get("time")
                .and_then(Value::as_u64)
                .or_else(|| event.get("time").and_then(Value::as_f64).map(|ts| ts as u64))
                .unwrap_or_else(now);
            let ref_ = BranchRef {
                branch: journal.ref_().branch,
                seq: journal.next_seq() as u64,
            };
            let applied = self.apply_event(&event, ref_.clone(), ts, false)?;
            let internal_causes = self.drain(applied.raised, ref_, ts, false)?;
            self.run_effects(applied.effects);
            let entry = journal
                .append(AppendInput {
                    type_: event
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    kind: EVENT_ENTRY_KIND.to_string(),
                    data: Some(Value::Object(event.clone())),
                })
                .await?;
            entries.push(entry.clone());
            causes.push(Cause::Event { event, entry });
            causes.extend(internal_causes);
        }
        self.notify(causes);
        Ok(entries.last().unwrap().clone())
    }

    fn apply_event(
        &self,
        event: &InternalEvent,
        ref_: BranchRef,
        ts: u64,
        replaying: bool,
    ) -> Result<AppliedEvent, StoreError> {
        let raised = Arc::new(Mutex::new(Vec::new()));
        let effects = Arc::new(Mutex::new(Vec::<Effect>::new()));
        let enqueue = {
            let raised = raised.clone();
            let effects = effects.clone();
            Enqueue::new(
                move |event| raised.lock().push(event),
                move |effect| effects.lock().push(effect),
            )
        };
        let mut ctx = FoldContext {
            ref_,
            ts,
            replaying,
            enqueue,
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut changed = false;
        let mut next = self.get_state();
        for (name, slice) in self.slices.lock().clone() {
            let Some(reducer) = slice.reducers.get(event_type) else {
                continue;
            };
            changed = true;
            let current = next.get(&name).cloned().unwrap_or(Value::Null);
            let produced = if replaying && self.mutable_replay {
                let mut current = current;
                let produced = reducer(&mut current, event, &mut ctx);
                produced.unwrap_or(current)
            } else {
                let mut draft = current;
                let produced = reducer(&mut draft, event, &mut ctx);
                produced.unwrap_or(draft)
            };
            next.insert(name, produced);
        }
        if changed {
            *self.state.lock() = next;
        }
        Ok(AppliedEvent {
            raised: std::mem::take(&mut *raised.lock()),
            effects: std::mem::take(&mut *effects.lock()),
        })
    }

    fn drain(
        &self,
        initial: Vec<InternalEvent>,
        ref_: BranchRef,
        ts: u64,
        replaying: bool,
    ) -> Result<Vec<Cause>, StoreError> {
        let mut causes = Vec::new();
        let mut queue = initial.into_iter().collect::<VecDeque<_>>();
        let mut count = 0;
        while let Some(internal) = queue.pop_front() {
            count += 1;
            if count > self.drain_limit {
                return Err(StoreError::new(
                    "drain-limit",
                    format!("internal event drain exceeded limit {}", self.drain_limit),
                ));
            }
            let applied = self.apply_event(&internal, ref_.clone(), ts, replaying)?;
            queue.extend(applied.raised);
            if !replaying {
                self.run_effects(applied.effects);
                causes.push(Cause::Internal { event: internal });
            }
        }
        Ok(causes)
    }

    fn run_effects(&self, effects: Vec<Effect>) {
        for effect in effects {
            if let Err(error) = catch_unwind(AssertUnwindSafe(effect)) {
                let message = error
                    .downcast_ref::<&str>()
                    .map(|message| (*message).to_string())
                    .or_else(|| error.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "effect panicked".to_string());
                (self.report)(Value::String(message));
            }
        }
    }

    fn notify(&self, causes: Vec<Cause>) {
        let state = self.get_state();
        for cause in causes {
            let listeners = self.listeners.lock().values().cloned().collect::<Vec<_>>();
            for listener in &listeners {
                listener(state.clone(), &cause);
            }
        }
    }
}

struct AppliedEvent {
    raised: Vec<InternalEvent>,
    effects: Vec<Effect>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
