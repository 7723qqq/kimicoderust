use std::collections::{HashMap, HashSet};
use std::fmt;

use indexmap::{IndexMap, IndexSet};
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, FutureExt, Shared, poll_fn};
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::lifecycle::{DisposableRef, to_disposable};
use crate::_base::errors::errors::SharedError;
use crate::_base::utils::abort::{AbortController, AbortError, user_cancellation_reason};
use crate::agent::context_memory::message_id::new_message_id;
use crate::agent::context_memory::types::{ContextMessage, PromptOrigin, user_prompt_origin};
use crate::agent::llm_requester::llm_requester::AgentLLMRequestSource;
use crate::agent::r#loop::machine::engine::{
    CreateMachineEngineOptions, MachineEngine, MachineEngineAttachBundle,
    MachineEngineAttachRef, MachineEngineEvent, MachineEnginePhase, MachineTurnOutcome,
    attach_machine_engine, engine_journal, machine_engine_attach_bundle,
};
use crate::agent::r#loop::machine::history::EMPTY_MACHINE_PROMPT;
use crate::human::agent::turn::{UserEntry, UserMeta};
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::{Role, UserMessage};
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::prompt::{PROMPT_NOT_FOUND, REQUEST_INVALID};

use super::r#loop::{
    AgentActivityEndingReason, AgentActivityPhase, AgentActivityTurnSnapshot, CompletedTurnResult,
    IAgentLoopService, LoopCancelTarget, LoopErrorHandler, LoopErrorHandlerRegistrationOptions,
    LoopHooks, LoopNotify, LoopNotifyHandle, LoopRunResult, LoopSnapshot, LoopState,
    LoopSubmitOptions, LoopSubmitResult, PromptCompletion, PromptHandle, PromptState,
    PromptTerminalState, Turn, TurnResult, TurnState,
};

pub type PromptMaterializer = Arc<
    dyn Fn(UserEntry) -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync,
>;

struct DeferredState<T> {
    value: Option<T>,
    wakers: Vec<Waker>,
}

struct Deferred<T> {
    state: Arc<Mutex<DeferredState<T>>>,
}

impl<T> Clone for Deferred<T> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}

impl<T> Deferred<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(DeferredState {
                value: None,
                wakers: Vec::new(),
            })),
        }
    }

    fn future(&self) -> Shared<BoxFuture<'static, T>> {
        let state = self.state.clone();
        async move {
            poll_fn(move |context| {
                let mut state = state.lock();
                if let Some(value) = state.value.clone() {
                    return Poll::Ready(value);
                }
                if !state
                    .wakers
                    .iter()
                    .any(|waker| waker.will_wake(context.waker()))
                {
                    state.wakers.push(context.waker().clone());
                }
                Poll::Pending
            })
            .await
        }
        .boxed()
        .shared()
    }

    fn resolve(&self, value: T) -> bool {
        let wakers = {
            let mut state = self.state.lock();
            if state.value.is_some() {
                return false;
            }
            state.value = Some(value);
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
        true
    }
}

struct PromptWaiter {
    id: String,
    entry: UserEntry,
    launched: Deferred<Option<Arc<Turn>>>,
    launched_future: Shared<BoxFuture<'static, Option<Arc<Turn>>>>,
    completion: Deferred<PromptCompletion>,
    completion_future: Shared<BoxFuture<'static, PromptCompletion>>,
    on_materialize: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl PromptWaiter {
    fn new(
        id: String,
        entry: UserEntry,
        on_materialize: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let launched = Deferred::new();
        let completion = Deferred::new();
        Self {
            id,
            entry,
            launched_future: launched.future(),
            completion_future: completion.future(),
            launched,
            completion,
            on_materialize,
        }
    }

    fn message(&self) -> ContextMessage {
        context_message_from_entry(&self.entry)
    }
}

struct ActiveTurn {
    id: u64,
    prompt_id: String,
    tracked: bool,
    message: ContextMessage,
    user_message_id: String,
    created_at: String,
    controller: Arc<AbortController>,
    steer_controller: Arc<AbortController>,
    result: Deferred<TurnResult>,
    turn: Arc<Turn>,
    steps: u32,
    started_at: f64,
    last_stop_reason: Option<FinishReason>,
    bypass_max_steps: bool,
}

struct Nudge {
    entry: UserEntry,
    bypass_max_steps: bool,
    turn_scoped: bool,
    sent: bool,
    consumed: bool,
    dropped: bool,
    on_consume: Option<Arc<dyn Fn() + Send + Sync>>,
    on_drop: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Default)]
struct LoopServiceState {
    engine: Option<Arc<MachineEngine>>,
    pending: Vec<UserEntry>,
    waiters: IndexMap<String, Arc<PromptWaiter>>,
    terminal: HashMap<String, PromptState>,
    steered: IndexSet<String>,
    active: Option<ActiveTurn>,
    nudges: Vec<Arc<Mutex<Nudge>>>,
    settle_waiters: Vec<Deferred<()>>,
    error_handlers: Vec<Arc<dyn LoopErrorHandler>>,
    quiescence_depth: usize,
    history_snapshot_depth: usize,
    active_trace: Option<LLMRequestTrace>,
    next_turn_id: u64,
    disposing: bool,
}

pub struct AgentLoopService {
    options: CreateMachineEngineOptions,
    materializer: Option<PromptMaterializer>,
    state: Arc<Mutex<LoopServiceState>>,
    hooks: LoopHooks,
}

impl AgentLoopService {
    pub fn new(options: CreateMachineEngineOptions) -> Self {
        let next_turn_id = options.initial_turn_id.unwrap_or(0);
        Self {
            options,
            materializer: None,
            state: Arc::new(Mutex::new(LoopServiceState {
                next_turn_id,
                ..Default::default()
            })),
            hooks: LoopHooks::default(),
        }
    }

    pub fn with_prompt_materializer(mut self, materializer: PromptMaterializer) -> Self {
        self.materializer = Some(materializer);
        self
    }

    fn engine_options(&self) -> CreateMachineEngineOptions {
        let mut options = self.options.clone();
        let state = self.state.clone();
        let publish = options.on_event.clone();
        options.on_event = Some(Arc::new(move |event| {
            project_machine_event(&state, event.clone());
            if let Some(publish) = &publish {
                publish(event);
            }
        }));
        let state = self.state.clone();
        let on_trace = options.on_trace.clone();
        options.on_trace = Some(Arc::new(move |trace| {
            state.lock().active_trace = Some(trace.clone());
            if let Some(on_trace) = &on_trace {
                on_trace(trace);
            }
        }));
        let state = self.state.clone();
        let previous = options.trace.clone();
        options.trace = Some(Arc::new(move || {
            state
                .lock()
                .active_trace
                .clone()
                .or_else(|| previous.as_ref().and_then(|trace| trace()))
        }));
        let state = self.state.clone();
        let previous = options.tool_turn_id.clone();
        options.tool_turn_id = Some(Arc::new(move || {
            state
                .lock()
                .active
                .as_ref()
                .map(|active| active.id)
                .or_else(|| previous.as_ref().and_then(|callback| callback()))
        }));
        let state = self.state.clone();
        let previous = options.steer_signal.clone();
        options.steer_signal = Some(Arc::new(move || {
            let mut state = state.lock();
            if let Some(active) = state.active.as_mut() {
                if active.steer_controller.signal().aborted() {
                    active.steer_controller = Arc::new(AbortController::new());
                }
                return Some(active.steer_controller.signal());
            }
            drop(state);
            previous.as_ref().and_then(|callback| callback())
        }));
        let state = self.state.clone();
        let previous = options.source.clone();
        options.source = Some(Arc::new(move || {
            let active = state.lock();
            if let Some(active) = &active.active {
                return Some(AgentLLMRequestSource::Turn {
                    turn_id: active.id,
                    step: Some(active.steps as u64),
                    log_fields: None,
                });
            }
            drop(active);
            previous.as_ref().and_then(|source| source())
        }));
        options
    }

    fn machine_engine(&self) -> Arc<MachineEngine> {
        self.state
            .lock()
            .engine
            .clone()
            .expect("Machine engine not attached")
    }

    pub fn dispose(&self) {
        let (waiters, active, engine) = {
            let mut state = self.state.lock();
            if state.disposing {
                return;
            }
            state.disposing = true;
            let waiters = state
                .waiters
                .drain(..)
                .map(|(_, waiter)| waiter)
                .collect::<Vec<_>>();
            for waiter in &waiters {
                state
                    .terminal
                    .insert(waiter.id.clone(), PromptState::Cancelled);
            }
            state.pending.clear();
            state.steered.clear();
            let active = state.active.take();
            let engine = state.engine.clone();
            (waiters, active, engine)
        };
        let reason: SharedError = Arc::new(AbortError::new("Agent loop disposed"));
        for waiter in waiters {
            settle_waiter_cancelled(&waiter);
        }
        if let Some(active) = active {
            active.controller.abort(Some(reason.clone()));
            active.turn.cancel(Some(reason.clone()));
            active.result.resolve(LoopRunResult::Cancelled {
                steps: active.steps,
                reason: Some(reason),
            });
        }
        if let Some(engine) = engine {
            engine.stop();
        }
        maybe_settle(&self.state);
    }

    fn drain_pending(&self) {
        drain_pending_to_machine(&self.state);
    }
}

#[async_trait]
impl IAgentLoopService for AgentLoopService {
    fn submit(&self, input: UserEntry, options: Option<LoopSubmitOptions>) -> LoopSubmitResult {
        if self.state.lock().disposing {
            std::panic::panic_any(AbortError::new("Agent loop disposed"));
        }
        let options = options.unwrap_or_default();
        let meta = input.meta.clone().unwrap_or_default();
        let id = meta.prompt_id.clone().unwrap_or_else(new_message_id);
        let origin = meta.origin.unwrap_or_default();
        let tracked = meta.tracked.unwrap_or(false);
        let created_at = meta.created_at.unwrap_or_else(|| {
            tracked
                .then(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_default()
        });
        let user_message_id = meta
            .user_message_id
            .unwrap_or_else(|| if tracked { id.clone() } else { String::new() });
        let entry = UserEntry {
            message: input.message,
            meta: Some(UserMeta {
                prompt_id: Some(id.clone()),
                origin: Some(origin),
                tracked: Some(tracked),
                created_at: Some(created_at),
                user_message_id: Some(user_message_id),
                ..meta
            }),
        };
        let waiter = Arc::new(PromptWaiter::new(
            id.clone(),
            entry.clone(),
            options.on_materialize,
        ));
        let engine = {
            let mut state = self.state.lock();
            state.terminal.remove(&id);
            state.waiters.insert(id.clone(), waiter.clone());
            if state.engine.is_none() {
                state.pending.push(entry.clone());
            }
            state.engine.clone()
        };
        if let Some(engine) = &engine {
            let submitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine.submit(entry.clone());
            }));
            if submitted.is_err() {
                waiter.launched.resolve(None);
                waiter.completion.resolve(PromptCompletion {
                    prompt_id: id.clone(),
                    result: None,
                    state: PromptTerminalState::Failed,
                });
                self.state
                    .lock()
                    .terminal
                    .insert(id.clone(), PromptState::Failed);
                return LoopSubmitResult { id };
            }
            if options.steer_if_active.unwrap_or(false) && self.state.lock().active.is_some() {
                engine.steer(id.clone());
            }
        }
        LoopSubmitResult { id }
    }

    async fn steer(&self, prompt_ids: &[String]) -> Result<(), WireAsyncError> {
        if self.state.lock().disposing {
            std::panic::panic_any(AbortError::new("Agent loop disposed"));
        }
        if prompt_ids.is_empty() {
            return Err(Box::new(Error2::new(
                REQUEST_INVALID,
                "prompt_ids must not be empty",
            )));
        }
        let active_id = self.state.lock().active.as_ref().map(|active| active.id);
        let Some(active_id) = active_id else {
            return Err(Box::new(Error2::new(
                PROMPT_NOT_FOUND,
                "no active prompt to steer into",
            )));
        };
        let unique = prompt_ids.iter().collect::<HashSet<_>>();
        let engine = self.machine_engine();
        let queued = engine
            .snapshot()
            .queue_ids
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();
        if unique.len() != prompt_ids.len()
            || !prompt_ids.iter().all(|prompt_id| queued.contains(prompt_id))
        {
            return Err(Box::new(Error2::new(
                PROMPT_NOT_FOUND,
                "one or more prompts are not pending",
            )));
        }
        if let Some(materializer) = &self.materializer {
            let entries = engine
                .snapshot()
                .queue
                .into_iter()
                .filter(|entry| {
                    entry
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.prompt_id.as_ref())
                        .is_some_and(|id| prompt_ids.contains(id))
                })
                .collect::<Vec<_>>();
            for entry in entries {
                materializer(entry).await?;
            }
        }
        let still_pending = engine
            .snapshot()
            .queue_ids
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();
        let active_changed = self
            .state
            .lock()
            .active
            .as_ref()
            .is_none_or(|active| active.id != active_id);
        if active_changed || !prompt_ids.iter().all(|id| still_pending.contains(id)) {
            return Err(Box::new(Error2::new(
                PROMPT_NOT_FOUND,
                "one or more prompts are no longer pending",
            )));
        }
        engine.steer(prompt_ids);
        Ok(())
    }

    fn cancel(&self, target: Option<LoopCancelTarget>, reason: Option<SharedError>) -> bool {
        let reason = reason.unwrap_or_else(|| Arc::new(user_cancellation_reason()));
        let target = target.unwrap_or_default();
        if let Some(prompt_id) = target.prompt_id {
            if self
                .state
                .lock()
                .active
                .as_ref()
                .is_some_and(|active| active.tracked && active.prompt_id == prompt_id)
            {
                return cancel_active_in_state(&self.state, None, reason);
            }
            let waiter = self.state.lock().waiters.get(&prompt_id).cloned();
            let Some(waiter) = waiter else {
                std::panic::panic_any(Error2::new(
                    PROMPT_NOT_FOUND,
                    format!("prompt {prompt_id} not found"),
                ));
            };
            if self
                .state
                .lock()
                .active
                .as_ref()
                .is_some_and(|active| active.prompt_id == prompt_id)
            {
                return cancel_active_in_state(&self.state, None, reason);
            }
            let engine = self.state.lock().engine.clone();
            if let Some(engine) = engine {
                engine.cancel_queue_item(prompt_id.clone());
            }
            {
                let mut state = self.state.lock();
                state
                    .terminal
                    .insert(prompt_id.clone(), PromptState::Cancelled);
                state.steered.remove(&prompt_id);
                state.waiters.remove(&prompt_id);
            }
            settle_waiter_cancelled(&waiter);
            maybe_settle(&self.state);
            return true;
        }
        cancel_active_in_state(&self.state, target.turn_id, reason)
    }

    fn snapshot(&self) -> LoopSnapshot {
        let (engine, parked, active, active_trace) = {
            let state = self.state.lock();
            (
                state.engine.clone(),
                state.pending.clone(),
                state.active.as_ref().map(|active| {
                    (
                        active.id,
                        active.prompt_id.clone(),
                        active.tracked,
                        active.started_at,
                    )
                }),
                state.active_trace.clone(),
            )
        };
        let engine_snapshot = engine.as_ref().map(|engine| engine.snapshot());
        let machine_queue = engine_snapshot
            .as_ref()
            .map(|snapshot| snapshot.queue.clone())
            .unwrap_or_default();
        let machine_ids = machine_queue
            .iter()
            .filter_map(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()))
            .collect::<HashSet<_>>();
        let mut queue = machine_queue;
        queue.extend(parked.into_iter().filter(|entry| {
            entry
                .meta
                .as_ref()
                .and_then(|meta| meta.prompt_id.as_ref())
                .is_none_or(|id| !machine_ids.contains(id))
        }));
        let turn = engine_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.turn.as_ref())
            .map(|turn| AgentActivityTurnSnapshot {
                turn_id: turn.turn_id,
                phase: match turn.phase {
                    MachineEnginePhase::Running => AgentActivityPhase::Running,
                    MachineEnginePhase::ToolCall => AgentActivityPhase::ToolCall,
                    MachineEnginePhase::Retrying => AgentActivityPhase::Retrying,
                },
                step: turn.step as u32,
                ending: engine_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.aborting),
                ending_reason: engine_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.aborting)
                    .then_some(AgentActivityEndingReason::Aborted),
                retry: turn.retry.clone(),
                active_tool_calls: turn.active_tool_calls.clone(),
                since: active.as_ref().map(|active| active.3),
            });
        LoopSnapshot {
            state: if active.is_some() {
                LoopState::Running
            } else {
                LoopState::Idle
            },
            active_turn_id: active.as_ref().map(|active| active.0),
            active_prompt_id: active.and_then(|active| active.2.then_some(active.1)),
            queue,
            notification_count: engine_snapshot
                .as_ref()
                .map(|snapshot| snapshot.notification_count)
                .unwrap_or(0),
            paused: engine_snapshot
                .as_ref()
                .map(|snapshot| snapshot.paused)
                .unwrap_or(false),
            has_pending_requests: has_pending_requests(&self.state),
            turn,
            active_trace_id: active_trace.and_then(|trace| trace.trace_id),
        }
    }

    async fn settled(&self) {
        let waiter = {
            let mut state = self.state.lock();
            if state.active.is_none() && !has_pending_requests_locked(&state) {
                None
            } else {
                let waiter = Deferred::new();
                state.settle_waiters.push(waiter.clone());
                Some(waiter)
            }
        };
        if let Some(waiter) = waiter {
            waiter.future().await;
        }
    }

    fn try_acquire_quiescence(&self) -> Option<DisposableRef> {
        if self.state.lock().disposing {
            std::panic::panic_any(AbortError::new("Agent loop disposed"));
        }
        let engine = {
            let mut state = self.state.lock();
            if state.quiescence_depth > 0
                || state.history_snapshot_depth > 0
                || state.active.is_some()
                || has_pending_requests_locked(&state)
            {
                return None;
            }
            state.quiescence_depth += 1;
            state.engine.clone()
        };
        if let Some(engine) = engine {
            engine.pause();
        }
        let state = self.state.clone();
        Some(to_disposable(move || {
            let (engine, drain) = {
                let mut state = state.lock();
                if state.quiescence_depth == 0 {
                    return;
                }
                state.quiescence_depth -= 1;
                let drain = state.quiescence_depth == 0 && !state.disposing;
                (state.engine.clone(), drain)
            };
            if drain {
                if let Some(engine) = engine {
                    engine.resume();
                }
                drain_pending_to_machine(&state);
                maybe_settle(&state);
            }
        }))
    }

    fn try_acquire_history_snapshot(&self) -> Option<DisposableRef> {
        if self.state.lock().disposing {
            std::panic::panic_any(AbortError::new("Agent loop disposed"));
        }
        {
            let mut state = self.state.lock();
            if state.quiescence_depth > 0 || state.history_snapshot_depth > 0 {
                return None;
            }
            state.history_snapshot_depth += 1;
        }
        let state = self.state.clone();
        Some(to_disposable(move || {
            let mut state = state.lock();
            state.history_snapshot_depth = state.history_snapshot_depth.saturating_sub(1);
        }))
    }

    fn notify(&self, note: Option<LoopNotify>) -> LoopNotifyHandle {
        let note = note.unwrap_or_default();
        if self.state.lock().disposing {
            std::panic::panic_any(AbortError::new("Agent loop disposed"));
        }
        let message = note.message.as_ref().map(|message| UserMessage {
            role: Role::User,
            content: message.content.clone(),
        });
        let nudge = Arc::new(Mutex::new(Nudge {
            entry: UserEntry {
                message: message.unwrap_or(EMPTY_MACHINE_PROMPT),
                meta: Some(UserMeta::default()),
            },
            bypass_max_steps: note.bypass_max_steps.unwrap_or(false),
            turn_scoped: note.turn_scoped.unwrap_or(true),
            sent: false,
            consumed: false,
            dropped: false,
            on_consume: note.on_consume,
            on_drop: note.on_drop,
        }));
        let engine = {
            let mut state = self.state.lock();
            state.nudges.push(nudge.clone());
            let engine = if state.quiescence_depth == 0 {
                state.engine.clone()
            } else {
                None
            };
            if engine.is_some() {
                nudge.lock().sent = true;
            }
            engine
        };
        if let Some(engine) = engine {
            engine.notify(nudge.lock().entry.clone());
        }
        let dropped_nudge = nudge.clone();
        let state = self.state.clone();
        LoopNotifyHandle::new(Some(Arc::new(move || {
            drop_nudge(&dropped_nudge);
            maybe_settle(&state);
        })))
    }

    fn build_attach_bundle(&self) -> MachineEngineAttachBundle {
        machine_engine_attach_bundle(self.engine_options())
    }

    fn attach_engine(
        &self,
        reference: MachineEngineAttachRef,
        bundle: MachineEngineAttachBundle,
    ) -> Arc<MachineEngine> {
        if self.state.lock().engine.is_some() {
            panic!("Machine engine already attached");
        }
        let engine = attach_machine_engine(reference, bundle, self.engine_options());
        {
            let mut state = self.state.lock();
            for entry in engine.snapshot().queue {
                let Some(id) = entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()) else {
                    continue;
                };
                if !state.waiters.contains_key(&id) {
                    state.terminal.remove(&id);
                    state.waiters.insert(
                        id.clone(),
                        Arc::new(PromptWaiter::new(id, entry, None)),
                    );
                }
            }
            state.engine = Some(engine.clone());
        }
        if self.state.lock().quiescence_depth > 0 {
            engine.pause();
        }
        if !self.state.lock().disposing {
            self.drain_pending();
            maybe_settle(&self.state);
        }
        engine
    }

    async fn reset_machine_engine(&self) -> Result<(), WireAsyncError> {
        let (engine, journal) = {
            let state = self.state.lock();
            if state.disposing {
                return Ok(());
            }
            if state.active.is_some() {
                return Err("Machine engine reset requires a quiescent loop".into());
            }
            let engine = state
                .engine
                .clone()
                .ok_or_else(|| -> WireAsyncError { "Machine engine not attached".into() })?;
            (
                engine,
                engine_journal(self.options.journal.clone(), state.next_turn_id),
            )
        };
        engine.reset_journal(journal).await
    }

    fn prompt_handle(&self, id: &str) -> Option<PromptHandle> {
        let (waiter, state, terminal) = {
            let mut state = self.state.lock();
            let waiter = state.waiters.get(id)?.clone();
            let prompt_state = prompt_state_of(&state, id);
            let terminal = state.terminal.contains_key(id);
            (waiter, prompt_state, terminal)
        };
        let handle = PromptHandle {
            id: id.to_string(),
            user_message_id: waiter
                .entry
                .meta
                .as_ref()
                .and_then(|meta| meta.user_message_id.clone())
                .unwrap_or_default(),
            created_at: waiter
                .entry
                .meta
                .as_ref()
                .and_then(|meta| meta.created_at.clone())
                .unwrap_or_default(),
            state,
            message: waiter.message(),
            launched: waiter.launched_future.clone(),
            completion: waiter.completion_future.clone(),
        };
        if terminal {
            self.state.lock().waiters.remove(id);
        }
        Some(handle)
    }

    fn register_loop_error_handler(
        &self,
        handler: Arc<dyn LoopErrorHandler>,
        options: Option<LoopErrorHandlerRegistrationOptions>,
    ) -> DisposableRef {
        let options = options.unwrap_or_default();
        assert!(
            options.before.is_none() || options.after.is_none(),
            "Loop error handler registration cannot specify both before and after"
        );
        let id = handler.id().to_string();
        let before = options.before.is_some();
        {
            let mut state = self.state.lock();
            state
                .error_handlers
                .retain(|registered| registered.id() != id);
            match options.before.or(options.after) {
                None => state.error_handlers.push(handler),
                Some(target) => {
                    let index = state
                        .error_handlers
                        .iter()
                        .position(|registered| registered.id() == target)
                        .unwrap_or_else(|| {
                            panic!("Loop error handler target \"{target}\" is not registered")
                        });
                    let insert_at = if before { index } else { index + 1 };
                    state.error_handlers.insert(insert_at, handler);
                }
            }
        }
        let state = self.state.clone();
        to_disposable(move || {
            state
                .lock()
                .error_handlers
                .retain(|registered| registered.id() != id);
        })
    }

    fn hooks(&self) -> LoopHooks {
        self.hooks.clone()
    }
}

fn project_machine_event(state: &Arc<Mutex<LoopServiceState>>, event: MachineEngineEvent) {
    match event {
        MachineEngineEvent::TurnStarted {
            machine_turn_id,
            queue_item_id,
            entry,
        } => {
            let (callbacks, bypass_max_steps) = {
                let mut state = state.lock();
                state.next_turn_id = state.next_turn_id.max(machine_turn_id + 1);
                if queue_item_id.is_none() {
                    consume_nudges_locked(&mut state)
                } else {
                    (Vec::new(), false)
                }
            };
            if state.lock().active.is_none() {
                begin_active_turn(
                    state,
                    machine_turn_id,
                    queue_item_id,
                    entry,
                    bypass_max_steps,
                );
            }
            for callback in callbacks {
                callback();
            }
        }
        MachineEngineEvent::StepStarted { step, .. } => {
            let mut state = state.lock();
            if let Some(active) = &mut state.active {
                active.steps = step as u32;
            }
        }
        MachineEngineEvent::StepCompleted { step, finish, .. } => {
            let mut state = state.lock();
            if let Some(active) = &mut state.active {
                active.steps = active.steps.max(step as u32);
                active.last_stop_reason = finish.and_then(|finish| finish.finish_reason);
            }
        }
        MachineEngineEvent::PromptSteered { queue_item_ids, .. } => {
            let mut state = state.lock();
            if let Some(active) = &state.active {
                active
                    .steer_controller
                    .abort(Some(Arc::new(AbortError::new("Steered by new input"))));
            }
            let active_turn = state.active.as_ref().map(|active| active.turn.clone());
            for id in queue_item_ids {
                if state.waiters.contains_key(&id) {
                    state.steered.insert(id.clone());
                    if let Some(turn) = &active_turn {
                        state
                            .waiters
                            .get(&id)
                            .expect("steered waiter exists")
                            .launched
                            .resolve(Some(turn.clone()));
                    }
                }
            }
        }
        MachineEngineEvent::PromptBlocked { queue_item_id, .. } => {
            if let Some(id) = queue_item_id {
                complete_blocked(state, &id);
            }
        }
        MachineEngineEvent::PromptGateFailed {
            queue_item_id,
            error,
            ..
        } => {
            if let Some(id) = queue_item_id {
                complete_failed(state, &id, MachineValueError(error));
            }
        }
        MachineEngineEvent::TurnSettled {
            outcome,
            error,
            produced: _,
        } => settle_active_turn(state, outcome, error),
        _ => {}
    }
}

fn begin_active_turn(
    state: &Arc<Mutex<LoopServiceState>>,
    turn_id: u64,
    queue_item_id: Option<String>,
    entry: Option<UserEntry>,
    bypass_max_steps: bool,
) {
    let weak = Arc::downgrade(state);
    let mut state = state.lock();
    if state.active.is_some() {
        return;
    }
    let prompt_id = queue_item_id.unwrap_or_else(new_message_id);
    let entry = entry.unwrap_or(UserEntry {
        message: EMPTY_MACHINE_PROMPT,
        meta: Some(UserMeta::default()),
    });
    let waiter = state.waiters.get(&prompt_id).cloned().unwrap_or_else(|| {
        let waiter = Arc::new(PromptWaiter::new(prompt_id.clone(), entry.clone(), None));
        state.waiters.insert(prompt_id.clone(), waiter.clone());
        waiter
    });
    if let Some(on_materialize) = &waiter.on_materialize {
        on_materialize();
    }
    let controller = Arc::new(AbortController::new());
    let steer_controller = Arc::new(AbortController::new());
    let result = Deferred::new();
    let result_future = result.future();
    let turn = Arc::new(Turn::new(
        Some(turn_id),
        Some(TurnState::Running),
        controller.signal(),
        async {}.boxed().shared(),
        result_future,
        Arc::new(move |reason| {
            let Some(state) = weak.upgrade() else {
                return true;
            };
            if !state
                .lock()
                .active
                .as_ref()
                .is_some_and(|active| active.id == turn_id)
            {
                return true;
            }
            cancel_active_in_state(
                &state,
                Some(turn_id),
                reason.unwrap_or_else(|| Arc::new(user_cancellation_reason())),
            )
        }),
    ));
    let tracked = waiter
        .entry
        .meta
        .as_ref()
        .and_then(|meta| meta.tracked)
        .unwrap_or(false);
    let user_message_id = waiter
        .entry
        .meta
        .as_ref()
        .and_then(|meta| meta.user_message_id.clone())
        .unwrap_or_default();
    let created_at = waiter
        .entry
        .meta
        .as_ref()
        .and_then(|meta| meta.created_at.clone())
        .unwrap_or_default();
    let message = waiter.message();
    let active = ActiveTurn {
        id: turn_id,
        prompt_id: prompt_id.clone(),
        tracked,
        message: message.clone(),
        user_message_id,
        created_at,
        controller,
        steer_controller,
        result,
        turn: turn.clone(),
        steps: 0,
        started_at: now_millis(),
        last_stop_reason: None,
        bypass_max_steps,
    };
    state.active = Some(active);
    waiter.launched.resolve(Some(turn));
}

fn settle_active_turn(
    state: &Arc<Mutex<LoopServiceState>>,
    outcome: MachineTurnOutcome,
    error: Option<Value>,
) {
    let drops = {
        let mut state = state.lock();
        let Some(active) = state.active.take() else {
            return;
        };
        let result = match outcome {
            MachineTurnOutcome::Done => LoopRunResult::Completed(CompletedTurnResult {
                steps: active.steps,
                truncated: active.last_stop_reason == Some(FinishReason::Truncated),
                stop_reason: None,
            }),
            MachineTurnOutcome::Failed => LoopRunResult::Failed {
                steps: active.steps,
                error: Arc::new(MachineValueError(
                    error.unwrap_or_else(|| Value::String("Turn failed".to_string())),
                )),
            },
            MachineTurnOutcome::Aborted => LoopRunResult::Cancelled {
                steps: active.steps,
                reason: Some(
                    active
                        .controller
                        .signal()
                        .reason()
                        .unwrap_or_else(|| Arc::new(AbortError::new("Turn aborted"))),
                ),
            },
        };
        let terminal = match &result {
            LoopRunResult::Completed(_) => PromptState::Completed,
            LoopRunResult::Failed { .. } => PromptState::Failed,
            LoopRunResult::Cancelled { .. } => PromptState::Cancelled,
        };
        let completion_state = match &result {
            LoopRunResult::Completed(_) => PromptTerminalState::Completed,
            LoopRunResult::Failed { .. } => PromptTerminalState::Failed,
            LoopRunResult::Cancelled { .. } => PromptTerminalState::Cancelled,
        };
        active.result.resolve(result.clone());
        if let Some(waiter) = state.waiters.remove(&active.prompt_id) {
            waiter.completion.resolve(PromptCompletion {
                prompt_id: active.prompt_id.clone(),
                result: Some(result.clone()),
                state: completion_state,
            });
            state.terminal.insert(active.prompt_id.clone(), terminal);
        }
        let steered = std::mem::take(&mut state.steered);
        for id in steered {
            if let Some(waiter) = state.waiters.remove(&id) {
                waiter.completion.resolve(PromptCompletion {
                    prompt_id: id.clone(),
                    result: Some(result.clone()),
                    state: completion_state,
                });
                state.terminal.insert(id, terminal);
            }
        }
        let drops = state
            .nudges
            .iter()
            .filter(|nudge| {
                let nudge = nudge.lock();
                nudge.turn_scoped && !nudge.consumed && !nudge.dropped
            })
            .cloned()
            .collect::<Vec<_>>();
        drops
    };
    for nudge in drops {
        drop_nudge(&nudge);
    }
    maybe_settle(state);
}

fn complete_blocked(state: &Arc<Mutex<LoopServiceState>>, id: &str) {
    let waiter = {
        let mut state = state.lock();
        state.terminal.insert(id.to_string(), PromptState::Blocked);
        state.waiters.remove(id)
    };
    if let Some(waiter) = waiter {
        waiter.launched.resolve(None);
        waiter.completion.resolve(PromptCompletion {
            prompt_id: id.to_string(),
            result: None,
            state: PromptTerminalState::Blocked,
        });
    }
    maybe_settle(state);
}

fn complete_failed(
    state: &Arc<Mutex<LoopServiceState>>,
    id: &str,
    _error: MachineValueError,
) {
    let waiter = {
        let mut state = state.lock();
        state.terminal.insert(id.to_string(), PromptState::Failed);
        state.waiters.remove(id)
    };
    if let Some(waiter) = waiter {
        waiter.launched.resolve(None);
        waiter.completion.resolve(PromptCompletion {
            prompt_id: id.to_string(),
            result: None,
            state: PromptTerminalState::Failed,
        });
    }
    maybe_settle(state);
}

fn cancel_active_in_state(
    state: &Arc<Mutex<LoopServiceState>>,
    turn_id: Option<u64>,
    reason: SharedError,
) -> bool {
    let (controller, engine, value) = {
        let state = state.lock();
        let Some(active) = &state.active else {
            return false;
        };
        if turn_id.is_some_and(|turn_id| turn_id != active.id) {
            return false;
        }
        (
            active.controller.clone(),
            state.engine.clone(),
            Value::String(reason.to_string()),
        )
    };
    controller.abort(Some(reason));
    if let Some(engine) = engine {
        engine.abort(Some(value));
    }
    true
}

fn settle_waiter_cancelled(waiter: &PromptWaiter) {
    waiter.launched.resolve(None);
    waiter.completion.resolve(PromptCompletion {
        prompt_id: waiter.id.clone(),
        result: None,
        state: PromptTerminalState::Cancelled,
    });
}

fn prompt_state_of(state: &LoopServiceState, id: &str) -> PromptState {
    if state
        .active
        .as_ref()
        .is_some_and(|active| active.prompt_id == id)
    {
        return PromptState::Running;
    }
    if state.steered.contains(id) {
        return PromptState::Steered;
    }
    state
        .terminal
        .get(id)
        .copied()
        .unwrap_or(PromptState::Pending)
}

fn context_message_from_entry(entry: &UserEntry) -> ContextMessage {
    let mut message = ContextMessage::new(Role::User, entry.message.content.clone());
    message.id = entry
        .meta
        .as_ref()
        .and_then(|meta| meta.prompt_id.clone());
    message.origin = entry
        .meta
        .as_ref()
        .and_then(|meta| meta.origin.as_ref())
        .and_then(|origin| serde_json::to_value(origin).ok())
        .and_then(|origin| serde_json::from_value(origin).ok())
        .or_else(|| Some(PromptOrigin::User(user_prompt_origin())));
    message
}

fn drain_pending_to_machine(state: &Arc<Mutex<LoopServiceState>>) {
    let (engine, pending, notifications) = {
        let mut state = state.lock();
        let Some(engine) = state.engine.clone() else {
            return;
        };
        let queued = engine
            .snapshot()
            .queue_ids
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();
        let pending = std::mem::take(&mut state.pending);
        let pending = pending
            .into_iter()
            .filter(|entry| {
                entry
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.prompt_id.as_ref())
                    .is_some_and(|id| state.waiters.contains_key(id) && !queued.contains(id))
            })
            .collect::<Vec<_>>();
        let notifications = if state.quiescence_depth == 0 {
            state
                .nudges
                .iter()
                .filter_map(|nudge| {
                    let mut nudge = nudge.lock();
                    if nudge.dropped || nudge.consumed || nudge.sent {
                        return None;
                    }
                    nudge.sent = true;
                    Some(nudge.entry.clone())
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        (engine, pending, notifications)
    };
    for entry in pending {
        engine.submit(entry);
    }
    for entry in notifications {
        engine.notify(entry);
    }
}

fn has_pending_requests(state: &Arc<Mutex<LoopServiceState>>) -> bool {
    has_pending_requests_locked(&state.lock())
}

fn has_pending_requests_locked(state: &LoopServiceState) -> bool {
    !state.pending.is_empty()
        || state
            .engine
            .as_ref()
            .is_some_and(|engine| engine.snapshot().queue_length > 0)
        || state.nudges.iter().any(|nudge| {
            let nudge = nudge.lock();
            !nudge.dropped && !nudge.consumed
        })
}

fn maybe_settle(state: &Arc<Mutex<LoopServiceState>>) {
    let waiters = {
        let mut state = state.lock();
        if state.active.is_some() || has_pending_requests_locked(&state) {
            return;
        }
        std::mem::take(&mut state.settle_waiters)
    };
    for waiter in waiters {
        waiter.resolve(());
    }
}

fn consume_nudges_locked(
    state: &mut LoopServiceState,
) -> (Vec<Arc<dyn Fn() + Send + Sync>>, bool) {
    let mut callbacks = Vec::new();
    let mut bypass = false;
    for nudge in &state.nudges {
        let mut nudge = nudge.lock();
        if nudge.consumed || nudge.dropped || !nudge.sent {
            continue;
        }
        nudge.consumed = true;
        bypass |= nudge.bypass_max_steps;
        if let Some(callback) = nudge.on_consume.take() {
            callbacks.push(callback);
        }
    }
    (callbacks, bypass)
}

fn drop_nudge(nudge: &Arc<Mutex<Nudge>>) {
    let callback = {
        let mut nudge = nudge.lock();
        if nudge.dropped || nudge.consumed {
            return;
        }
        nudge.dropped = true;
        nudge.on_drop.take()
    };
    if let Some(callback) = callback {
        callback();
    }
}

fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0
}

#[derive(Debug)]
struct MachineValueError(Value);

impl fmt::Display for MachineValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Value::String(message) => formatter.write_str(message),
            value => write!(formatter, "{value}"),
        }
    }
}

impl std::error::Error for MachineValueError {}
