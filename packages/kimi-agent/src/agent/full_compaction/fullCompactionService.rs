use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::future::pending;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable, to_disposable};
use crate::_base::errors::errors::{Error2, Error2Options, SharedError};
use crate::_base::event::{Emitter, Event};
use crate::_base::utils::abort::{
    AbortController, AbortError, AbortSignal, abortable, is_abort_error,
};
use crate::agent::context_memory::compaction_handoff::is_real_user_input;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::open_tool_exchange::close_trailing_open_tool_exchange;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::llm_requester::llm_requester::IAgentLLMRequesterService;
use crate::agent::r#loop::r#loop::{
    IAgentLoopService, LoopErrorContext, LoopErrorHandler, LoopState,
};
use crate::agent::r#loop::turn_events::TurnStarted;
use crate::agent::r#loop::turn_ops::TurnEnded;
use crate::agent::mcp::mcp_events::{AgentErrorEvent, AgentErrorEventPayload};
use crate::agent::profile::profile::{IAgentProfileService, ProfileModelContext};
use crate::agent::scope_context::scope_context::{IAgentScopeContext, agent_context_of_scope};
use crate::agent::state::agent_state::IAgentStateService;
use crate::agent::task::task::IAgentTaskService;
use crate::agent::tool_select::tool_select::IAgentToolSelectService;
use crate::app::auth::errors::AUTH_LOGIN_REQUIRED;
use crate::app::event::event2::Event2;
use crate::app::event::event_bus::IEventBus;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::features::associativeMemory::service::AssociativeMemoryService;
use crate::features::todo::todoService::AgentTodoService;
use crate::human::llm::message::ToolDescription as Tool;
use crate::human::llm::usage::{TokenUsage, input_total};
use crate::llm_adapter::contract::errors::{
    CONTEXT_OVERFLOW_ERROR_CODE, PROVIDER_AUTH_ERROR_CODE,
};
use crate::llm_adapter::contract::message::{Message, create_user_message};
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::session::session_activity::session_run_gate::ISessionRunGate;
use crate::session::session_token_counting::session_token_counting::ISessionTokenCountingService;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use crate::wire::record::WireAsyncError;
use crate::wire::wire::IWireService;

use super::compaction_apply::{
    CompactionApplyDeps, CompactionApplyInput, apply_compaction_round, non_history_state_fingerprint,
};
use super::compaction_generation::{
    CompactionGenerationDeps, CompactionGenerationState, generate_compaction_summary,
};
use super::compaction_instruction::{CompactionInstructionInput, render_compaction_instruction};
use super::compaction_ops::{
    AgentIdPayload, CompactionBlocked, CompactionBlockedPayload, CompactionCancelled,
    CompactionCompleted, CompactionCompletedPayload, FullCompactionBegin, FullCompactionBeginPayload,
    FullCompactionCancel, FullCompactionComplete, full_compaction_key,
    full_compaction_wire_ranges_key,
};
use super::compaction_utils::{
    CompactionTruncatedError, HistoryTokenProfile, assistant_usage_total,
    build_history_token_profile, find_api_status_error, take_recent_messages_within_token_budget,
};
use super::full_compaction::{
    FullCompactionHooks, FullCompactionInput, FullCompactionTask, IAgentFullCompactionService,
};
use super::strategy::{CompactionStrategy, RuntimeCompactionStrategy};
use super::types::{CompactionBeginData, CompactionResult, CompactionSource};

pub const MAX_COMPACTION_RETRY_ATTEMPTS: u32 = 5;
const DEFAULT_COMPACTION_MAX_COMPLETION_TOKENS: u32 = 128 * 1024;
const OVERFLOW_CONTEXT_SAFETY_RATIO: f64 = 0.85;
const OVERFLOW_STATUS_RECOVERY_RATIO: f64 = 0.5;

fn transient_key(
    name: &str,
    initial: Arc<dyn Fn() -> Value + Send + Sync>,
) -> Arc<ReplayableStateKey> {
    Arc::new(ReplayableStateKey::new(
        name.to_string(),
        initial,
        ReplayableOptions {
            schema: json!({}),
            durable: Some(false),
            blobs: None,
            mutable_replay: None,
        },
    ))
}

pub static FULL_COMPACTION_COMPACTION_COUNT_IN_TURN_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        transient_key(
            "fullCompaction.compactionCountInTurn",
            Arc::new(|| json!(0)),
        )
    });
pub static FULL_COMPACTION_OBSERVED_MAX_CONTEXT_TOKENS_BY_MODEL_KEY: LazyLock<
    Arc<ReplayableStateKey>,
> = LazyLock::new(|| {
    transient_key(
        "fullCompaction.observedMaxContextTokensByModel",
        Arc::new(|| json!({})),
    )
});
pub static FULL_COMPACTION_LAST_COMPACTED_TOKEN_COUNT_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        transient_key(
            "fullCompaction.lastCompactedTokenCount",
            Arc::new(|| Value::Null),
        )
    });
pub static FULL_COMPACTION_CONSECUTIVE_OVERFLOW_COMPACTIONS_KEY: LazyLock<
    Arc<ReplayableStateKey>,
> = LazyLock::new(|| {
    transient_key(
        "fullCompaction.consecutiveOverflowCompactions",
        Arc::new(|| json!(0)),
    )
});
pub static FULL_COMPACTION_ACTIVE_TURN_ID_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        transient_key(
            "fullCompaction.activeTurnId",
            Arc::new(|| Value::Null),
        )
    });

static NEXT_ACTIVE_COMPACTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ActiveCompaction {
    id: u64,
    task: FullCompactionTask,
    origin_turn_id: Option<u64>,
    quiescence: Option<DisposableRef>,
    trace: Arc<Mutex<Option<LLMRequestTrace>>>,
    blocked_by_turn: Arc<AtomicBool>,
    resolution: Arc<Mutex<Option<oneshot::Sender<Result<CompactionResult, SharedError>>>>>,
    abort_listener: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ActiveCompaction {
    fn trace_id(&self) -> Option<String> {
        self.trace
            .lock()
            .as_ref()
            .and_then(|trace| trace.trace_id.clone())
    }

    fn public_task(&self) -> FullCompactionTask {
        let mut task = self.task.clone();
        task.trace_id = self.trace_id();
        task
    }

    fn settle(&self, result: Result<CompactionResult, SharedError>) {
        if let Some(sender) = self.resolution.lock().take() {
            let _ = sender.send(result);
        }
    }

    fn finish(&self) {
        if let Some(listener) = self.abort_listener.lock().take() {
            listener.abort();
        }
        if let Some(quiescence) = &self.quiescence {
            quiescence.dispose();
        }
    }
}

pub struct AgentFullCompactionService {
    context: Arc<dyn IAgentContextMemoryService>,
    token_counting: Arc<dyn ISessionTokenCountingService>,
    llm_requester: Arc<dyn IAgentLLMRequesterService>,
    profile: Arc<dyn IAgentProfileService>,
    tool_select: Arc<dyn IAgentToolSelectService>,
    agent: Arc<dyn IAgentScopeContext>,
    todo: Arc<AgentTodoService>,
    task: Arc<dyn IAgentTaskService>,
    telemetry: Arc<dyn ITelemetryService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    loop_service: Arc<dyn IAgentLoopService>,
    states: Arc<dyn IAgentStateService>,
    wire: Arc<dyn IWireService>,
    run_gate: Arc<dyn ISessionRunGate>,
    associative_memory: Arc<AssociativeMemoryService>,
    hooks: FullCompactionHooks,
    on_did_finish_compaction: Emitter<FullCompactionTask>,
    strategy: RuntimeCompactionStrategy,
    compacting: Mutex<Option<ActiveCompaction>>,
    dispatch_tail: Mutex<Shared<BoxFuture<'static, ()>>>,
    registrations: DisposableStore,
    this: Weak<AgentFullCompactionService>,
}

impl AgentFullCompactionService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        context: Arc<dyn IAgentContextMemoryService>,
        token_counting: Arc<dyn ISessionTokenCountingService>,
        llm_requester: Arc<dyn IAgentLLMRequesterService>,
        profile: Arc<dyn IAgentProfileService>,
        tool_select: Arc<dyn IAgentToolSelectService>,
        agent: Arc<dyn IAgentScopeContext>,
        todo: Arc<AgentTodoService>,
        task: Arc<dyn IAgentTaskService>,
        telemetry: Arc<dyn ITelemetryService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        event_bus: Arc<dyn IEventBus>,
        loop_service: Arc<dyn IAgentLoopService>,
        states: Arc<dyn IAgentStateService>,
        wire: Arc<dyn IWireService>,
        run_gate: Arc<dyn ISessionRunGate>,
        associative_memory: Arc<AssociativeMemoryService>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|this: &Weak<Self>| {
            let registrations = DisposableStore::new();
            for key in [
                full_compaction_key(),
                full_compaction_wire_ranges_key(),
                FULL_COMPACTION_COMPACTION_COUNT_IN_TURN_KEY.clone(),
                FULL_COMPACTION_OBSERVED_MAX_CONTEXT_TOKENS_BY_MODEL_KEY.clone(),
                FULL_COMPACTION_LAST_COMPACTED_TOKEN_COUNT_KEY.clone(),
                FULL_COMPACTION_CONSECUTIVE_OVERFLOW_COMPACTIONS_KEY.clone(),
                FULL_COMPACTION_ACTIVE_TURN_ID_KEY.clone(),
            ] {
                registrations.add(
                    states
                        .contribute_state(key.state_key())
                        .unwrap_or_else(|error| panic!("{error}")),
                );
            }
            let strategy_context = this.clone();
            let strategy = RuntimeCompactionStrategy::new(
                Arc::new(move || {
                    strategy_context
                        .upgrade()
                        .expect("full compaction service is alive")
                        .resolve_model_context_with_effective_max()
                }),
                None,
            );
            let restore = this.clone();
            let restore_hook = dispatcher
                .hooks()
                .on_did_restore
                .register(
                    "full-compaction",
                    Arc::new(move |_ctx, next| {
                        let restore = restore.clone();
                        Box::pin(async move {
                            if let Some(this) = restore.upgrade() {
                                this.normalize_after_replay();
                            }
                            next.proceed(None).await;
                        })
                    }),
                    Default::default(),
                )
                .expect("full compaction restore hook can be registered");
            registrations.add(to_disposable(move || restore_hook.dispose()));

            let started = this.clone();
            let subscription = event_bus.subscribe_class(
                TurnStarted::class(),
                Arc::new(move |_event| {
                    if let Some(this) = started.upgrade() {
                        this.reset_for_turn();
                    }
                }),
            );
            registrations.add(to_disposable(move || subscription.dispose()));
            let ended = this.clone();
            let subscription = event_bus.subscribe_class(
                TurnEnded::class(),
                Arc::new(move |_event| {
                    if let Some(this) = ended.upgrade() {
                        this.set_active_turn_id(None);
                    }
                }),
            );
            registrations.add(to_disposable(move || subscription.dispose()));

            let before = this.clone();
            let before_hook = loop_service
                .hooks()
                .on_will_begin_step
                .register(
                    "full-compaction",
                    Arc::new(move |ctx, next| {
                        let before = before.clone();
                        Box::pin(async move {
                            if let Some(this) = before.upgrade() {
                                this.before_step(ctx.signal.clone(), Some(ctx.turn_id))
                                    .await
                                    .unwrap_or_else(|error| std::panic::panic_any(error));
                            }
                            next.proceed(None).await;
                        })
                    }),
                    Default::default(),
                )
                .expect("full compaction before-step hook can be registered");
            registrations.add(to_disposable(move || before_hook.dispose()));

            let after = this.clone();
            let after_hook = loop_service
                .hooks()
                .on_did_finish_step
                .register(
                    "full-compaction",
                    Arc::new(move |_ctx, next| {
                        let after = after.clone();
                        Box::pin(async move {
                            if let Some(this) = after.upgrade() {
                                this.after_step()
                                    .await
                                    .unwrap_or_else(|error| std::panic::panic_any(error));
                            }
                            next.proceed(None).await;
                        })
                    }),
                    Default::default(),
                )
                .expect("full compaction after-step hook can be registered");
            registrations.add(to_disposable(move || after_hook.dispose()));
            registrations.add(loop_service.register_loop_error_handler(
                Arc::new(FullCompactionLoopErrorHandler { this: this.clone() }),
                None,
            ));

            Self {
                context,
                token_counting,
                llm_requester,
                profile,
                tool_select,
                agent,
                todo,
                task,
                telemetry,
                dispatcher,
                loop_service,
                states,
                wire,
                run_gate,
                associative_memory,
                hooks: FullCompactionHooks::default(),
                on_did_finish_compaction: Emitter::new(),
                strategy,
                compacting: Mutex::new(None),
                dispatch_tail: Mutex::new(async {}.boxed().shared()),
                registrations,
                this: this.clone(),
            }
        })
    }

    fn this(&self) -> Arc<Self> {
        self.this.upgrade().expect("full compaction service is alive")
    }

    fn dispatch(&self, event: impl Into<Event2>) {
        let dispatcher = self.dispatcher.clone();
        let event = event.into();
        let previous = self.dispatch_tail.lock().clone();
        let run = async move {
            previous.await;
            let _ = dispatcher.dispatch(event).await;
        }
        .boxed()
        .shared();
        *self.dispatch_tail.lock() = run.clone();
        tokio::spawn(run);
    }

    fn state_u32(&self, key: &Arc<ReplayableStateKey>) -> u32 {
        self.states.get(&key.state_key()).as_u64().unwrap_or(0) as u32
    }

    fn set_state_u32(&self, key: &Arc<ReplayableStateKey>, value: u32) {
        self.states.set(&key.state_key(), json!(value));
    }

    fn compaction_count_in_turn(&self) -> u32 {
        self.state_u32(&FULL_COMPACTION_COMPACTION_COUNT_IN_TURN_KEY)
    }

    fn set_compaction_count_in_turn(&self, value: u32) {
        self.set_state_u32(&FULL_COMPACTION_COMPACTION_COUNT_IN_TURN_KEY, value);
    }

    fn observed_max_context_tokens_by_model(&self) -> HashMap<String, u32> {
        self.states
            .get(&FULL_COMPACTION_OBSERVED_MAX_CONTEXT_TOKENS_BY_MODEL_KEY.state_key())
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(model, tokens)| Some((model.clone(), tokens.as_u64()? as u32)))
            .collect()
    }

    fn last_compacted_token_count(&self) -> Option<u64> {
        self.states
            .get(&FULL_COMPACTION_LAST_COMPACTED_TOKEN_COUNT_KEY.state_key())
            .as_u64()
    }

    fn set_last_compacted_token_count(&self, value: Option<u64>) {
        self.states.set(
            &FULL_COMPACTION_LAST_COMPACTED_TOKEN_COUNT_KEY.state_key(),
            value.map_or(Value::Null, |value| json!(value)),
        );
    }

    fn consecutive_overflow_compactions(&self) -> u32 {
        self.state_u32(&FULL_COMPACTION_CONSECUTIVE_OVERFLOW_COMPACTIONS_KEY)
    }

    fn set_consecutive_overflow_compactions(&self, value: u32) {
        self.set_state_u32(&FULL_COMPACTION_CONSECUTIVE_OVERFLOW_COMPACTIONS_KEY, value);
    }

    fn active_turn_id(&self) -> Option<u64> {
        self.states
            .get(&FULL_COMPACTION_ACTIVE_TURN_ID_KEY.state_key())
            .as_u64()
    }

    fn set_active_turn_id(&self, value: Option<u64>) {
        self.states.set(
            &FULL_COMPACTION_ACTIVE_TURN_ID_KEY.state_key(),
            value.map_or(Value::Null, |value| json!(value)),
        );
    }

    fn active(&self) -> Option<ActiveCompaction> {
        self.compacting.lock().clone()
    }

    fn is_active(&self, active: &ActiveCompaction) -> bool {
        self.compacting
            .lock()
            .as_ref()
            .is_some_and(|current| current.id == active.id)
    }

    pub fn dispose(&self) {
        if let Some(active) = self.active() {
            self.cancel_active(&active);
        }
        self.registrations.dispose();
        self.on_did_finish_compaction.dispose();
    }

    fn get_effective_max_context_tokens(&self) -> u32 {
        let capability = &self.profile.data().base.model_capabilities;
        let configured = capability
            .max_input_tokens
            .unwrap_or(capability.max_context_tokens);
        let observed = self
            .profile
            .data()
            .base
            .model_alias
            .and_then(|alias| self.observed_max_context_tokens_by_model().get(&alias).copied());
        match observed {
            None => configured,
            Some(observed) if configured == 0 => observed,
            Some(observed) => configured.min(observed),
        }
    }

    fn resolve_model_context_with_effective_max(&self) -> ProfileModelContext {
        let mut resolved = self.profile.resolve_model_context();
        let effective_max = self.get_effective_max_context_tokens();
        resolved.model_capabilities.max_context_tokens = effective_max;
        resolved.model_capabilities.max_input_tokens = Some(effective_max);
        resolved
    }

    fn current_request_tokens(&self) -> u64 {
        self.history_request_tokens(&self.context.get())
    }

    fn history_request_tokens(&self, messages: &[ContextMessage]) -> u64 {
        let estimated = self.request_tokens_for_context(messages);
        match self.usage_anchored_request_tokens(messages) {
            Some(anchored) => estimated.max(anchored),
            None => estimated,
        }
    }

    fn usage_anchored_request_tokens(&self, messages: &[ContextMessage]) -> Option<u64> {
        for index in (0..messages.len()).rev() {
            let Some(usage_tokens) = assistant_usage_total(&messages[index]) else {
                continue;
            };
            let tail = messages[index + 1..]
                .iter()
                .cloned()
                .map(Into::into)
                .collect::<Vec<Message>>();
            return Some(
                usage_tokens
                    .saturating_add(self.token_counting.estimate_messages(&tail, None) as u64),
            );
        }
        None
    }

    fn request_tokens_for_context(&self, messages: &[ContextMessage]) -> u64 {
        let messages = messages
            .iter()
            .cloned()
            .map(Into::into)
            .collect::<Vec<Message>>();
        self.request_tokens(&messages)
    }

    fn request_tokens(&self, messages: &[Message]) -> u64 {
        self.token_counting.request_size(
            &crate::agent::token_counting::token_counting::TokenCountingRequest {
                system_prompt: self.profile.get_system_prompt(),
                tools: self.default_tools(),
                messages: messages.to_vec(),
            },
            None,
        ) as u64
    }

    fn default_tools(&self) -> Vec<Tool> {
        self.tool_select
            .default_tools(None)
            .into_iter()
            .filter(|tool| tool.deferred.is_none())
            .map(|tool| Tool {
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
                deferred: tool.deferred,
            })
            .collect()
    }

    fn should_recover_from_context_overflow(
        &self,
        error: &(dyn Error + Send + Sync + 'static),
        estimated_request_tokens: u64,
    ) -> bool {
        if error_code(error) == Some(CONTEXT_OVERFLOW_ERROR_CODE) {
            return true;
        }
        let status_error = find_api_status_error(error);
        if status_error.is_some_and(|status| {
            status.base.code == crate::llm_adapter::contract::errors::ProviderErrorCode::ContextOverflow
        }) {
            return true;
        }
        let Some(status_error) = status_error else {
            return false;
        };
        if status_error.status_code != Some(413) {
            return false;
        }
        let effective_max = self.get_effective_max_context_tokens();
        effective_max > 0
            && estimated_request_tokens as f64
                >= effective_max as f64 * OVERFLOW_STATUS_RECOVERY_RATIO
    }

    fn observe_context_overflow(&self, estimated_request_tokens: u64) {
        if estimated_request_tokens == 0 {
            return;
        }
        let Some(model_alias) = self.profile.data().base.model_alias else {
            return;
        };
        let observed = ((estimated_request_tokens as f64 * OVERFLOW_CONTEXT_SAFETY_RATIO).floor()
            as u64)
            .max(1) as u32;
        let current = self.get_effective_max_context_tokens();
        if current > 0 && observed >= current {
            return;
        }
        let mut observed_by_model = self.observed_max_context_tokens_by_model();
        observed_by_model.insert(model_alias, observed);
        self.states.set(
            &FULL_COMPACTION_OBSERVED_MAX_CONTEXT_TOKENS_BY_MODEL_KEY.state_key(),
            json!(observed_by_model),
        );
    }

    pub fn begin(&self, input: FullCompactionInput) -> Result<bool, SharedError> {
        if self.active().is_some() {
            return Ok(false);
        }
        let data = CompactionBeginData {
            instruction: input.instruction,
            source: input.source,
        };
        if data.source == CompactionSource::Manual {
            self.run_gate.begin().map_err(shared_error)?;
        }
        if !self.reserve_compaction_slot(data.source) {
            return Ok(false);
        }
        let token_count = self.validate_compaction_start(data.source)?;
        let quiescence = (data.source == CompactionSource::Manual)
            .then(|| self.loop_service.try_acquire_quiescence())
            .flatten();
        if data.source == CompactionSource::Manual && quiescence.is_none() {
            return Err(error2(
                super::errors::COMPACTION_UNABLE,
                "Cannot compact while a turn is active or another context change is running. Wait for it to finish, then retry.",
                None,
                None,
            ));
        }
        self.dispatch(FullCompactionBegin::new(FullCompactionBeginPayload::new(
            self.agent.agent_id(),
            data.clone(),
        )));
        let origin_turn_id = (data.source == CompactionSource::Auto)
            .then(|| self.active_turn_id())
            .flatten();
        let active = self.create_active_compaction(
            data.source,
            token_count,
            origin_turn_id,
            quiescence,
        );
        *self.compacting.lock() = Some(active.clone());
        self.install_abort_listener(&active);
        let this = self.this();
        let worker_active = active.clone();
        tokio::spawn(async move {
            let result = this.compaction_worker(worker_active.clone(), data).await;
            worker_active.settle(result);
        });
        Ok(true)
    }

    fn reserve_compaction_slot(&self, source: CompactionSource) -> bool {
        match source {
            CompactionSource::Manual => self.set_compaction_count_in_turn(0),
            CompactionSource::Auto => {
                self.set_compaction_count_in_turn(self.compaction_count_in_turn() + 1)
            }
        }
        self.compaction_count_in_turn() <= self.strategy.max_compaction_per_turn()
    }

    fn validate_compaction_start(&self, source: CompactionSource) -> Result<u64, SharedError> {
        let history = self.context.get();
        if history.is_empty() {
            return Err(error2(
                super::errors::COMPACTION_UNABLE,
                "No messages to compact in current history.",
                None,
                None,
            ));
        }
        if source == CompactionSource::Manual && self.loop_service.snapshot().state != LoopState::Idle {
            return Err(error2(
                super::errors::COMPACTION_UNABLE,
                "Cannot compact while a turn is active. Wait for it to finish, then retry.",
                None,
                None,
            ));
        }
        Ok(self.history_request_tokens(&history))
    }

    fn create_active_compaction(
        &self,
        trigger: CompactionSource,
        token_count: u64,
        origin_turn_id: Option<u64>,
        quiescence: Option<DisposableRef>,
    ) -> ActiveCompaction {
        let abort_controller = Arc::new(AbortController::new());
        let (sender, receiver) = oneshot::channel();
        let promise = async move {
            match receiver.await {
                Ok(result) => result,
                Err(_) => Err(Arc::new(AbortError::new("Compaction cancelled.")) as SharedError),
            }
        }
        .boxed()
        .shared();
        ActiveCompaction {
            id: NEXT_ACTIVE_COMPACTION_ID.fetch_add(1, Ordering::Relaxed),
            task: FullCompactionTask {
                abort_controller,
                promise,
                trigger,
                token_count,
                trace_id: None,
            },
            origin_turn_id,
            quiescence,
            trace: Arc::new(Mutex::new(None)),
            blocked_by_turn: Arc::new(AtomicBool::new(false)),
            resolution: Arc::new(Mutex::new(Some(sender))),
            abort_listener: Arc::new(Mutex::new(None)),
        }
    }

    fn install_abort_listener(&self, active: &ActiveCompaction) {
        let signal = active.task.abort_controller.signal();
        let this = self.this.clone();
        let active_for_task = active.clone();
        let listener = tokio::spawn(async move {
            let _ = abortable(pending::<()>(), signal).await;
            if let Some(this) = this.upgrade() {
                this.cancel_active(&active_for_task);
            }
        });
        *active.abort_listener.lock() = Some(listener);
    }

    fn cancel_active(&self, active: &ActiveCompaction) -> bool {
        if !self.is_active(active) {
            return false;
        }
        self.dispatch(FullCompactionCancel::new(AgentIdPayload {
            agent_id: self.agent.agent_id(),
        }));
        *self.compacting.lock() = None;
        if !active.task.abort_controller.signal().aborted() {
            active.task.abort_controller.abort(None);
        }
        self.dispatch(CompactionCancelled::new(AgentIdPayload {
            agent_id: self.agent.agent_id(),
        }));
        true
    }

    fn mark_completed(&self, active: &ActiveCompaction) -> bool {
        if !self.is_active(active) {
            return false;
        }
        self.dispatch(FullCompactionComplete::new(AgentIdPayload {
            agent_id: self.agent.agent_id(),
        }));
        *self.compacting.lock() = None;
        true
    }

    fn normalize_after_replay(&self) {
        if self
            .states
            .get(&full_compaction_key().state_key())
            .get("phase")
            .and_then(Value::as_str)
            != Some("running")
        {
            return;
        }
        self.dispatch(FullCompactionCancel::new(AgentIdPayload {
            agent_id: self.agent.agent_id(),
        }));
    }

    fn reset_for_turn(&self) {
        self.set_compaction_count_in_turn(0);
        self.set_last_compacted_token_count(None);
        self.set_consecutive_overflow_compactions(0);
    }

    async fn recover_from_context_overflow(
        &self,
        context: LoopErrorContext,
    ) -> Result<bool, SharedError> {
        self.record_overflow_recovery(context.error.clone())?;
        let did_start_compaction = self.begin_auto_compaction(true)?;
        if !did_start_compaction && self.active().is_none() {
            return Ok(false);
        }
        self.block(Some(context.signal.clone()), Some(context.turn_id))
            .await?;
        Ok(self.retry_failed_driver(&context))
    }

    fn record_overflow_recovery(&self, error: SharedError) -> Result<(), SharedError> {
        self.observe_context_overflow(self.current_request_tokens());
        self.set_consecutive_overflow_compactions(self.consecutive_overflow_compactions() + 1);
        let max_attempts = self.strategy.max_overflow_compaction_attempts();
        if self.consecutive_overflow_compactions() <= max_attempts {
            return Ok(());
        }
        Err(error2(
            CONTEXT_OVERFLOW_ERROR_CODE,
            format!(
                "Compaction failed to bring the context under the model window after {max_attempts} attempts."
            ),
            None,
            Some(error),
        ))
    }

    fn retry_failed_driver(&self, context: &LoopErrorContext) -> bool {
        if context.signal.aborted() {
            return false;
        }
        (context.retry)();
        true
    }

    async fn before_step(
        &self,
        signal: AbortSignal,
        turn_id: Option<u64>,
    ) -> Result<(), SharedError> {
        self.set_active_turn_id(turn_id);
        self.check_auto_compaction(true)?;
        if self
            .strategy
            .should_block(self.token_count_with_pending() as u32)
        {
            self.block(Some(signal), turn_id).await?;
        }
        Ok(())
    }

    async fn after_step(&self) -> Result<(), SharedError> {
        self.set_consecutive_overflow_compactions(0);
        if self.strategy.check_after_step() {
            self.check_auto_compaction(false)?;
        }
        Ok(())
    }

    fn check_auto_compaction(&self, throw_on_limit: bool) -> Result<bool, SharedError> {
        if self.active().is_some() {
            return Ok(true);
        }
        if self.last_compacted_token_count().is_some_and(|last| {
            self.token_count_with_pending() <= last
        }) {
            return Ok(false);
        }
        if !self
            .strategy
            .should_compact(self.token_count_with_pending() as u32)
        {
            return Ok(false);
        }
        self.begin_auto_compaction(throw_on_limit)
    }

    fn begin_auto_compaction(&self, throw_on_limit: bool) -> Result<bool, SharedError> {
        if self.active().is_some() {
            return Ok(true);
        }
        let max_compactions = self.strategy.max_compaction_per_turn();
        if self.compaction_count_in_turn() >= max_compactions {
            if throw_on_limit {
                return Err(error2(
                    CONTEXT_OVERFLOW_ERROR_CODE,
                    format!("Compaction limit exceeded ({max_compactions})"),
                    Some(HashMap::from([(
                        "maxCompactions".to_string(),
                        json!(max_compactions),
                    )])),
                    None,
                ));
            }
            return Ok(false);
        }
        self.begin(FullCompactionInput {
            source: CompactionSource::Auto,
            instruction: None,
        })
    }

    async fn block(
        &self,
        signal: Option<AbortSignal>,
        turn_id: Option<u64>,
    ) -> Result<(), SharedError> {
        let Some(active) = self.active() else {
            return Ok(());
        };
        active.blocked_by_turn.store(true, Ordering::SeqCst);
        let abort_listener = self.propagate_blocking_abort(&active, signal.clone());
        self.dispatch(CompactionBlocked::new(CompactionBlockedPayload {
            agent_id: self.agent.agent_id(),
            turn_id,
        }));
        let result = active.task.promise.clone().await;
        if let Some(listener) = abort_listener {
            listener.abort();
        }
        match result {
            Ok(_) => Ok(()),
            Err(error) if self.was_blocking_wait_aborted(&active, signal.as_ref(), error.as_ref()) => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn propagate_blocking_abort(
        &self,
        active: &ActiveCompaction,
        signal: Option<AbortSignal>,
    ) -> Option<JoinHandle<()>> {
        let signal = signal?;
        let this = self.this.clone();
        let active = active.clone();
        Some(tokio::spawn(async move {
            let _ = abortable(pending::<()>(), signal).await;
            if let Some(this) = this.upgrade()
                && this.is_active(&active)
            {
                active.task.abort_controller.abort(None);
            }
        }))
    }

    fn was_blocking_wait_aborted(
        &self,
        active: &ActiveCompaction,
        signal: Option<&AbortSignal>,
        error: &(dyn Error + Send + Sync + 'static),
    ) -> bool {
        signal.is_some_and(AbortSignal::aborted)
            && (active.task.abort_controller.signal().aborted() || is_compaction_abort(error))
    }

    async fn compaction_worker(
        &self,
        active: ActiveCompaction,
        data: CompactionBeginData,
    ) -> Result<CompactionResult, SharedError> {
        let outcome = match self.compaction_round(active.clone(), data.clone()).await {
            Ok(result) => {
                if !self.is_active(&active) {
                    Err(compaction_cancelled_reason(Some(&active)))
                } else {
                    self.set_last_compacted_token_count(Some(result.tokens_after));
                    if !self.mark_completed(&active) {
                        Err(compaction_cancelled_reason(Some(&active)))
                    } else {
                        self.dispatch(CompactionCompleted::new(CompactionCompletedPayload {
                            agent_id: self.agent.agent_id(),
                            result: result.clone(),
                        }));
                        Ok(result)
                    }
                }
            }
            Err(error) => {
                if active.task.abort_controller.signal().aborted()
                    || is_compaction_abort(error.as_ref())
                {
                    self.cancel_active(&active);
                    Err(error)
                } else {
                    let blocked_by_turn = self.is_active(&active)
                        && active.blocked_by_turn.load(Ordering::SeqCst);
                    if self.is_active(&active) {
                        self.cancel_active(&active);
                    }
                    if blocked_by_turn {
                        Err(error)
                    } else {
                        self.dispatch(AgentErrorEvent::new(AgentErrorEventPayload {
                            error: crate::_base::errors::serialize::to_kimi_error_payload(
                                &error
                                    .downcast_ref::<crate::_base::errors::errors::Error2>()
                                    .map(|coded| {
                                        crate::_base::errors::serialize::UnknownError::Error(
                                            Arc::new(coded.clone())
                                                as crate::_base::errors::errors::SharedError,
                                        )
                                    })
                                    .unwrap_or_else(|| {
                                        crate::_base::errors::serialize::UnknownError::String(
                                            error.to_string(),
                                        )
                                    }),
                            ),
                            agent_id: self.agent.agent_id(),
                        }));
                        Err(error)
                    }
                }
            }
        };
        self.on_did_finish_compaction.fire(active.public_task());
        active.finish();
        outcome
    }

    async fn compaction_round(
        &self,
        active: ActiveCompaction,
        data: CompactionBeginData,
    ) -> Result<CompactionResult, SharedError> {
        let started_at = Instant::now();
        let original_history = self.context.get();
        let token_counting = self.token_counting.clone();
        let apply_deps = CompactionApplyDeps {
            context: self.context.clone(),
            todo: self.todo.clone(),
            task: self.task.clone(),
            associative_memory: self.associative_memory.clone(),
            wire: self.wire.clone(),
            states: self.states.clone(),
            estimate_text: Arc::new(move |text| token_counting.estimate_text(text) as u64),
        };
        let non_history_state_before = non_history_state_fingerprint(&apply_deps);
        let tokens_before = self.history_request_tokens(&original_history);
        let mut generation_state = CompactionGenerationState::default();
        let mut thinking_effort = self.profile.data().base.thinking_level.clone();
        let signal = active.task.abort_controller.signal();

        let result = async {
            if signal.aborted() {
                return Err(compaction_cancelled_reason(Some(&active)));
            }
            self.hooks.on_will_compact.run(active.public_task()).await;
            let resolved_model = self.profile.resolve_model_context();
            thinking_effort = resolved_model.thinking_level.clone();
            let max_context_tokens = resolved_model.model_capabilities.max_context_tokens;
            let default_compaction_cap = (max_context_tokens > 0)
                .then(|| max_context_tokens.min(DEFAULT_COMPACTION_MAX_COMPLETION_TOKENS));
            let compaction_max_output_size = resolved_model
                .max_output_size
                .or(default_compaction_cap);
            let instruction = render_compaction_instruction(CompactionInstructionInput {
                custom_instruction: data.instruction.clone(),
            });
            let instruction_message = create_user_message(&instruction);
            let token_counting = self.token_counting.clone();
            let token_profile = build_history_token_profile(
                &original_history,
                self.request_tokens(&[]),
                Arc::new(move |message: &Message| {
                    token_counting.estimate_message(message, None) as u64
                }),
            );
            let max_attempts = resolved_model
                .compaction_max_attempts
                .unwrap_or(MAX_COMPACTION_RETRY_ATTEMPTS);
            let history = original_history
                .iter()
                .cloned()
                .map(Into::into)
                .collect::<Vec<Message>>();
            let this = self.this();
            let get_effective = Arc::new(move || this.get_effective_max_context_tokens());
            let this = self.this();
            let pre_shrink_instruction = instruction_message;
            let pre_shrink_history_tokens = original_history.clone();
            let pre_shrink_profile = token_profile.clone();
            let pre_shrink = Arc::new(move |history: &[Message], max_output| {
                this.pre_shrink_history_to_window_budget(
                    history,
                    &pre_shrink_history_tokens,
                    &pre_shrink_instruction,
                    max_output,
                    &pre_shrink_profile,
                )
            });
            let this = self.this();
            let should_recover = Arc::new(
                move |error: &(dyn Error + Send + Sync + 'static), estimated: u64| {
                    this.should_recover_from_context_overflow(error, estimated)
                },
            );
            let this = self.this();
            let observe = Arc::new(move |estimated| this.observe_context_overflow(estimated));
            let trace = active.trace.clone();
            let generation = generate_compaction_summary(CompactionGenerationDeps {
                state: &mut generation_state,
                history: &history,
                instruction: &instruction,
                signal: signal.clone(),
                max_attempts,
                compaction_max_output_size,
                origin_turn_id: active.origin_turn_id,
                token_profile,
                get_effective_max_context_tokens: get_effective,
                llm_requester: self.llm_requester.clone(),
                pre_shrink_history: pre_shrink,
                should_recover_from_context_overflow: should_recover,
                observe_context_overflow: observe,
                set_trace: Arc::new(move |next| *trace.lock() = Some(next)),
            })
            .await?;
            let attempt = generation.attempt;
            let request_overhead_tokens = self.request_tokens(&[]);
            let in_flight_turn = self.in_flight_turn_of(&self.context.get());
            let this = self.this();
            let apply = apply_compaction_round(
                &apply_deps,
                &CompactionApplyInput {
                    attempt_summary: attempt.summary.clone(),
                    attempt_usage: attempt.usage.clone(),
                    original_history: original_history.clone(),
                    non_history_state_before,
                    tokens_before,
                    dropped_count: generation.dropped_count,
                    request_overhead_tokens,
                    in_flight_turn,
                },
                signal.clone(),
                Arc::new(move || {
                    let current = this.active();
                    if let Some(current) = &current {
                        this.cancel_active(current);
                    }
                    compaction_cancelled_reason(current.as_ref())
                }),
            )
            .await?;
            let result = CompactionResult {
                summary: apply.summary,
                context_summary: None,
                compacted_count: apply.compacted_count,
                tokens_before: apply.tokens_before,
                tokens_after: apply.tokens_after,
                kept_user_message_count: None,
                kept_head_user_message_count: None,
                dropped_count: apply.dropped_count,
            };
            let mut properties = json!({
                "turn_id": active.origin_turn_id,
                "source": data.source,
                "tokens_before": result.tokens_before,
                "tokens_after": result.tokens_after,
                "duration_ms": started_at.elapsed().as_millis() as u64,
                "compacted_count": result.compacted_count,
                "dropped_count": result.dropped_count,
                "retry_count": generation_state.retry_count,
                "round": 1,
                "thinking_effort": thinking_effort.clone(),
                "trace_id": attempt.trace_id,
            })
            .as_object()
            .cloned()
            .unwrap_or_default();
            properties.extend(usage_telemetry(attempt.usage.as_ref()));
            for key in ["turn_id", "dropped_count", "trace_id"] {
                if properties.get(key).is_some_and(Value::is_null) {
                    properties.remove(key);
                }
            }
            self.telemetry.track2("compaction_finished", Some(properties));
            Ok(result)
        }
        .await;

        match result {
            Ok(result) => Ok(result),
            Err(error) => {
                if is_compaction_abort(error.as_ref()) {
                    return Err(error);
                }
                let mut properties = json!({
                    "turn_id": active.origin_turn_id,
                    "source": data.source,
                    "tokens_before": tokens_before,
                    "duration_ms": started_at.elapsed().as_millis() as u64,
                    "round": 1,
                    "retry_count": generation_state.retry_count,
                    "thinking_effort": thinking_effort,
                    "error_type": error_name(error.as_ref()),
                    "trace_id": find_api_status_error(error.as_ref())
                        .and_then(|status| status.trace_id.clone())
                        .or_else(|| active.trace_id()),
                })
                .as_object()
                .cloned()
                .unwrap_or_default();
                for key in ["turn_id", "trace_id"] {
                    if properties.get(key).is_some_and(Value::is_null) {
                        properties.remove(key);
                    }
                }
                self.telemetry.track2("compaction_failed", Some(properties));
                if error_code(error.as_ref()) == Some(AUTH_LOGIN_REQUIRED)
                    || error_code(error.as_ref()) == Some(PROVIDER_AUTH_ERROR_CODE)
                {
                    return Err(error);
                }
                Err(error2(
                    super::errors::COMPACTION_FAILED,
                    js_error_string(error.as_ref()),
                    None,
                    Some(error),
                ))
            }
        }
    }

    fn pre_shrink_history_to_window_budget(
        &self,
        history: &[Message],
        history_tokens: &[ContextMessage],
        instruction_message: &Message,
        compaction_max_output_size: Option<u32>,
        token_profile: &HistoryTokenProfile,
    ) -> Vec<Message> {
        let effective_max_tokens = self.get_effective_max_context_tokens();
        if effective_max_tokens == 0 {
            return history.to_vec();
        }
        let output_reserve = compaction_max_output_size
            .unwrap_or(effective_max_tokens / 8)
            .min(effective_max_tokens / 8);
        let input_limit = effective_max_tokens as i64 - output_reserve as i64;
        let total_input_tokens = self.history_request_tokens(history_tokens) as i64
            + self.token_counting.estimate_message(instruction_message, None) as i64;
        if total_input_tokens <= input_limit {
            return history.to_vec();
        }
        let message_budget =
            (input_limit as f64 * OVERFLOW_CONTEXT_SAFETY_RATIO).floor() as i64
                - token_profile.overhead_tokens as i64;
        if message_budget <= 0 {
            return history.to_vec();
        }
        let pre_shrunk = take_recent_messages_within_token_budget(
            history,
            message_budget as u64,
            Some(token_profile.estimate_message.as_ref()),
        );
        if pre_shrunk.is_empty() {
            history.to_vec()
        } else {
            pre_shrunk
        }
    }

    fn in_flight_turn_of(&self, history: &[ContextMessage]) -> Vec<ContextMessage> {
        if self.loop_service.snapshot().state != LoopState::Running {
            return Vec::new();
        }
        let Some(last_user_index) = history
            .iter()
            .rposition(|message| is_real_user_input(message))
        else {
            return Vec::new();
        };
        close_trailing_open_tool_exchange(&history[last_user_index + 1..])
    }

    fn token_count_with_pending(&self) -> u64 {
        let counted = self
            .token_counting
            .get(&agent_context_of_scope(&self.agent), None, None)
            .size as u64;
        match self.usage_anchored_request_tokens(&self.context.get()) {
            Some(anchored) => counted.max(anchored),
            None => counted,
        }
    }
}

impl IAgentFullCompactionService for AgentFullCompactionService {
    fn compacting(&self) -> Option<FullCompactionTask> {
        self.active().map(|active| active.public_task())
    }

    fn begin(&self, input: FullCompactionInput) -> Result<bool, WireAsyncError> {
        AgentFullCompactionService::begin(self, input).map_err(into_wire_error)
    }

    fn cancel(&self) {
        if let Some(active) = self.active() {
            let mut properties = json!({
                "from": "compacting",
                "trace_id": active.trace_id(),
            })
            .as_object()
            .cloned()
            .unwrap_or_default();
            if properties.get("trace_id").is_some_and(Value::is_null) {
                properties.remove("trace_id");
            }
            self.telemetry.track2("cancel", Some(properties));
            self.cancel_active(&active);
        }
    }

    fn hooks(&self) -> FullCompactionHooks {
        self.hooks.clone()
    }

    fn on_did_finish_compaction(&self) -> Event<FullCompactionTask> {
        self.on_did_finish_compaction.event()
    }
}

struct FullCompactionLoopErrorHandler {
    this: Weak<AgentFullCompactionService>,
}

#[async_trait]
impl LoopErrorHandler for FullCompactionLoopErrorHandler {
    fn id(&self) -> &str {
        "full-compaction"
    }

    fn matches(&self, context: &LoopErrorContext) -> bool {
        self.this.upgrade().is_some_and(|this| {
            this.should_recover_from_context_overflow(
                context.error.as_ref(),
                this.current_request_tokens(),
            )
        })
    }

    async fn handle(&self, context: LoopErrorContext) -> Result<Option<bool>, WireAsyncError> {
        let Some(this) = self.this.upgrade() else {
            return Ok(Some(false));
        };
        this.recover_from_context_overflow(context)
            .await
            .map(Some)
            .map_err(into_wire_error)
    }
}

fn usage_telemetry(usage: Option<&TokenUsage>) -> Map<String, Value> {
    let Some(usage) = usage else {
        return Map::new();
    };
    json!({
        "input_tokens": input_total(usage),
        "output_tokens": usage.output,
        "input_cache_read": usage.input_cache_read,
        "input_cache_creation": usage.input_cache_creation,
    })
    .as_object()
    .cloned()
    .unwrap_or_default()
}

fn compaction_cancelled_reason(active: Option<&ActiveCompaction>) -> SharedError {
    active
        .and_then(|active| active.task.abort_controller.signal().reason())
        .unwrap_or_else(|| Arc::new(AbortError::new("Compaction cancelled.")))
}

fn is_compaction_abort(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    is_abort_error(error)
        || crate::llm_adapter::contract::errors::is_abort_error(error)
        || error
            .downcast_ref::<Error2>()
            .is_some_and(|error| error.name == "AbortError")
        || error
            .downcast_ref::<kimi_core::primitives::errors::Error2>()
            .is_some_and(|error| error.name() == "AbortError")
}

fn error_code<'a>(error: &'a (dyn Error + Send + Sync + 'static)) -> Option<&'a str> {
    if let Some(error) = error.downcast_ref::<Error2>() {
        return Some(error.code.as_str());
    }
    if let Some(error) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        return Some(error.code().as_str());
    }
    None
}

fn error_name(error: &(dyn Error + Send + Sync + 'static)) -> String {
    if let Some(error) = error.downcast_ref::<Error2>() {
        return error.name.clone();
    }
    if let Some(error) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        return error.name().to_string();
    }
    if error.is::<CompactionTruncatedError>() {
        return "CompactionTruncatedError".to_string();
    }
    if error.is::<AbortError>() {
        return "AbortError".to_string();
    }
    if let Some(error) = chat_error_of(error) {
        return error.name.clone();
    }
    "Error".to_string()
}

fn chat_error_of<'a>(
    error: &'a (dyn Error + Send + Sync + 'static),
) -> Option<&'a crate::llm_adapter::contract::errors::ChatProviderError> {
    if let Some(error) = error.downcast_ref::<crate::llm_adapter::contract::errors::ChatProviderError>() {
        return Some(error);
    }
    if let Some(error) = error.downcast_ref::<crate::llm_adapter::contract::errors::APIConnectionError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<crate::llm_adapter::contract::errors::VideoUploadUnsupportedError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<crate::llm_adapter::contract::errors::ImageUploadUnsupportedError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<crate::llm_adapter::contract::errors::APITimeoutError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<crate::llm_adapter::contract::errors::APIEmptyResponseError>() {
        return Some(&error.0);
    }
    find_api_status_error(error).map(|error| &error.base)
}

fn js_error_string(error: &(dyn Error + Send + Sync + 'static)) -> String {
    let name = error_name(error);
    let message = error.to_string();
    if message.is_empty() {
        name
    } else {
        format!("{name}: {message}")
    }
}

fn error2(
    code: &str,
    message: impl Into<String>,
    details: Option<HashMap<String, Value>>,
    cause: Option<SharedError>,
) -> SharedError {
    Arc::new(Error2::new(
        code.to_string(),
        message,
        Error2Options {
            details,
            cause,
            name: None,
        },
    ))
}

fn shared_error(error: WireAsyncError) -> SharedError {
    Arc::from(error)
}

struct BoxedSharedError(SharedError);

impl fmt::Debug for BoxedSharedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("BoxedSharedError").field(&self.0.to_string()).finish()
    }
}

impl fmt::Display for BoxedSharedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl Error for BoxedSharedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

fn into_wire_error(error: SharedError) -> WireAsyncError {
    Box::new(BoxedSharedError(error))
}
