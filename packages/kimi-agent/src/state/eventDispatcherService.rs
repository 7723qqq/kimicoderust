use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, StreamExt};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::collection::CollectionView;
use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::log::log::ILogService;
use crate::agent::agent_context::agent_space::AgentSpaceImpl;
use crate::agent::blob::agent_blob_service::IAgentBlobService;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state::IAgentStateService;
use crate::app::event::event2::{Event2, Event2Class, event2_from_record};
use crate::app::event::event_bus::IEventBus;
use crate::hooks::OrderedHookSlot;
use crate::wire::errors::{WIRE_UNKNOWN_RECORD, WireError};
use crate::wire::human::is_human_record_type;
use crate::wire::record::{PartsTransformer, WireAsyncError, WireRecord};
use crate::wire::tree::tree::AGENT_SWITCHED_TYPE;
use crate::wire::wire::IWireService;

use super::agent_model::{
    AgentModel, AgentModelDefinition, agent_model_definitions,
};
use super::errors::{STATE_CYCLE, StateError};
use super::event_dispatcher::{
    DurableAgentRuntimeParticipant, DurableRuntimeParticipantHost, EventDispatcherHooks,
    IEventDispatcher, RestorePhase,
};
use super::state::{
    ReplayableStateKey, expanded_model_appliers, expanded_runtime_folds,
};
use super::state_contribution::{
    EventStateContributionRecord, FoldedEventStateRegistry, fold_event_state_contributions,
};

const MAX_DRAIN: usize = 1_000;
const MAX_DRAIN_PER_TYPE: usize = 100;
const DISPATCH_STUCK_WARN_MS: i64 = 30_000;

#[derive(Debug, Clone)]
struct SharedDispatchError(Arc<String>);

impl SharedDispatchError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl std::fmt::Display for SharedDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl std::error::Error for SharedDispatchError {}

#[derive(Clone)]
pub struct CycleError {
    pub depth: usize,
    pub event_types: Vec<String>,
    pub repeated_type: Option<String>,
}

impl CycleError {
    pub fn new(depth: usize, event_types: Vec<String>, repeated_type: Option<String>) -> StateError {
        let mut details = serde_json::Map::new();
        details.insert("depth".to_string(), depth.into());
        details.insert(
            "eventTypes".to_string(),
            event_types.iter().take(20).cloned().collect::<Vec<_>>().into(),
        );
        if let Some(repeated_type) = &repeated_type {
            details.insert("repeatedType".to_string(), repeated_type.clone().into());
        }
        StateError::with_options(
            STATE_CYCLE,
            format!(
                "Event dispatch cascade exceeded MAX_DRAIN (depth {depth}, per-type {MAX_DRAIN_PER_TYPE}); possible event cycle{}{}",
                repeated_type.as_ref().map(|repeated| format!(" [repeated: {repeated}]")).unwrap_or_default(),
                if event_types.is_empty() {
                    String::new()
                } else {
                    format!(" [queued: {}]", event_types.iter().take(20).cloned().collect::<Vec<_>>().join(", "))
                }
            ),
            kimi_core::primitives::errors::Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )
    }
}

struct StateMeta {
    checkpoints: Vec<Value>,
}

#[derive(Clone)]
struct QueuedEvent {
    event: Event2,
    resolve: Arc<dyn Fn() + Send + Sync>,
    reject: Arc<dyn Fn(WireAsyncError) + Send + Sync>,
}

struct PreparedFold {
    key: Arc<ReplayableStateKey>,
    meta: Arc<Mutex<StateMeta>>,
    ctx: FoldContextImpl,
    next: Value,
}

type ParticipantApplier = Arc<dyn Fn(Value, &Event2, &mut FoldContextImpl) -> Option<Value> + Send + Sync>;

#[derive(Clone)]
struct ParticipantAttachment {
    id: String,
    appliers: HashMap<String, ParticipantApplier>,
    meta: Arc<Mutex<StateMeta>>,
    undoable: bool,
    initial: Value,
    get_state: Arc<dyn Fn() -> Value + Send + Sync>,
    commit: Arc<dyn Fn(Value, bool) + Send + Sync>,
}

struct PreparedParticipant {
    attachment: ParticipantAttachment,
    ctx: FoldContextImpl,
    next: Value,
}

struct ResolvedRecord {
    cls: Option<Arc<Event2Class>>,
    event: Option<Event2>,
}

#[derive(Clone)]
struct FoldContextImpl {
    owner: std::sync::Weak<EventDispatcherServiceInner>,
    silent: bool,
    pending_checkpoint: bool,
    pending_clear: bool,
    pending_undo: Option<u64>,
}

impl FoldContextImpl {
    fn sanitize_pending_undo(&mut self, meta: &StateMeta) {
        if self.pending_undo.is_some_and(|count| {
            count == 0 || count as usize > meta.checkpoints.len()
        }) {
            self.pending_undo = None;
        }
    }
}

impl super::state::FoldContext for FoldContextImpl {
    fn silent(&self) -> bool {
        self.silent
    }

    fn checkpoint(&mut self) {
        if !self.silent {
            self.pending_checkpoint = true;
        }
    }

    fn clear_checkpoints(&mut self) {
        if !self.silent {
            self.pending_clear = true;
        }
    }

    fn undo_to_checkpoint(&mut self, count: u64) {
        if !self.silent {
            self.pending_undo = Some(count);
        }
    }

    fn emit(&mut self, event: Event2) {
        if self.silent {
            return;
        }
        if let Some(owner) = self.owner.upgrade() {
            owner.enqueue(event);
        }
    }
}

struct LateAttachment {
    participant: Arc<dyn DurableAgentRuntimeParticipant>,
    resolve: Arc<dyn Fn(Box<dyn IDisposable>) + Send + Sync>,
    reject: Arc<dyn Fn(WireAsyncError) + Send + Sync>,
}

struct DispatcherState {
    metas: HashMap<usize, Arc<Mutex<StateMeta>>>,
    replay_fresh_keys: HashSet<usize>,
    replay_fresh_attachments: HashSet<usize>,
    replay_mutable_states: HashMap<usize, Value>,
    frozen_states: HashSet<usize>,
    folded: FoldedEventStateRegistry,
    active_model_defs: IndexMap<String, Arc<AgentModelDefinition>>,
    withdrawn_model_ids: HashSet<String>,
    model_targets: HashMap<String, Vec<Arc<AgentModelDefinition>>>,
    model_attachments: HashMap<usize, ParticipantAttachment>,
    participant_targets: HashMap<String, Vec<ParticipantAttachment>>,
    participant_attachments: HashMap<String, ParticipantAttachment>,
    restore_phase: RestorePhase,
    dispatching: bool,
    late_attachment_tail: Option<Shared<BoxFuture<'static, ()>>>,
    late_attachment_serial: u64,
    dispatching_since: Option<i64>,
    queue_stuck_warned: bool,
    disposed: bool,
    queue: Vec<QueuedEvent>,
    drain_depth: usize,
    drain_counts: HashMap<String, usize>,
    did_run_restore_hooks: bool,
    late_attachments: Vec<LateAttachment>,
}

struct EventDispatcherServiceInner {
    wire: Arc<dyn IWireService>,
    event_bus: Arc<dyn IEventBus>,
    agent_scope: Option<Arc<dyn IAgentScopeContext>>,
    blob_service: Arc<dyn IAgentBlobService>,
    agent_state: Arc<dyn IAgentStateService>,
    logger: Arc<dyn ILogService>,
    space_host: Mutex<AgentSpaceHost>,
    hooks: EventDispatcherHooks,
    state: Arc<Mutex<DispatcherState>>,
}

#[derive(Clone)]
pub struct EventDispatcherService {
    inner: Arc<EventDispatcherServiceInner>,
    _registrations: Arc<Mutex<Vec<Box<dyn IDisposable>>>>,
}

#[derive(Clone)]
struct AgentSpaceHost {
    is_active_model_definition: Arc<dyn Fn(&Arc<AgentModelDefinition>) -> bool + Send + Sync>,
    register_model: Arc<dyn Fn(&Arc<AgentModelDefinition>, &AgentModel) + Send + Sync>,
    dispatch_model_event: Arc<dyn Fn(Event2) -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>,
    read_legacy_state:
        Arc<dyn Fn(&Arc<crate::state::state::ReplayableStateKey>) -> Value + Send + Sync>,
}

impl EventDispatcherService {
    pub fn new(
        wire: Arc<dyn IWireService>,
        event_bus: Arc<dyn IEventBus>,
        agent_scope: Option<Arc<dyn IAgentScopeContext>>,
        blob_service: Arc<dyn IAgentBlobService>,
        agent_state: Arc<dyn IAgentStateService>,
        logger: Arc<dyn ILogService>,
        contribution_view: CollectionView<EventStateContributionRecord>,
        model_view: CollectionView<Arc<AgentModelDefinition>>,
    ) -> Self {
        let folded = fold_event_state_contributions(&contribution_view.items, &agent_state.replayable_keys());
        let active_model_defs = model_view.items.iter().map(|definition| (definition.id.clone(), definition.clone())).collect();
        let this = Self {
            inner: Arc::new(EventDispatcherServiceInner {
                wire,
                event_bus,
                agent_scope,
                blob_service,
                agent_state,
                logger,
                space_host: Mutex::new(AgentSpaceHost {
                    is_active_model_definition: Arc::new(|_| true),
                    register_model: Arc::new(|_, _| {}),
                    dispatch_model_event: Arc::new(|event| async move { Ok(()) }.boxed()),
                    read_legacy_state: Arc::new(|key| key.initial()),
                }),
                hooks: EventDispatcherHooks {
                    on_did_restore: OrderedHookSlot::new(),
                },
                state: Arc::new(Mutex::new(DispatcherState {
                    metas: HashMap::new(),
                    replay_fresh_keys: HashSet::new(),
                    replay_fresh_attachments: HashSet::new(),
                    replay_mutable_states: HashMap::new(),
                    frozen_states: HashSet::new(),
                    folded,
                    active_model_defs,
                    withdrawn_model_ids: HashSet::new(),
                    model_targets: HashMap::new(),
                    model_attachments: HashMap::new(),
                    participant_targets: HashMap::new(),
                    participant_attachments: HashMap::new(),
                    restore_phase: RestorePhase::New,
                    dispatching: false,
                    late_attachment_tail: None,
                    late_attachment_serial: 0,
                    dispatching_since: None,
                    queue_stuck_warned: false,
                    disposed: false,
                    queue: Vec::new(),
                    drain_depth: 0,
                    drain_counts: HashMap::new(),
                    did_run_restore_hooks: false,
                    late_attachments: Vec::new(),
                })),
            }),
            _registrations: Arc::new(Mutex::new(Vec::new())),
        };
        *this.inner.space_host.lock() = this.build_space_host();
        this.register_contribution_view(contribution_view);
        this.register_model_view(model_view);
        this.refold_models(model_view.items.clone());
        if let Some(space) = this.space() {
            space.attach_host(this.inner.space_host.lock().clone());
        }
        this.materialize_undoable_models();
        this
    }

    fn build_space_host(&self) -> AgentSpaceHost {
        let weak = Arc::downgrade(&self.inner);
        AgentSpaceHost {
            is_active_model_definition: {
                let weak = weak.clone();
                Arc::new(move |definition| {
                    weak.upgrade().is_some_and(|inner| {
                        inner
                            .state
                            .lock()
                            .active_model_defs
                            .get(&definition.id)
                            .is_some_and(|current| Arc::ptr_eq(current, definition))
                    })
                })
            },
            register_model: {
                let weak = weak.clone();
                Arc::new(move |definition, model| {
                    if let Some(inner) = weak.upgrade() {
                        Self { inner: weak_inner(&inner) }.register_model(definition, model);
                    }
                })
            },
            dispatch_model_event: {
                let weak = weak.clone();
                Arc::new(move |event| {
                    let weak = weak.clone();
                    async move {
                        if let Some(inner) = weak.upgrade() {
                            return Self { inner: weak_inner(&inner) }.dispatch(event).await;
                        }
                        Ok(())
                    }
                    .boxed()
                })
            },
            read_legacy_state: {
                let agent_state = self.inner.agent_state.clone();
                Arc::new(move |key| agent_state.get(&key.state_key()))
            },
        }
    }

    fn register_contribution_view(&self, view: CollectionView<EventStateContributionRecord>) {
        let weak = Arc::downgrade(&self.inner);
        self._registrations.lock().push(view.on_did_change(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.state.lock().folded = fold_event_state_contributions(
                    &view.items,
                    &inner.agent_state.replayable_keys(),
                );
            }
        })));
        let weak = Arc::downgrade(&self.inner);
        self._registrations.lock().push(self.inner.agent_state.on_did_contribute_replayable(Arc::new(move |key| {
            if let Some(inner) = weak.upgrade() {
                let phase = inner.state.lock().restore_phase;
                if phase != RestorePhase::New {
                    panic!(
                        "Replayable state '{}' contributed while the event dispatcher is in phase '{}'; replayable state owners must contribute before restore",
                        key.name,
                        phase.as_str()
                    );
                }
                inner.state.lock().folded = fold_event_state_contributions(
                    &view.items,
                    &inner.agent_state.replayable_keys(),
                );
            }
            Ok(())
        })));
        let weak = Arc::downgrade(&self.inner);
        self._registrations.lock().push(self.inner.agent_state.on_did_withdraw_replayable(Arc::new(move |key| {
            if let Some(inner) = weak.upgrade() {
                let address = Arc::as_ptr(key) as usize;
                inner.state.lock().metas.remove(&address);
                inner.state.lock().folded = fold_event_state_contributions(
                    &view.items,
                    &inner.agent_state.replayable_keys(),
                );
            }
            Ok(())
        })));
    }

    fn register_model_view(&self, view: CollectionView<Arc<AgentModelDefinition>>) {
        let weak = Arc::downgrade(&self.inner);
        self._registrations.lock().push(view.on_did_change(Arc::new(move |change| {
            if let Some(inner) = weak.upgrade() {
                for definition in &change.removed {
                    inner.state.lock().withdrawn_model_ids.insert(definition.id.clone());
                    let address = Arc::as_ptr(definition) as usize;
                    if let Some(attachment) = inner.state.lock().model_attachments.remove(&address) {
                        Self { inner: weak_inner(&inner) }.detach_participant(&attachment);
                        if let Some(space) = (Self { inner: weak_inner(&inner) }).space() {
                            space.retire_model(definition);
                        }
                    }
                }
                for definition in &change.added {
                    inner.state.lock().withdrawn_model_ids.remove(&definition.id);
                }
                let this = Self { inner: weak_inner(&inner) };
                this.refold_models(view.items.clone());
                this.materialize_undoable_models();
            }
        })));
    }

    fn space(&self) -> Option<Arc<AgentSpaceImpl>> {
        self.inner
            .agent_scope
            .as_ref()
            .and_then(|scope| scope.agent_context().space())
    }

    pub fn attach(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> Box<dyn IDisposable> {
        if self.inner.state.lock().restore_phase != RestorePhase::New {
            panic!(
                "Agent runtime participant '{}' attached while the event dispatcher is in phase '{}'; durable runtime owners must attach before restore",
                participant.id(),
                self.inner.state.lock().restore_phase.as_str()
            );
        }
        let attachment = self.build_participant_attachment(participant);
        self.attach_participant(attachment.clone());
        to_disposable(move || {
            self.detach_participant(&attachment);
        })
    }

    pub async fn attach_late(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> Result<Box<dyn IDisposable>, WireAsyncError> {
        if self.inner.state.lock().restore_phase == RestorePhase::Restoring {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.inner.state.lock().late_attachments.push(LateAttachment {
                participant,
                resolve: Arc::new(move |disposable| {
                    let _ = sender.send(Ok(disposable));
                }),
                reject: Arc::new(move |error| {
                    let _ = sender.send(Err(error));
                }),
            });
            return receiver.await.unwrap_or_else(|_| Err("late attach cancelled".into()));
        }
        if self.inner.state.lock().restore_phase != RestorePhase::Ready {
            return Err(format!(
                "Agent runtime participant '{}' late-attached while the event dispatcher is in phase '{}'; late attach requires a restored dispatcher",
                participant.id(),
                self.inner.state.lock().restore_phase.as_str()
            ).into());
        }
        let previous = self.inner.state.lock().late_attachment_tail.clone();
        let this = self.clone();
        let pending = async move {
            if let Some(previous) = previous {
                previous.await;
            }
            this.attach_late_now(participant).await.map_err(SharedDispatchError::new)
        }
        .boxed()
        .shared();
        let settled = {
            let pending = pending.clone();
            async move {
                let _ = pending.await;
            }
            .boxed()
            .shared()
        };
        let serial = {
            let mut state = self.inner.state.lock();
            state.late_attachment_serial += 1;
            let serial = state.late_attachment_serial;
            state.late_attachment_tail = Some(settled.clone());
            serial
        };
        let state = self.inner.state.clone();
        tokio::spawn(async move {
            settled.await;
            let mut state = state.lock();
            if state.late_attachment_serial == serial {
                state.late_attachment_tail = None;
            }
        });
        pending.await.map_err(SharedDispatchError::boxed)
    }

    async fn attach_late_now(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> Result<Box<dyn IDisposable>, WireAsyncError> {
        if self.inner.state.lock().disposed {
            return Err(format!(
                "Agent runtime participant '{}' late-attached to a disposed event dispatcher",
                participant.id()
            ).into());
        }
        let attachment = self.build_participant_attachment(participant);
        {
            let mut state = self.inner.state.lock();
            state.dispatching = true;
            state.dispatching_since = Some(now_ms());
        }
        let mut replay_failure = None;
        let result = async {
            self.inner.wire.flush().await?;
            let stream = if attachment.undoable {
                self.inner.wire.read_restorable()
            } else {
                self.inner.wire.read_journal()
            };
            futures_util::pin_mut!(stream);
            while let Some(record) = stream.next().await {
                let record = record?;
                if record.get("type").and_then(serde_json::Value::as_str) == Some("metadata") {
                    continue;
                }
                let Some(resolved) = self.resolve_record(&record) else {
                    continue;
                };
                let Some(event) = resolved.event else {
                    continue;
                };
                let Some(applier) = attachment.appliers.get(&event.type_).cloned() else {
                    continue;
                };
                let mut ctx = FoldContextImpl {
                    owner: Arc::downgrade(&self.inner),
                    silent: true,
                    pending_checkpoint: false,
                    pending_clear: false,
                    pending_undo: None,
                };
                let next = applier((attachment.get_state)(), &event, &mut ctx);
                if ctx.pending_undo.is_some() && next.as_ref() != Some(&(attachment.get_state)()) {
                    return Err(format!(
                        "Fold of event '{}' on durable participant '{}' both mutates and undoes to a checkpoint",
                        event.type_, attachment.id
                    ).into());
                }
                ctx.sanitize_pending_undo(&attachment.meta.lock());
                if let Some(next) = next {
                    self.commit_participant(&attachment, &mut ctx, next);
                }
            }
            self.attach_participant(attachment.clone());
            (attachment.commit)((attachment.get_state)(), false);
            Ok(())
        }
        .await;
        if let Err(error) = result {
            replay_failure = Some(error);
        }
        let mut drain_failure = None;
        if self.inner.state.lock().restore_phase != RestorePhase::Restoring {
            if let Err(error) = self.drain_queue() {
                for entry in self.inner.state.lock().queue.drain(..) {
                    (entry.reject)(error.to_string().into());
                }
                drain_failure = Some(error);
            }
        }
        {
            let mut state = self.inner.state.lock();
            if state.restore_phase != RestorePhase::Restoring {
                state.queue.clear();
                state.dispatching = false;
                state.dispatching_since = None;
                state.queue_stuck_warned = false;
                state.drain_depth = 0;
                state.drain_counts.clear();
            }
        }
        if let Some(error) = drain_failure {
            return Err(error);
        }
        if let Some(error) = replay_failure {
            return Err(error);
        }
        Ok(to_disposable(move || {
            self.detach_participant(&attachment);
        }))
    }

    fn build_participant_attachment(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> ParticipantAttachment {
        let base = participant
            .events()
            .into_iter()
            .map(|cls| (cls.type_.clone(), participant.transition()))
            .collect::<HashMap<_, _>>();
        let folds = expanded_runtime_folds(participant.id(), participant.undoable(), &base)
            .expect("failed to expand runtime folds");
        let appliers = folds
            .into_iter()
            .map(|(cls, fold)| {
                (
                    cls.type_.clone(),
                    Arc::new(move |state, event, ctx| fold(state, event, ctx)) as ParticipantApplier,
                )
            })
            .collect();
        let initial = participant.get_state();
        ParticipantAttachment {
            id: participant.id().to_string(),
            appliers,
            meta: Arc::new(Mutex::new(StateMeta {
                checkpoints: Vec::new(),
            })),
            undoable: participant.undoable(),
            initial,
            get_state: Arc::new(move || participant.get_state()),
            commit: Arc::new(move |state, silent| participant.commit(state, silent)),
        }
    }

    fn attach_participant(&self, attachment: ParticipantAttachment) {
        if self
            .inner
            .state
            .lock()
            .participant_attachments
            .contains_key(&attachment.id)
        {
            panic!("Durable participant '{}' is already attached", attachment.id);
        }
        self.inner
            .state
            .lock()
            .participant_attachments
            .insert(attachment.id.clone(), attachment.clone());
        for event_type in attachment.appliers.keys() {
            self.inner
                .state
                .lock()
                .participant_targets
                .entry(event_type.clone())
                .or_default()
                .push(attachment.clone());
        }
    }

    fn detach_participant(&self, attachment: &ParticipantAttachment) {
        if self
            .inner
            .state
            .lock()
            .participant_attachments
            .get(&attachment.id)
            .is_none_or(|current| !std::ptr::eq(current, attachment))
        {
            return;
        }
        self.inner
            .state
            .lock()
            .participant_attachments
            .remove(&attachment.id);
        for event_type in attachment.appliers.keys() {
            let mut state = self.inner.state.lock();
            let Some(list) = state.participant_targets.get_mut(event_type) else {
                continue;
            };
            list.retain(|candidate| candidate.id != attachment.id || !std::ptr::eq(candidate, attachment));
            if list.is_empty() {
                state.participant_targets.remove(event_type);
            }
        }
    }

    fn refold_models(&self, records: Vec<Arc<AgentModelDefinition>>) {
        let mut defs = IndexMap::new();
        for definition in agent_model_definitions() {
            if !self.inner.state.lock().withdrawn_model_ids.contains(&definition.id) {
                defs.insert(definition.id.clone(), definition);
            }
        }
        for definition in records {
            defs.insert(definition.id.clone(), definition);
        }
        self.inner.state.lock().active_model_defs = defs;
        self.rebuild_model_targets();
    }

    fn rebuild_model_targets(&self) {
        let mut targets = HashMap::<String, Vec<Arc<AgentModelDefinition>>>::new();
        let mut add = |event_type: &str, definition: &Arc<AgentModelDefinition>| {
            let list = targets.entry(event_type.to_string()).or_default();
            if !list.iter().any(|candidate| Arc::ptr_eq(candidate, definition)) {
                list.push(definition.clone());
            }
        };
        let mut domain_owners = HashMap::<String, Arc<AgentModelDefinition>>::new();
        for definition in self.inner.state.lock().active_model_defs.values() {
            for cls in &definition.events {
                if let Some(owner) = domain_owners.get(&cls.type_) {
                    if !Arc::ptr_eq(owner, definition) {
                        panic!(
                            "Event '{}' is applied by both agent models '{}' and '{}'",
                            cls.type_, owner.id, definition.id
                        );
                    }
                }
                domain_owners.insert(cls.type_.clone(), definition.clone());
                add(&cls.type_, definition);
            }
        }
        let attachments = self
            .inner
            .state
            .lock()
            .model_attachments
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for attachment in attachments {
            let Some(definition) = self
                .inner
                .state
                .lock()
                .active_model_defs
                .get(&attachment.id)
                .cloned()
            else {
                continue;
            };
            for event_type in attachment.appliers.keys() {
                add(event_type, &definition);
            }
        }
        self.inner.state.lock().model_targets = targets;
    }

    fn materialize_undoable_models(&self) {
        let Some(space) = self.space() else {
            return;
        };
        for definition in self.inner.state.lock().active_model_defs.values() {
            if definition.undoable
                && !self
                    .inner
                    .state
                    .lock()
                    .model_attachments
                    .contains_key(&(Arc::as_ptr(definition) as usize))
            {
                space.ensure_model(definition);
            }
        }
    }

    fn register_model(&self, definition: &Arc<AgentModelDefinition>, model: &AgentModel) {
        let address = Arc::as_ptr(definition) as usize;
        if self.inner.state.lock().model_attachments.contains_key(&address) {
            return;
        }
        let domain_appliers = model
            .appliers_table()
            .iter()
            .map(|(address, applier)| {
                let applier = applier.clone();
                let model = model.clone();
                (
                    *address,
                    Arc::new(move |event: &Event2, _ctx: &mut FoldContextImpl| {
                        applier(event);
                    }) as ParticipantApplier,
                )
            })
            .collect::<HashMap<_, _>>();
        let custom_undo = model.on_undo_handler.clone();
        let expanded = expanded_model_appliers(&definition.id, definition.undoable, &domain_appliers, custom_undo)
            .expect("failed to expand model appliers");
        let appliers = expanded
            .into_iter()
            .map(|(address, applier)| {
                let model = model.clone();
                (
                    address,
                    Arc::new(move |state, event, ctx| {
                        model.enter_window(state, Arc::new(Mutex::new(ctx.clone())));
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            applier(event, ctx);
                        }));
                        let window = model.exit_window();
                        match result {
                            Ok(()) => window.1,
                            Err(error) => std::panic::resume_unwind(error),
                        }
                    }) as ParticipantApplier,
                )
            })
            .collect();
        let attachment = ParticipantAttachment {
            id: definition.id.clone(),
            appliers,
            meta: Arc::new(Mutex::new(StateMeta {
                checkpoints: Vec::new(),
            })),
            undoable: definition.undoable,
            initial: model.state_value(),
            get_state: Arc::new({
                let model = model.clone();
                move || model.state_value()
            }),
            commit: Arc::new({
                let model = model.clone();
                move |state, _silent| model.commit_state(state)
            }),
        };
        self.attach_participant(attachment);
        self.inner
            .state
            .lock()
            .model_attachments
            .insert(address, attachment);
        self.rebuild_model_targets();
    }

    fn materialize_model(&self, definition: &Arc<AgentModelDefinition>) -> ParticipantAttachment {
        let Some(space) = self.space() else {
            panic!("Agent model '{}' cannot materialize without an agent space", definition.id);
        };
        space.ensure_model(definition);
        self.inner
            .state
            .lock()
            .model_attachments
            .get(&(Arc::as_ptr(definition) as usize))
            .cloned()
            .unwrap_or_else(|| panic!("Agent model '{}' failed to attach", definition.id))
    }

    pub async fn dispatch(&self, event: Event2) -> Result<(), WireAsyncError> {
        let class = self
            .inner
            .state
            .lock()
            .folded
            .events
            .get(&event.type_)
            .cloned();
        if class.as_ref().is_some_and(|class| class.agent_domain) {
            let matches = self
                .inner
                .agent_scope
                .as_ref()
                .is_some_and(|scope| scope.agent_id() == event.payload.get("agentId").and_then(serde_json::Value::as_str).unwrap_or(""));
            if !matches {
                return Err(format!(
                    "Agent event '{}' does not match dispatcher lifecycle context",
                    event.type_
                ).into());
            }
        }
        if self.inner.state.lock().dispatching {
            let (warn, agent_id, event_type, waiting_ms) = {
                let state = self.inner.state.lock();
                (
                    !state.queue_stuck_warned
                        && state.dispatching_since.is_some_and(|since| now_ms() - since > DISPATCH_STUCK_WARN_MS),
                    self.inner.agent_scope.as_ref().map(|scope| scope.agent_id()),
                    event.type_.clone(),
                    state.dispatching_since.map(|since| now_ms() - since),
                )
            };
            if warn {
                self.inner.state.lock().queue_stuck_warned = true;
                self.inner.logger.warn(
                    "event dispatch is stuck; queueing event behind it",
                    Some(serde_json::json!({
                        "agentId": agent_id,
                        "event": event_type,
                        "waitingMs": waiting_ms,
                    })),
                );
            }
            let (sender, receiver) = tokio::sync::oneshot::channel();
            self.inner.state.lock().queue.push(QueuedEvent {
                event,
                resolve: Arc::new(move || {
                    let _ = sender.send(Ok(()));
                }),
                reject: Arc::new(move |error| {
                    let _ = sender.send(Err(error));
                }),
            });
            return receiver.await.unwrap_or_else(|_| Err("event dispatch cancelled".into()));
        }
        {
            let mut state = self.inner.state.lock();
            state.dispatching = true;
            state.dispatching_since = Some(now_ms());
        }
        let result = self.run_dispatch(event);
        let result = if let Err(error) = self.drain_queue() {
            for entry in self.inner.state.lock().queue.drain(..) {
                (entry.reject)(error.to_string().into());
            }
            result.and(Err(error))
        } else {
            result
        };
        {
            let mut state = self.inner.state.lock();
            state.queue.clear();
            state.dispatching = false;
            state.dispatching_since = None;
            state.queue_stuck_warned = false;
            state.drain_depth = 0;
            state.drain_counts.clear();
        }
        result
    }

    fn run_dispatch(&self, event: Event2) -> Result<(), WireAsyncError> {
        self.execute_event(event, false, None)
    }

    fn execute_event(&self, event: Event2, silent: bool, replay_undoable: Option<bool>) -> Result<(), WireAsyncError> {
        let folds = self.inner.state.lock().folded.folds.get(&event.type_).cloned();
        let mut prepared = Vec::new();
        if let Some(folds) = &folds {
            for registration in folds {
                if replay_undoable.is_some_and(|undoable| {
                    registration.key.replayable.undoable.is_some() != undoable
                }) {
                    continue;
                }
                let meta = self.ensure_meta(&registration.key);
                let mut ctx = FoldContextImpl {
                    owner: Arc::downgrade(&self.inner),
                    silent,
                    pending_checkpoint: false,
                    pending_clear: false,
                    pending_undo: None,
                };
                let key_address = Arc::as_ptr(&registration.key) as usize;
                let staged = self.inner.state.lock().replay_mutable_states.contains_key(&key_address);
                if !silent && staged {
                    let next = self.inner.state.lock().replay_mutable_states.get(&key_address).cloned().unwrap();
                    self.inner.agent_state.set(&registration.key.state_key(), next);
                }
                let current = if staged {
                    self.inner.state.lock().replay_mutable_states.get(&key_address).cloned().unwrap()
                } else {
                    self.inner.agent_state.get(&registration.key.state_key())
                };
                let mutable_replay = silent && staged && !self.inner.state.lock().frozen_states.contains(&key_address);
                let produced = (registration.fold)(current.clone(), &event, &mut ctx);
                let next = if mutable_replay && produced.is_none() {
                    current.clone()
                } else {
                    produced.unwrap_or(current.clone())
                };
                if ctx.pending_undo.is_some() && next != current {
                    return Err(format!(
                        "Fold of event '{}' on state '{}' both mutates and undoes to a checkpoint",
                        event.type_, registration.key.name
                    ).into());
                }
                ctx.sanitize_pending_undo(&meta.lock());
                if silent && staged && (ctx.pending_checkpoint || ctx.pending_undo.is_some()) {
                    return Err(format!(
                        "State key '{}' cannot use checkpoints during mutable replay",
                        registration.key.name
                    ).into());
                }
                prepared.push(PreparedFold {
                    key: registration.key.clone(),
                    meta,
                    ctx,
                    next,
                });
            }
        }
        if let Some(targets) = self.inner.state.lock().model_targets.get(&event.type_).cloned() {
            for definition in targets {
                if replay_undoable.is_some_and(|undoable| definition.undoable != undoable) {
                    continue;
                }
                if !self
                    .inner
                    .state
                    .lock()
                    .model_attachments
                    .contains_key(&(Arc::as_ptr(&definition) as usize))
                {
                    self.materialize_model(&definition);
                }
            }
        }
        let targets = self.inner.state.lock().participant_targets.get(&event.type_).cloned();
        let mut prepared_participants = Vec::new();
        if let Some(targets) = targets {
            for attachment in targets {
                if replay_undoable.is_some_and(|undoable| attachment.undoable != undoable) {
                    continue;
                }
                let Some(applier) = attachment.appliers.get(&event.type_).cloned() else {
                    continue;
                };
                let mut ctx = FoldContextImpl {
                    owner: Arc::downgrade(&self.inner),
                    silent,
                    pending_checkpoint: false,
                    pending_clear: false,
                    pending_undo: None,
                };
                let current = (attachment.get_state)();
                let next = applier(current.clone(), &event, &mut ctx).unwrap_or(current.clone());
                if ctx.pending_undo.is_some() && next != current {
                    return Err(format!(
                        "Fold of event '{}' on durable participant '{}' both mutates and undoes to a checkpoint",
                        event.type_, attachment.id
                    ).into());
                }
                ctx.sanitize_pending_undo(&attachment.meta.lock());
                prepared_participants.push(PreparedParticipant {
                    attachment,
                    ctx,
                    next,
                });
            }
        }
        for prepared in prepared {
            self.commit(&prepared.key, &prepared.meta, &mut prepared.ctx, prepared.next);
        }
        for prepared in prepared_participants {
            self.commit_participant(&prepared.attachment, &mut prepared.ctx, prepared.next);
        }
        if silent {
            return Ok(());
        }
        if let Some(cls) = self.inner.state.lock().folded.events.get(&event.type_).cloned() {
            if cls.durable {
                let dehydrator = folds.as_ref().and_then(|folds| {
                    folds.iter().find_map(|registration| {
                        registration
                            .key
                            .replayable
                            .blobs
                            .as_ref()
                            .map(|codec| codec.dehydrate.clone())
                    })
                });
                self.inner.wire.append_record(event.serialize(), dehydrator);
            }
            if cls.observable && !self.inner.state.lock().disposed {
                self.inner.event_bus.publish(event, self.inner.agent_scope.as_ref().map(|scope| scope.agent_context()));
            }
        }
        Ok(())
    }

    pub fn dispose(&self) {
        self.inner.state.lock().disposed = true;
        let pending = self.inner.state.lock().late_attachments.drain(..).collect::<Vec<_>>();
        for pending in pending {
            (pending.reject)("Event dispatcher disposed while a late attach was pending".into());
        }
        if let Some(space) = self.space() {
            space.detach_host(&self.inner.space_host.lock().clone());
        }
        let registrations = self._registrations.lock().drain(..).collect::<Vec<_>>();
        for registration in registrations {
            registration.dispose();
        }
    }

    fn commit(&self, key: &Arc<ReplayableStateKey>, meta: &Arc<Mutex<StateMeta>>, ctx: &mut FoldContextImpl, next: Value) {
        let address = Arc::as_ptr(key) as usize;
        if let Some(count) = ctx.pending_undo {
            let target_index = meta.lock().checkpoints.len() - count as usize;
            let snapshot = meta.lock().checkpoints[target_index].clone();
            self.inner.agent_state.set(&key.state_key(), snapshot);
            meta.lock().checkpoints.truncate(target_index);
            return;
        }
        let staged = self.inner.state.lock().replay_mutable_states.contains_key(&address);
        if ctx.silent && (staged || next != self.inner.agent_state.get(&key.state_key())) {
            self.inner.state.lock().replay_fresh_keys.insert(address);
        }
        if staged {
            if !ctx.silent {
                self.inner.agent_state.set(&key.state_key(), next.clone());
            }
            self.inner.state.lock().replay_mutable_states.insert(address, next.clone());
        } else {
            self.inner.agent_state.set(&key.state_key(), next.clone());
        }
        if ctx.pending_clear {
            meta.lock().checkpoints.clear();
        }
        if ctx.pending_checkpoint {
            meta.lock().checkpoints.push(next);
        }
    }

    fn commit_participant(&self, attachment: &ParticipantAttachment, ctx: &mut FoldContextImpl, next: Value) {
        let meta = attachment.meta.lock();
        if let Some(count) = ctx.pending_undo {
            let target_index = meta.checkpoints.len() - count as usize;
            let snapshot = meta.checkpoints[target_index].clone();
            (attachment.commit)(snapshot, ctx.silent);
            meta.checkpoints.truncate(target_index);
            return;
        }
        if ctx.silent && next != (attachment.get_state)() {
            self.inner
                .state
                .lock()
                .replay_fresh_attachments
                .insert(attachment.id.as_ptr() as usize);
        }
        (attachment.commit)(next, ctx.silent);
        if ctx.pending_clear {
            meta.checkpoints.clear();
        }
        if ctx.pending_checkpoint {
            meta.checkpoints.push(next);
        }
    }

    fn ensure_meta(&self, key: &Arc<ReplayableStateKey>) -> Arc<Mutex<StateMeta>> {
        let address = Arc::as_ptr(key) as usize;
        self.inner
            .state
            .lock()
            .metas
            .entry(address)
            .or_insert_with(|| Arc::new(Mutex::new(StateMeta {
                checkpoints: Vec::new(),
            })))
            .clone()
    }

    pub async fn restore(&self) -> Result<(), WireAsyncError> {
        if self.inner.state.lock().restore_phase == RestorePhase::Restoring {
            return Err(format!(
                "Agent state restore called while phase is {}",
                self.inner.state.lock().restore_phase.as_str()
            ).into());
        }
        let rerun = self.inner.state.lock().restore_phase != RestorePhase::New;
        self.inner.state.lock().restore_phase = RestorePhase::Restoring;
        if rerun {
            let mut state = self.inner.state.lock();
            state.dispatching = true;
            state.dispatching_since = Some(now_ms());
        }
        let result = async {
            if let Some(tail) = self.inner.state.lock().late_attachment_tail.clone() {
                tail.await;
            }
            if rerun {
                self.inner.wire.flush().await?;
                self.reset_replay_state();
            }
            self.replay_records().await?;
            self.freeze_replayed_states();
            self.rehydrate_states().await?;
            self.inner.state.lock().restore_phase = RestorePhase::Ready;
            if !self.inner.state.lock().did_run_restore_hooks {
                self.hooks().on_did_restore.run(()).await?;
                self.inner.state.lock().did_run_restore_hooks = true;
            }
            if rerun {
                self.drain_queue()?;
            }
            self.drain_late_attachments().await
        }
        .await;
        if let Err(error) = result {
            self.inner.state.lock().replay_mutable_states.clear();
            self.inner.state.lock().restore_phase = RestorePhase::Failed;
            for pending in self.inner.state.lock().late_attachments.drain(..) {
                (pending.reject)(error.to_string().into());
            }
            if rerun {
                for entry in self.inner.state.lock().queue.drain(..) {
                    (entry.reject)(error.to_string().into());
                }
                let mut state = self.inner.state.lock();
                state.queue.clear();
                state.dispatching = false;
                state.dispatching_since = None;
                state.queue_stuck_warned = false;
                state.drain_depth = 0;
                state.drain_counts.clear();
            }
            return Err(error);
        }
        if rerun {
            let mut state = self.inner.state.lock();
            state.queue.clear();
            state.dispatching = false;
            state.dispatching_since = None;
            state.queue_stuck_warned = false;
            state.drain_depth = 0;
            state.drain_counts.clear();
        }
        Ok(())
    }

    async fn drain_late_attachments(&self) -> Result<(), WireAsyncError> {
        let pending = self.inner.state.lock().late_attachments.drain(..).collect::<Vec<_>>();
        for pending in pending {
            match self.attach_late(pending.participant).await {
                Ok(disposable) => (pending.resolve)(disposable),
                Err(error) => (pending.reject)(error),
            }
        }
        Ok(())
    }

    fn reset_replay_state(&self) {
        self.inner.state.lock().replay_mutable_states.clear();
        self.inner.state.lock().replay_fresh_keys.clear();
        self.inner.state.lock().replay_fresh_attachments.clear();
        for key in self.inner.agent_state.replayable_keys() {
            self.inner.agent_state.set(&key.state_key(), (key.initial)());
        }
        for attachment in self
            .inner
            .state
            .lock()
            .participant_attachments
            .values()
            .cloned()
            .collect::<Vec<_>>()
        {
            (attachment.commit)(attachment.initial.clone(), true);
            self.inner
                .state
                .lock()
                .replay_fresh_attachments
                .insert(attachment.id.as_ptr() as usize);
            attachment.meta.lock().checkpoints.clear();
        }
        self.inner.state.lock().metas.clear();
    }

    async fn replay_records(&self) -> Result<(), WireAsyncError> {
        self.inner.state.lock().replay_mutable_states.clear();
        for key in self.inner.agent_state.replayable_keys() {
            if key.replayable.mutable_replay {
                let current = self.inner.agent_state.get(&key.state_key());
                self.inner
                    .state
                    .lock()
                    .replay_mutable_states
                    .insert(Arc::as_ptr(&key) as usize, current);
            }
        }
        let chains = self.inner.wire.read_restore_chains().await?;
        let mut resolved = HashMap::<*const std::collections::HashMap<String, Value>, ResolvedRecord>::new();
        self.inner.state.lock().replay_fresh_keys.clear();
        self.replay_pass(&chains.restorable, true, &mut resolved)?;
        self.replay_pass(&chains.journal, false, &mut resolved)
    }

    fn replay_pass(
        &self,
        records: &[WireRecord],
        undoable: bool,
        resolved: &mut HashMap<*const std::collections::HashMap<String, Value>, ResolvedRecord>,
    ) -> Result<(), WireAsyncError> {
        let mut record_index = 0;
        for record in records {
            let record_type = record.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
            if record_type == "metadata" {
                continue;
            }
            let key = record as *const std::collections::HashMap<String, Value>;
            let entry = resolved.entry(key).or_insert_with(|| self.resolve_record(record));
            if entry.event.is_none() {
                if !undoable {
                    if entry.cls.is_none() {
                        if !unreported_wire_record_type(record_type) && !is_human_record_type(record_type) {
                            self.report_skipped_record(record_type, record_index, false);
                        }
                    } else {
                        self.report_skipped_record(record_type, record_index, true);
                    }
                }
                record_index += 1;
                continue;
            }
            let event = entry.event.clone().unwrap();
            self.execute_event(event, true, Some(undoable))?;
            record_index += 1;
        }
        Ok(())
    }

    fn resolve_record(&self, record: &WireRecord) -> ResolvedRecord {
        let Some(cls) = self
            .inner
            .state
            .lock()
            .folded
            .events
            .get(record.get("type").and_then(serde_json::Value::as_str).unwrap_or(""))
            .cloned()
        else {
            return ResolvedRecord {
                cls: None,
                event: None,
            };
        };
        let mut event_record = record.clone();
        if cls.agent_domain {
            let Some(scope) = &self.inner.agent_scope else {
                return ResolvedRecord {
                    cls: Some(cls),
                    event: None,
                };
            };
            let record_agent_id = record.get("agentId").and_then(serde_json::Value::as_str);
            if let Some(record_agent_id) = record_agent_id {
                if record_agent_id != scope.agent_id() {
                    return ResolvedRecord {
                        cls: Some(cls),
                        event: None,
                    };
                }
            } else {
                event_record.insert("agentId".to_string(), scope.agent_id().into());
            }
        }
        ResolvedRecord {
            cls: cls.clone(),
            event: event2_from_record(&cls, &Value::Object(event_record)),
        }
    }

    fn freeze_replayed_states(&self) {
        let fresh = self.inner.state.lock().replay_fresh_keys.drain().collect::<Vec<_>>();
        for address in fresh {
            if let Some(next) = self.inner.state.lock().replay_mutable_states.remove(&address) {
                let key = self
                    .inner
                    .agent_state
                    .replayable_keys()
                    .into_iter()
                    .find(|key| Arc::as_ptr(key) as usize == address)
                    .unwrap();
                self.inner.agent_state.set(&key.state_key(), next);
                self.inner.state.lock().frozen_states.insert(address);
            } else {
                self.inner.state.lock().frozen_states.insert(address);
            }
        }
        let attachments = self.inner.state.lock().replay_fresh_attachments.drain().collect::<Vec<_>>();
        for address in attachments {
            if let Some(attachment) = self
                .inner
                .state
                .lock()
                .participant_attachments
                .values()
                .find(|attachment| attachment.id.as_ptr() as usize == address)
                .cloned()
            {
                (attachment.commit)((attachment.get_state)(), false);
            }
        }
        self.inner.state.lock().replay_mutable_states.clear();
    }

    fn report_skipped_record(&self, record_type: &str, index: usize, malformed: bool) {
        let message = if malformed {
            format!("Malformed wire record type '{record_type}' skipped during restore")
        } else {
            format!("Unknown wire record type '{record_type}' skipped during restore")
        };
        if malformed {
            let mut details = serde_json::Map::new();
            details.insert("type".to_string(), record_type.into());
            details.insert("index".to_string(), index.into());
            on_unexpected_error(Box::new(WireError::with_options(
                WIRE_UNKNOWN_RECORD,
                message,
                kimi_core::primitives::errors::Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )));
        } else {
            self.inner.logger.warn(
                &message,
                Some(serde_json::json!({
                    "code": WIRE_UNKNOWN_RECORD,
                    "type": record_type,
                    "index": index,
                })),
            );
        }
    }

    async fn rehydrate_states(&self) -> Result<(), WireAsyncError> {
        let transform: PartsTransformer = {
            let blob_service = self.inner.blob_service.clone();
            Arc::new(move |parts| {
                let blob_service = blob_service.clone();
                async move { blob_service.load_parts(parts).await }.boxed()
            })
        };
        for key in self.inner.state.lock().folded.states.clone() {
            let Some(codec) = &key.replayable.blobs else {
                continue;
            };
            let rehydrate = &codec.rehydrate;
            let current = self.inner.agent_state.get(&key.state_key());
            let next = rehydrate(current, transform.clone()).await?;
            self.inner.agent_state.set(&key.state_key(), next);
        }
        Ok(())
    }

    pub async fn flush(&self) -> Result<(), WireAsyncError> {
        self.inner.wire.flush().await
    }

    fn hooks(&self) -> &EventDispatcherHooks {
        &self.inner.hooks
    }
}

impl EventDispatcherServiceInner {
    fn enqueue(&self, event: Event2) {
        self.inner_state().queue.push(QueuedEvent {
            event,
            resolve: Arc::new(|| {}),
            reject: Arc::new(|error| on_unexpected_error(error)),
        });
    }

    fn inner_state(&self) -> parking_lot::MutexGuard<'_, DispatcherState> {
        self.state.lock()
    }

    fn drain_queue(&self) -> Result<(), WireAsyncError> {
        loop {
            let next = self.inner_state().queue.first().cloned();
            let Some(next) = next else {
                return Ok(());
            };
            let (depth, seen) = {
                let mut state = self.inner_state();
                state.drain_depth += 1;
                let seen = state.drain_counts.entry(next.event.type_.clone()).or_insert(0);
                *seen += 1;
                (state.drain_depth, *seen)
            };
            if depth > MAX_DRAIN || seen > MAX_DRAIN_PER_TYPE {
                return Err(CycleError::new(
                    depth,
                    self.inner_state()
                        .queue
                        .iter()
                        .map(|entry| entry.event.type_.clone())
                        .collect(),
                    Some(next.event.type_.clone()),
                ).into());
            }
            self.inner_state().queue.remove(0);
            EventDispatcherService {
                inner: self.clone(),
                _registrations: Arc::new(Mutex::new(Vec::new())),
            }
            .run_dispatch(next.event)?;
            (next.resolve)();
        }
    }
}

#[async_trait]
impl DurableRuntimeParticipantHost for EventDispatcherService {
    fn attach(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> Box<dyn IDisposable> {
        EventDispatcherService::attach(self, participant)
    }
}

#[async_trait]
impl IEventDispatcher for EventDispatcherService {
    fn hooks(&self) -> &EventDispatcherHooks {
        EventDispatcherService::hooks(self)
    }

    fn restore_phase(&self) -> RestorePhase {
        self.inner.state.lock().restore_phase
    }

    async fn dispatch(&self, event: Event2) -> Result<(), WireAsyncError> {
        EventDispatcherService::dispatch(self, event).await
    }

    async fn attach_late(&self, participant: Arc<dyn DurableAgentRuntimeParticipant>) -> Result<Box<dyn IDisposable>, WireAsyncError> {
        EventDispatcherService::attach_late(self, participant).await
    }

    async fn restore(&self) -> Result<(), WireAsyncError> {
        EventDispatcherService::restore(self).await
    }

    async fn flush(&self) -> Result<(), WireAsyncError> {
        EventDispatcherService::flush(self).await
    }
}

fn unreported_wire_record_type(record_type: &str) -> bool {
    matches!(record_type, "staleGuard.recorded" | "staleGuard.cleared" | AGENT_SWITCHED_TYPE | "context.undone")
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn weak_inner(inner: &Arc<EventDispatcherServiceInner>) -> Arc<EventDispatcherServiceInner> {
    inner.clone()
}
