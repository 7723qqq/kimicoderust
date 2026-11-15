use indexmap::{IndexMap, IndexSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use once_cell::sync::Lazy;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ServiceDependency, refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::_base::event::Subscription;
use crate::_base::utils::canonical_args::canonical_telemetry_args;
use crate::_base::utils::env::parse_boolean_env;
use crate::agent::r#loop::r#loop::{
    I_AGENT_LOOP_SERVICE, IAgentLoopService, LoopNotify,
};
use crate::agent::r#loop::turn_ops::TurnEnded;
use crate::_base::state::state_registry::StateKey;
use crate::agent::state::errors::StateError;
use crate::agent::state::agent_state::{I_AGENT_STATE_SERVICE, IAgentStateService};
use crate::agent::tool_executor::tool_executor::{
    I_AGENT_TOOL_EXECUTOR_SERVICE, ExecutableToolOutput, ExecutableToolResult,
    IAgentToolExecutorService, ToolCallDupType,
};
use crate::agent::tool_executor::tool_hooks::ToolDidExecuteContext;
use crate::app::bootstrap::bootstrap::{I_BOOTSTRAP_SERVICE, IBootstrapService};
use crate::app::event::event_bus::{I_EVENT_BUS, IEventBus};
use crate::app::telemetry::telemetry::{I_TELEMETRY_SERVICE, ITelemetryService};
use crate::features::reminder::system_reminder::wrap_system_reminder;
use crate::hooks::HookRegistration;
use crate::human::llm::message::ContentPart;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::tool::tool_args_parse::parse_tool_arguments;

use super::tool_dedupe::{
    I_AGENT_TOOL_DEDUPE_SERVICE, IAgentToolDedupeService, REPEAT_BREAKER_STOP_REASON,
    ToolDedupeOutput, ToolDedupeResult,
};

pub const REPEAT_BREAKER_ENV: &str = "KIMI_CODE_REPEAT_BREAKER";

static REMINDER_TEXT_1: LazyLock<String> = LazyLock::new(|| {
    format!(
        "\n\n{}",
        wrap_system_reminder(
            "The same tool call has been repeated several times in a row. Before making your next call, write one sentence stating what new information you expect it to produce. Then act on that sentence: if it names something this result does not already give you, choose the action that best provides it; otherwise, continue with the evidence you already have.",
        )
    )
});

static REMINDER_TEXT_3: LazyLock<String> = LazyLock::new(|| {
    format!(
        "\n\n{}",
        wrap_system_reminder(
            "Write your final response now, without any further tool calls. Cover: the current blocker, each approach you have tried and what it established, and the specific information or decision you need from the user to unblock progress. Text only.",
        )
    )
});

const REPEAT_REMINDER_1_START: u64 = 3;
const REPEAT_REMINDER_2_START: u64 = 5;
const REPEAT_REMINDER_3_START: u64 = 8;
const REPEAT_FORCE_STOP_STREAK: u64 = 12;

fn make_reminder_text_2(repeat_count: u64) -> String {
    format!(
        "\n\n{}",
        wrap_system_reminder(&format!(
            "The same tool call has now been issued {repeat_count} times in a row. Choose exactly one of the following and state your choice before acting:\n(1) Falsification check: run the cheapest test that could conclusively disprove your current approach, if such a test exists.\n(2) Missing input: tell the user precisely what information or decision you need to proceed, and ask for it.\n(3) Conclude: deliver your best result based on the evidence already gathered, listing anything that remains uncertain."
        ))
    )
}

fn handoff_veto_text() -> String {
    format!(
        "This turn was ended by the repeat breaker after the same tool call was issued {REPEAT_FORCE_STOP_STREAK} times in a row. This step accepts a text response only, so the tool call was not executed. Reply in text: the current blocker, what you tried, and what you need next."
    )
}

fn handoff_veto_result() -> ToolDedupeResult {
    ToolDedupeResult {
        output: ToolDedupeOutput::Text(handoff_veto_text()),
        is_error: Some(true),
        stop_turn: Some(true),
        stop_turn_reason: Some(REPEAT_BREAKER_STOP_REASON.to_string()),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HandoffPhase {
    #[default]
    Idle,
    Pending,
    Active,
    Done,
}

#[derive(Clone)]
struct Deferred {
    promise: Shared<BoxFuture<'static, ToolDedupeResult>>,
    resolver: Arc<Mutex<Option<oneshot::Sender<ToolDedupeResult>>>>,
}

impl Deferred {
    fn resolve(&self, result: ToolDedupeResult) {
        if let Some(resolve) = self.resolver.lock().take() {
            let _ = resolve.send(result);
        }
    }
}

fn lost_original_result() -> ToolDedupeResult {
    ToolDedupeResult {
        output: ToolDedupeOutput::Text(
            "Tool call deduplicated but original result was lost".to_string(),
        ),
        is_error: Some(true),
        ..Default::default()
    }
}

fn make_deferred() -> Deferred {
    let (resolve, receiver) = oneshot::channel();
    let promise = async move {
        match receiver.await {
            Ok(result) => result,
            Err(_) => std::future::pending::<ToolDedupeResult>().await,
        }
    }
    .boxed()
    .shared();
    Deferred {
        promise,
        resolver: Arc::new(Mutex::new(Some(resolve))),
    }
}

fn make_key(tool_name: &str, args: &Value) -> String {
    format!("{tool_name} {}", canonical_telemetry_args(args))
}

fn args_hash(args: &Value) -> String {
    let digest = Sha256::digest(canonical_telemetry_args(args).as_bytes());
    digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn call_signature(key: &str) -> String {
    format!("{:x}", Sha256::digest(key.as_bytes()))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnCallRecord {
    count: u64,
    last_step: u64,
}

fn append_reminder(mut result: ToolDedupeResult, reminder_text: &str) -> ToolDedupeResult {
    result.output = match result.output {
        ToolDedupeOutput::Text(output) => {
            ToolDedupeOutput::Text(format!("{output}{reminder_text}"))
        }
        ToolDedupeOutput::Parts(mut parts) => {
            if let Some(ContentPart::Text { text, .. }) = parts.last_mut() {
                let text = std::mem::take(text);
                let last = parts.len() - 1;
                parts[last] = ContentPart::Text {
                    text: format!("{text}{reminder_text}"),
                    meta: None,
                };
            } else {
                parts.push(ContentPart::Text {
                    text: reminder_text.to_string(),
                    meta: None,
                });
            }
            ToolDedupeOutput::Parts(parts)
        }
    };
    if let Some(spill) = result.spill.as_mut() {
        spill.suffix = Some(format!(
            "{}{reminder_text}",
            spill.suffix.as_deref().unwrap_or("")
        ));
    }
    result
}

fn force_stop_result(result: ToolDedupeResult, reminder_text: &str) -> ToolDedupeResult {
    let mut result = append_reminder(result, reminder_text);
    result.stop_turn = Some(true);
    result.stop_turn_reason = Some(REPEAT_BREAKER_STOP_REASON.to_string());
    result
}

pub fn tool_dedupe_step_calls_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.stepCalls", || json!([]))
}

pub fn tool_dedupe_original_call_index_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.originalCallIndex", || json!({}))
}

pub fn tool_dedupe_synthetic_call_ids_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.syntheticCallIds", || json!([]))
}

pub fn tool_dedupe_call_key_by_call_id_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.callKeyByCallId", || json!({}))
}

pub fn tool_dedupe_consecutive_key_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.consecutiveKey", || Value::Null)
}

pub fn tool_dedupe_consecutive_count_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.consecutiveCount", || json!(0))
}

pub fn tool_dedupe_active_turn_id_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.activeTurnId", || Value::Null)
}

pub fn tool_dedupe_active_step_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.activeStep", || json!(0))
}

pub fn tool_dedupe_turn_call_records_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.turnCallRecords", || json!({}))
}

pub fn tool_dedupe_turn_repeat_count_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.turnRepeatCount", || json!(0))
}

pub fn tool_dedupe_handoff_phase_key() -> StateKey<Value> {
    StateKey::new("toolDedupe.handoffPhase", || json!("idle"))
}

struct AgentToolDedupeServiceInner {
    telemetry: Arc<dyn ITelemetryService>,
    agent_loop: Arc<dyn IAgentLoopService>,
    tool_executor: Arc<dyn IAgentToolExecutorService>,
    states: Arc<dyn IAgentStateService>,
    step_deferreds: Mutex<IndexMap<String, Deferred>>,
    handoff_vetoed_call_ids: Mutex<IndexSet<String>>,
    force_stopped_in_step: AtomicBool,
    repeat_breaker_enabled: bool,
}

struct DedupeRegistrations {
    _states: Vec<DisposableRef>,
    _turn_ended: DisposableRef,
    _begin_step: HookRegistration<crate::agent::r#loop::r#loop::BeforeStepContext>,
    _end_step: HookRegistration<crate::agent::r#loop::r#loop::AfterStepContext>,
    _before_execute: Subscription,
    _did_execute: HookRegistration<ToolDidExecuteContext>,
}

pub struct AgentToolDedupeService {
    inner: Arc<AgentToolDedupeServiceInner>,
    _registrations: DedupeRegistrations,
}

impl AgentToolDedupeService {
    pub fn new(
        telemetry: Arc<dyn ITelemetryService>,
        agent_loop: Arc<dyn IAgentLoopService>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
        states: Arc<dyn IAgentStateService>,
        bootstrap: Arc<dyn IBootstrapService>,
        event_bus: Arc<dyn IEventBus>,
    ) -> Result<Self, StateError> {
        let repeat_breaker_enabled = parse_boolean_env(
            bootstrap.get_env(REPEAT_BREAKER_ENV).as_deref(),
        ) == Some(true);
        let state_registrations = vec![
            states.contribute_state(tool_dedupe_step_calls_key())?,
            states.contribute_state(tool_dedupe_original_call_index_key())?,
            states.contribute_state(tool_dedupe_synthetic_call_ids_key())?,
            states.contribute_state(tool_dedupe_call_key_by_call_id_key())?,
            states.contribute_state(tool_dedupe_consecutive_key_key())?,
            states.contribute_state(tool_dedupe_consecutive_count_key())?,
            states.contribute_state(tool_dedupe_active_turn_id_key())?,
            states.contribute_state(tool_dedupe_active_step_key())?,
            states.contribute_state(tool_dedupe_turn_call_records_key())?,
            states.contribute_state(tool_dedupe_turn_repeat_count_key())?,
            states.contribute_state(tool_dedupe_handoff_phase_key())?,
        ];
        let inner = Arc::new(AgentToolDedupeServiceInner {
            telemetry,
            agent_loop,
            tool_executor,
            states,
            step_deferreds: Mutex::new(IndexMap::new()),
            handoff_vetoed_call_ids: Mutex::new(IndexSet::new()),
            force_stopped_in_step: AtomicBool::new(false),
            repeat_breaker_enabled,
        });
        let turn_ended = event_bus.subscribe_class(TurnEnded::class(), {
            let weak = Arc::downgrade(&inner);
            Arc::new(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.clear_turn_records();
                }
            })
        });
        let begin_step = inner
            .agent_loop
            .hooks()
            .on_will_begin_step
            .register(
                "toolDedupe",
                Arc::new({
                    let weak = Arc::downgrade(&inner);
                    move |context, next| {
                        let weak = weak.clone();
                        Box::pin(async move {
                            if let Some(this) = weak.upgrade() {
                                this.begin_step(Some(context.turn_id), Some(context.step.into()));
                            }
                            next.proceed(None).await
                        })
                    }
                }),
                Default::default(),
            )
            .expect("tool dedupe begin-step hook registers");
        let end_step = inner
            .agent_loop
            .hooks()
            .on_did_finish_step
            .register(
                "toolDedupe",
                Arc::new({
                    let weak = Arc::downgrade(&inner);
                    move |context, next| {
                        let weak = weak.clone();
                        Box::pin(async move {
                            if let Some(this) = weak.upgrade() {
                                this.end_step();
                                this.settle_handoff(context.turn_id);
                            }
                            next.proceed(None).await
                        })
                    }
                }),
                Default::default(),
            )
            .expect("tool dedupe end-step hook registers");
        let before_execute = inner
            .tool_executor
            .on_before_execute_tool()
            .subscribe({
                let weak = Arc::downgrade(&inner);
                move |event| {
                    let Some(this) = weak.upgrade() else {
                        return;
                    };
                    if this.handoff_phase() == HandoffPhase::Active {
                        this.handoff_vetoed_call_ids
                            .lock()
                            .insert(event.context.base.tool_call.id.clone());
                        event.veto(into_executable(handoff_veto_result()));
                        return;
                    }
                    let checked = this.check_tool_call(
                        &event.context.base.tool_call.id,
                        &event.context.base.tool_call.name,
                        &event.context.base.args,
                        event.context.base.trace.as_ref(),
                    );
                    if let Some(result) = checked {
                        event.veto(into_executable(result));
                    }
                }
            });
        let did_execute = inner
            .tool_executor
            .hooks()
            .on_did_execute_tool
            .register(
                "toolDedupe",
                Arc::new({
                    let weak = Arc::downgrade(&inner);
                    move |mut context: ToolDidExecuteContext, next| {
                        let weak = weak.clone();
                        Box::pin(async move {
                            let Some(this) = weak.upgrade() else {
                                return next.proceed(Some(context)).await;
                            };
                            if this.handoff_phase() == HandoffPhase::Active {
                                this.handoff_vetoed_call_ids
                                    .lock()
                                    .insert(context.base.tool_call.id.clone());
                                context.result = into_executable(handoff_veto_result());
                                context.stop_turn = Some(true);
                                return next.proceed(Some(context)).await;
                            }
                            this.register_skipped(
                                &context.base.tool_call.id,
                                &context.base.tool_call.name,
                                &context.base.args,
                                context.base.tool_call.arguments.as_deref(),
                                context.base.trace.as_ref(),
                            );
                            context.result = into_executable(
                                this.finalize_result(
                                    &context.base.tool_call.id,
                                    &context.base.tool_call.name,
                                    &context.base.args,
                                    from_executable(context.result),
                                    context.base.trace.as_ref(),
                                )
                                .await,
                            );
                            if context.result.stop_turn == Some(true) {
                                context.stop_turn = Some(true);
                            }
                            next.proceed(Some(context)).await
                        })
                    }
                }),
                Default::default(),
            )
            .expect("tool dedupe did-execute hook registers");
        Ok(Self {
            inner,
            _registrations: DedupeRegistrations {
                _states: state_registrations,
                _turn_ended: Arc::from(turn_ended),
                _begin_step: begin_step,
                _end_step: end_step,
                _before_execute: before_execute,
                _did_execute: did_execute,
            },
        })
    }

    fn begin_step(&self, turn_id: Option<u64>, step: Option<u64>) {
        self.inner.begin_step(turn_id, step);
    }

    fn end_step(&self) {
        self.inner.end_step();
    }

    fn settle_handoff(&self, turn_id: u64) {
        self.inner.settle_handoff(turn_id);
    }
}

impl AgentToolDedupeServiceInner {
    fn step_calls(&self) -> Vec<String> {
        state_value(&self.states, tool_dedupe_step_calls_key()).unwrap_or_default()
    }

    fn set_step_calls(&self, value: Vec<String>) {
        set_state(&self.states, tool_dedupe_step_calls_key(), value);
    }

    fn original_call_index(&self) -> IndexMap<String, usize> {
        state_value(&self.states, tool_dedupe_original_call_index_key()).unwrap_or_default()
    }

    fn set_original_call_index(&self, value: IndexMap<String, usize>) {
        set_state(&self.states, tool_dedupe_original_call_index_key(), value);
    }

    fn synthetic_call_ids(&self) -> IndexSet<String> {
        state_value(&self.states, tool_dedupe_synthetic_call_ids_key()).unwrap_or_default()
    }

    fn set_synthetic_call_ids(&self, value: IndexSet<String>) {
        set_state(&self.states, tool_dedupe_synthetic_call_ids_key(), value);
    }

    fn call_key_by_call_id(&self) -> IndexMap<String, String> {
        state_value(&self.states, tool_dedupe_call_key_by_call_id_key()).unwrap_or_default()
    }

    fn set_call_key_by_call_id(&self, value: IndexMap<String, String>) {
        set_state(&self.states, tool_dedupe_call_key_by_call_id_key(), value);
    }

    fn consecutive_key(&self) -> Option<String> {
        state_value(&self.states, tool_dedupe_consecutive_key_key()).unwrap_or_default()
    }

    fn set_consecutive_key(&self, value: Option<String>) {
        set_state(&self.states, tool_dedupe_consecutive_key_key(), value);
    }

    fn consecutive_count(&self) -> u64 {
        state_value(&self.states, tool_dedupe_consecutive_count_key()).unwrap_or_default()
    }

    fn set_consecutive_count(&self, value: u64) {
        set_state(&self.states, tool_dedupe_consecutive_count_key(), value);
    }

    fn active_turn_id(&self) -> Option<u64> {
        state_value(&self.states, tool_dedupe_active_turn_id_key()).unwrap_or_default()
    }

    fn set_active_turn_id(&self, value: Option<u64>) {
        set_state(&self.states, tool_dedupe_active_turn_id_key(), value);
    }

    fn active_step(&self) -> u64 {
        state_value(&self.states, tool_dedupe_active_step_key()).unwrap_or_default()
    }

    fn set_active_step(&self, value: u64) {
        set_state(&self.states, tool_dedupe_active_step_key(), value);
    }

    fn turn_call_records(&self) -> IndexMap<String, TurnCallRecord> {
        state_value(&self.states, tool_dedupe_turn_call_records_key()).unwrap_or_default()
    }

    fn set_turn_call_records(&self, value: IndexMap<String, TurnCallRecord>) {
        set_state(&self.states, tool_dedupe_turn_call_records_key(), value);
    }

    fn turn_repeat_count(&self) -> u64 {
        state_value(&self.states, tool_dedupe_turn_repeat_count_key()).unwrap_or_default()
    }

    fn set_turn_repeat_count(&self, value: u64) {
        set_state(&self.states, tool_dedupe_turn_repeat_count_key(), value);
    }

    fn handoff_phase(&self) -> HandoffPhase {
        state_value(&self.states, tool_dedupe_handoff_phase_key()).unwrap_or_default()
    }

    fn set_handoff_phase(&self, value: HandoffPhase) {
        set_state(&self.states, tool_dedupe_handoff_phase_key(), value);
    }

    fn clear_turn_records(&self) {
        self.set_turn_call_records(IndexMap::new());
        self.set_turn_repeat_count(0);
    }

    fn begin_step(&self, turn_id: Option<u64>, step: Option<u64>) {
        if let Some(turn_id) = turn_id
            && Some(turn_id) != self.active_turn_id()
        {
            self.set_active_turn_id(Some(turn_id));
            self.set_consecutive_key(None);
            self.set_consecutive_count(0);
            self.set_handoff_phase(HandoffPhase::Idle);
            self.clear_turn_records();
        }
        if let Some(step) = step {
            self.set_active_step(step);
        }
        self.force_stopped_in_step.store(false, Ordering::SeqCst);
        self.handoff_vetoed_call_ids.lock().clear();
        let deferreds = self
            .step_deferreds
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for deferred in deferreds {
            deferred.resolve(lost_original_result());
        }
        self.step_deferreds.lock().clear();
        self.set_step_calls(Vec::new());
        self.set_original_call_index(IndexMap::new());
        self.set_synthetic_call_ids(IndexSet::new());
        self.set_call_key_by_call_id(IndexMap::new());
    }

    fn end_step(&self) {
        for key in self.step_calls() {
            if Some(&key) == self.consecutive_key().as_ref() {
                self.set_consecutive_count(self.consecutive_count() + 1);
            } else {
                self.set_consecutive_key(Some(key));
                self.set_consecutive_count(1);
            }
        }
    }

    fn settle_handoff(self: &Arc<Self>, turn_id: u64) {
        let phase = self.handoff_phase();
        if phase == HandoffPhase::Active {
            self.set_handoff_phase(HandoffPhase::Done);
            let outcome = if self.handoff_vetoed_call_ids.lock().is_empty() {
                "text"
            } else {
                "vetoed"
            };
            self.track(
                "tool_call_repeat_handoff",
                json!({ "turn_id": turn_id, "outcome": outcome }),
            );
            return;
        }
        if phase != HandoffPhase::Idle || !self.force_stopped_in_step.load(Ordering::SeqCst) {
            return;
        }
        self.set_handoff_phase(HandoffPhase::Pending);
        let on_consume = Arc::new({
            let weak = Arc::downgrade(self);
            move || {
                if let Some(this) = weak.upgrade() {
                    this.set_handoff_phase(HandoffPhase::Active);
                }
            }
        });
        let on_drop = Arc::new({
            let weak = Arc::downgrade(self);
            move || {
                if let Some(this) = weak.upgrade() {
                    this.set_handoff_phase(HandoffPhase::Done);
                }
            }
        });
        let _ = self.agent_loop.notify(Some(LoopNotify {
            message: None,
            turn_scoped: None,
            bypass_max_steps: Some(true),
            on_consume: Some(on_consume),
            on_drop: Some(on_drop),
        }));
    }

    fn record_turn_repeat(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
        key: &str,
        trace: Option<&LLMRequestTrace>,
    ) {
        let signature = call_signature(key);
        let active_step = self.active_step();
        let mut records = self.turn_call_records();
        let Some(record) = records.get_mut(&signature) else {
            records.insert(
                signature,
                TurnCallRecord {
                    count: 0,
                    last_step: active_step,
                },
            );
            self.set_turn_call_records(records);
            return;
        };
        if record.last_step == active_step {
            return;
        }
        record.count += 1;
        record.last_step = active_step;
        self.set_turn_call_records(records);
        let repeat_count = self.turn_repeat_count() + 1;
        self.set_turn_repeat_count(repeat_count);
        let mut properties = Map::new();
        insert_optional(&mut properties, "turn_id", self.active_turn_id().map(Value::from));
        properties.extend(telemetry_properties(json!({
            "step_no": active_step,
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "turn_repeat_count": repeat_count,
            "args_hash": args_hash(args),
        })));
        insert_optional(
            &mut properties,
            "trace_id",
            trace.and_then(|trace| trace.trace_id.clone()).map(Value::String),
        );
        self.telemetry.track2("tool_call_turn_repeat", Some(properties));
    }

    fn check_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
        trace: Option<&LLMRequestTrace>,
    ) -> Option<ToolDedupeResult> {
        let key = make_key(tool_name, args);
        let mut step_calls = self.step_calls();
        let index = step_calls.len();
        step_calls.push(key.clone());
        self.set_step_calls(step_calls);
        let mut call_keys = self.call_key_by_call_id();
        call_keys.insert(tool_call_id.to_string(), key.clone());
        self.set_call_key_by_call_id(call_keys);
        if self.step_deferreds.lock().contains_key(&key) {
            let mut synthetic = self.synthetic_call_ids();
            synthetic.insert(tool_call_id.to_string());
            self.set_synthetic_call_ids(synthetic);
            self.record_dup_type(
                tool_call_id,
                tool_name,
                args,
                ToolCallDupType::SameStep,
                trace,
            );
            return Some(ToolDedupeResult::default());
        }
        self.record_turn_repeat(tool_call_id, tool_name, args, &key, trace);
        self.step_deferreds
            .lock()
            .insert(key.clone(), make_deferred());
        let mut original = self.original_call_index();
        original.insert(tool_call_id.to_string(), index);
        self.set_original_call_index(original);
        if self.consecutive_key().as_deref() == Some(key.as_str()) && self.consecutive_count() > 0 {
            self.record_dup_type(
                tool_call_id,
                tool_name,
                args,
                ToolCallDupType::CrossStep,
                trace,
            );
        }
        None
    }

    fn register_skipped(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
        raw_arguments: Option<&str>,
        trace: Option<&LLMRequestTrace>,
    ) {
        if self.call_key_by_call_id().contains_key(tool_call_id) {
            return;
        }
        let raw = raw_arguments.filter(|raw| parse_tool_arguments(Some(*raw)).parse_failed);
        let key_args = raw
            .map(|raw| Value::String(raw.to_string()))
            .unwrap_or_else(|| args.clone());
        let _ = self.check_tool_call(tool_call_id, tool_name, &key_args, trace);
    }

    fn record_dup_type(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &Value,
        dup_type: ToolCallDupType,
        trace: Option<&LLMRequestTrace>,
    ) {
        self.tool_executor.record_dup_type(tool_call_id, dup_type);
        let mut properties = Map::new();
        insert_optional(&mut properties, "turn_id", self.active_turn_id().map(Value::from));
        properties.extend(telemetry_properties(json!({
            "step_no": self.active_step(),
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "dup_type": dup_type.as_str(),
            "args_hash": args_hash(args),
        })));
        insert_optional(
            &mut properties,
            "trace_id",
            trace.and_then(|trace| trace.trace_id.clone()).map(Value::String),
        );
        self.telemetry
            .track2("tool_call_dedup_detected", Some(properties));
    }

    async fn finalize_result(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        _args: &Value,
        result: ToolDedupeResult,
        trace: Option<&LLMRequestTrace>,
    ) -> ToolDedupeResult {
        let mut call_keys = self.call_key_by_call_id();
        let Some(key) = call_keys.remove(tool_call_id) else {
            return result;
        };
        self.set_call_key_by_call_id(call_keys);
        let mut synthetic = self.synthetic_call_ids();
        if synthetic.remove(tool_call_id) {
            self.set_synthetic_call_ids(synthetic);
            let deferred = self.step_deferreds.lock().get(&key).cloned();
            return match deferred {
                Some(deferred) => deferred.promise.clone().await,
                None => result,
            };
        }
        let mut original = self.original_call_index();
        let Some(index) = original.remove(tool_call_id) else {
            return result;
        };
        self.set_original_call_index(original);
        let mut last_key = self.consecutive_key();
        let mut streak = self.consecutive_count();
        let step_calls = self.step_calls();
        for key in step_calls.iter().take(index + 1) {
            if last_key.as_ref() == Some(key) {
                streak += 1;
            } else {
                last_key = Some(key.clone());
                streak = 1;
            }
        }
        let mut final_result = result;
        let mut action = "none";
        if self.repeat_breaker_enabled {
            if streak >= REPEAT_FORCE_STOP_STREAK {
                final_result = force_stop_result(final_result, &REMINDER_TEXT_3);
                action = "stop";
                self.force_stopped_in_step.store(true, Ordering::SeqCst);
            } else if streak >= REPEAT_REMINDER_3_START {
                final_result = append_reminder(final_result, &REMINDER_TEXT_3);
                action = "r3";
            } else if streak >= REPEAT_REMINDER_2_START {
                final_result = append_reminder(final_result, &make_reminder_text_2(streak));
                action = "r2";
            } else if streak >= REPEAT_REMINDER_1_START {
                final_result = append_reminder(final_result, &REMINDER_TEXT_1);
                action = "r1";
            }
        }
        if streak >= 2 {
            let mut properties = Map::new();
            insert_optional(&mut properties, "turn_id", self.active_turn_id().map(Value::from));
            properties.extend(telemetry_properties(json!({
                "tool_name": tool_name,
                "repeat_count": streak,
                "action": action,
            })));
            insert_optional(
                &mut properties,
                "trace_id",
                trace.and_then(|trace| trace.trace_id.clone()).map(Value::String),
            );
            self.telemetry.track2("tool_call_repeat", Some(properties));
        }
        if let Some(deferred) = self.step_deferreds.lock().get(&key).cloned() {
            deferred.resolve(final_result.clone());
        }
        final_result
    }

    fn track(&self, event: &str, properties: Value) {
        self.telemetry
            .track2(event, Some(telemetry_properties(properties)));
    }
}

impl IAgentToolDedupeService for AgentToolDedupeService {}

fn state_value<T: DeserializeOwned>(
    states: &Arc<dyn IAgentStateService>,
    key: StateKey<Value>,
) -> Option<T> {
    let value = states.get(&key);
    Some(serde_json::from_value(value).expect("tool dedupe state has its registered shape"))
}

fn set_state<T: Serialize>(states: &Arc<dyn IAgentStateService>, key: StateKey<Value>, value: T) {
    let value = serde_json::to_value(value).expect("tool dedupe state is serializable");
    states.set(&key, value);
}

fn telemetry_properties(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn insert_optional(properties: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        properties.insert(key.to_string(), value);
    }
}

fn into_executable(result: ToolDedupeResult) -> ExecutableToolResult {
    ExecutableToolResult {
        output: match result.output {
            ToolDedupeOutput::Text(text) => ExecutableToolOutput::Text(text),
            ToolDedupeOutput::Parts(parts) => ExecutableToolOutput::Parts(parts),
        },
        is_error: result.is_error,
        stop_turn: result.stop_turn,
        stop_turn_reason: result.stop_turn_reason,
        truncated: result.truncated,
        note: result.note,
        delivery: result.delivery,
        spill: result.spill,
        spill_exempt: result.spill_exempt,
    }
}

fn from_executable(result: ExecutableToolResult) -> ToolDedupeResult {
    ToolDedupeResult {
        output: match result.output {
            ExecutableToolOutput::Text(text) => ToolDedupeOutput::Text(text),
            ExecutableToolOutput::Parts(parts) => ToolDedupeOutput::Parts(parts),
        },
        is_error: result.is_error,
        stop_turn: result.stop_turn,
        stop_turn_reason: result.stop_turn_reason,
        truncated: result.truncated,
        note: result.note,
        delivery: result.delivery,
        spill: result.spill,
        spill_exempt: result.spill_exempt,
        message: None,
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("toolDedupe dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static AGENT_TOOL_DEDUPE_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IAgentToolDedupeService> = Arc::new(
            AgentToolDedupeService::new(
                dependency::<dyn ITelemetryService>(&args, 0),
                dependency::<dyn IAgentLoopService>(&args, 1),
                dependency::<dyn IAgentToolExecutorService>(&args, 2),
                dependency::<dyn IAgentStateService>(&args, 3),
                dependency::<dyn IBootstrapService>(&args, 4),
                dependency::<dyn IEventBus>(&args, 5),
            )
            .expect("tool dedupe service constructs from resolved agent-scope dependencies"),
        );
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: refine_service_decorator(&I_TELEMETRY_SERVICE),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_LOOP_SERVICE),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_TOOL_EXECUTOR_SERVICE),
                index: 2,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_STATE_SERVICE),
                index: 3,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_BOOTSTRAP_SERVICE),
                index: 4,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_EVENT_BUS),
                index: 5,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        refine_service_decorator(&I_AGENT_TOOL_DEDUPE_SERVICE),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "toolDedupe",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_agent_tool_dedupe_service_registered() -> Result<(), String> {
    Lazy::force(&AGENT_TOOL_DEDUPE_SERVICE_REGISTRATION).clone()
}

