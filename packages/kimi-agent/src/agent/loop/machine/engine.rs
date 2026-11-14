use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::_base::utils::abort::AbortSignal;
use crate::agent::llm_requester::llm_requester::{
    AgentLLMRequestFinish, AgentLLMRequestSource, IAgentLLMRequesterService,
};
use crate::human::agent::events::{MESSAGE_APPENDED, TURN_ENDED};
use crate::human::agent::machine::{
    AgentEmitted, AgentEvent, AgentMachineSelf, AgentReminderEntry, AgentSubscription, PromptGate,
};
pub use crate::human::agent::machine::PromptGateVerdict;
use crate::human::agent::slices::{AgentEventStore, agent_slices};
use crate::human::agent::turn::{
    AssistantEntry, HistoryMessage, SystemEntry, TurnLlmEvent, UserEntry, create_turn_machine,
};
use crate::human::credentials::credentials::CredentialsRecovery;
use crate::human::eventStore::eventStore::{SyncEventStoreOptions, create_event_store_sync};
use crate::human::eventStore::events::ExternalEvent;
use crate::human::eventStore::journal::{
    JournalRecord, JournalRef, StoreJournal, SyncStoreJournal, memory_journal,
};
use crate::human::llm::capability::UNKNOWN_CAPABILITY;
use crate::human::llm::errors::LlmErrorMessage;
use crate::human::llm::finish_reason::FinishInfo;
use crate::human::llm::message::{ContentPart, StreamIndex, StreamedMessagePart, ToolCall};
use crate::human::llm::model::LlmModel;
use crate::human::llm::requester::actor::LlmEvent;
use crate::human::llm::requester::recovery::{
    LlmRecovery, LlmRecoveryContext, LlmRecoveryProposalRecord, LlmRecoveryRecord,
};
use crate::human::llm::requester::requester::{
    LlmCredential, LlmCredentialProvider, LlmRequestConfig, LlmRequestEvent,
};
use crate::human::llm::requester::retry::{LlmRetryOptions, resolve_max_attempts};
use crate::human::llm::usage::{TokenUsage, empty_usage};
use crate::human::tool::executor::{ToolExecutor, ToolResult as MachineToolResult, ToolUpdate};
use crate::human::tool::machine::{ToolEvent, create_tool_machine};
use crate::human::tool::tool::ToolDefinition;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::llm_adapter::model::model_requester::ModelRequestTiming;
use crate::tool_input_display::ToolInputDisplay;
use crate::tool::tool_contract::ToolDefinition as ToolInfo;
use crate::wire::record::WireAsyncError;

use super::requester::{
    MachineRequester, MachineRequesterGateDecision, create_machine_requester,
};
use super::store_journal::{appended_store_journal, seeded_store_journal};
use super::tools::{
    AgentToolResult, AgentToolUpdate, IAgentToolExecutorService, MachineToolCallPayload,
    MachineTools, create_machine_tools,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MachineEngineDelta {
    Assistant { delta: String },
    #[serde(rename_all = "camelCase")]
    Thinking {
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details_index: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_key: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        tool_call_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments_part: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started: Option<bool>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MachineTurnOutcome {
    Done,
    Failed,
    Aborted,
}

#[derive(Clone)]
pub enum MachineEngineEvent {
    TurnStarted {
        machine_turn_id: u64,
        queue_item_id: Option<String>,
        entry: Option<UserEntry>,
    },
    TurnSettled {
        outcome: MachineTurnOutcome,
        error: Option<Value>,
        produced: Vec<HistoryMessage>,
    },
    StepStarted {
        step: u64,
        recovery: Option<LlmRecoveryRecord>,
    },
    StepCompleted {
        step: u64,
        entry: AssistantEntry,
        usage: TokenUsage,
        finish: Option<FinishInfo>,
        message_id: Option<String>,
        model: Option<String>,
        timing: Option<ModelRequestTiming>,
        trace_id: Option<String>,
    },
    StepFailed {
        step: u64,
        error: LlmErrorMessage,
        raw_error: Option<Value>,
    },
    Delta {
        delta: MachineEngineDelta,
    },
    Retrying {
        step: u64,
        failed_attempt: u32,
        next_attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_name: String,
        error_message: String,
        status_code: Option<u16>,
        raw_error: Option<Value>,
    },
    Recovering {
        step: u64,
        strategy: String,
        action: String,
        error_name: String,
        error_message: String,
        status_code: Option<u16>,
    },
    ToolStarted {
        tool_call_id: String,
        name: String,
        args: Value,
        display: Option<ToolInputDisplay>,
    },
    ToolUpdate {
        tool_call_id: String,
        update: ToolUpdate,
    },
    ToolAsync {
        tool_call_id: String,
        text: String,
    },
    ToolDone {
        tool_call_id: String,
        result: MachineToolResult,
    },
    ToolFailed {
        tool_call_id: String,
        error: Value,
    },
    ToolAborted {
        tool_call_id: String,
    },
    ToolBatchFailed {
        error: Value,
    },
    RemindersConsumed {
        reminders: Vec<HistoryMessage>,
    },
    PromptBlocked {
        queue_item_id: Option<String>,
        entry: Option<UserEntry>,
    },
    PromptGateFailed {
        queue_item_id: Option<String>,
        error: Value,
        entry: Option<UserEntry>,
    },
    PromptSteered {
        queue_item_ids: Vec<String>,
        entries: Vec<UserEntry>,
    },
    Aborting,
}

pub type MachineEngineEventHandler = Arc<dyn Fn(MachineEngineEvent) + Send + Sync>;
pub type MachineRequesterGate = Arc<
    dyn Fn(AbortSignal) -> BoxFuture<'static, Result<MachineRequesterGateDecision, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct CreateMachineEngineOptions {
    pub model: LlmModel,
    pub system_prompt: Option<String>,
    pub llm_requester: Arc<dyn IAgentLLMRequesterService>,
    pub tool_executor: Arc<dyn IAgentToolExecutorService>,
    pub tool_infos: Arc<dyn Fn() -> Vec<ToolInfo> + Send + Sync>,
    pub max_attempts_per_step: Option<u32>,
    pub recovery: Option<Arc<dyn LlmRecovery>>,
    pub abort_timeout_ms: Option<u64>,
    pub initial_turn_id: Option<u64>,
    pub journal: Option<Arc<dyn SyncStoreJournal>>,
    pub trace: Option<Arc<dyn Fn() -> Option<LLMRequestTrace> + Send + Sync>>,
    pub source: Option<Arc<dyn Fn() -> Option<AgentLLMRequestSource> + Send + Sync>>,
    pub tool_turn_id: Option<Arc<dyn Fn() -> Option<u64> + Send + Sync>>,
    pub steer_signal: Option<Arc<dyn Fn() -> Option<AbortSignal> + Send + Sync>>,
    pub gate: Option<MachineRequesterGate>,
    pub prompt_gate: Option<PromptGate>,
    pub on_trace: Option<Arc<dyn Fn(LLMRequestTrace) + Send + Sync>>,
    pub on_event: Option<MachineEngineEventHandler>,
    pub on_tool_result: Option<Arc<dyn Fn(String, AgentToolResult, u64) + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineEngineRetrySnapshot {
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineEngineToolCallSnapshot {
    pub tool_call_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachineEnginePhase {
    Running,
    ToolCall,
    Retrying,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineEngineTurnSnapshot {
    pub turn_id: u64,
    pub phase: MachineEnginePhase,
    pub step: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<MachineEngineRetrySnapshot>,
    pub active_tool_calls: Vec<MachineEngineToolCallSnapshot>,
}

#[derive(Clone)]
pub struct MachineEngineSnapshot {
    pub running: bool,
    pub aborting: bool,
    pub waiting_for_background: bool,
    pub paused: bool,
    pub queue: Vec<UserEntry>,
    pub queue_length: usize,
    pub queue_ids: Vec<Option<String>>,
    pub notification_count: usize,
    pub reminder_count: usize,
    pub background_count: usize,
    pub turn: Option<MachineEngineTurnSnapshot>,
}

pub type MachineEngineAttachRef = AgentMachineSelf;

pub static MACHINE_LOOP_MODEL: LazyLock<LlmModel> = LazyLock::new(|| LlmModel {
    provider: "agent-loop".to_string(),
    model: "agent-loop".to_string(),
    capability: UNKNOWN_CAPABILITY,
    ..Default::default()
});

#[derive(Clone)]
pub struct MachineEngineAttachBundle {
    pub store: Arc<AgentEventStore>,
    pub turn_logic: Arc<crate::human::agent::turn::TurnMachineLogic>,
    pub tool_logic: Arc<crate::human::tool::machine::ToolMachineLogic>,
    pub tools: Vec<ToolDefinition>,
    pub request: Option<LlmRequestConfig>,
    pub requester: Arc<MachineRequester>,
    pub machine_tools: Arc<MachineTools>,
    pub prompt_gate: Option<PromptGate>,
}

#[derive(Default)]
struct DeltaSplitter {
    calls_by_index: HashMap<Option<StreamIndex>, (String, String)>,
}

impl DeltaSplitter {
    fn split(&mut self, part: StreamedMessagePart) -> Option<MachineEngineDelta> {
        match part {
            StreamedMessagePart::Content(ContentPart::Text { text, .. }) => {
                Some(MachineEngineDelta::Assistant { delta: text })
            }
            StreamedMessagePart::Content(ContentPart::Think {
                think,
                encrypted,
                details_index,
                hidden,
                reasoning_key,
            }) => Some(MachineEngineDelta::Thinking {
                delta: think,
                encrypted,
                details_index,
                hidden,
                reasoning_key,
            }),
            StreamedMessagePart::Content(_) => None,
            StreamedMessagePart::ToolCall(call) => {
                self.calls_by_index.insert(
                    call._stream_index.clone(),
                    (call.id.clone(), call.name.clone()),
                );
                Some(MachineEngineDelta::ToolCall {
                    tool_call_id: call.id,
                    name: call.name,
                    arguments_part: call.arguments,
                    started: Some(true),
                })
            }
            StreamedMessagePart::ToolCallPart(part) => {
                let arguments = part.arguments_part?;
                let (id, name) = self.calls_by_index.get(&part.index)?.clone();
                Some(MachineEngineDelta::ToolCall {
                    tool_call_id: id,
                    name,
                    arguments_part: Some(arguments),
                    started: None,
                })
            }
        }
    }
}

struct CurrentCredentialProvider {
    service: Arc<dyn IAgentLLMRequesterService>,
    source: Option<Arc<dyn Fn() -> Option<AgentLLMRequestSource> + Send + Sync>>,
}

impl CurrentCredentialProvider {
    fn current(&self) -> Option<Arc<dyn LlmCredentialProvider>> {
        match self.source.as_ref().and_then(|source| source()) {
            Some(AgentLLMRequestSource::Turn { turn_id, .. }) => {
                self.service.credential_provider_for_turn(turn_id)
            }
            _ => self.service.current_credential_provider(),
        }
    }
}

#[async_trait]
impl LlmCredentialProvider for CurrentCredentialProvider {
    async fn resolve(&self) -> Result<Option<LlmCredential>, WireAsyncError> {
        match self.current() {
            Some(provider) => provider.resolve().await,
            None => Ok(None),
        }
    }

    fn can_recover(&self, error: &WireAsyncError) -> bool {
        self.current()
            .is_some_and(|provider| provider.can_recover(error))
    }

    fn invalidate(&self) {
        if let Some(provider) = self.current() {
            provider.invalidate();
        }
    }
}

struct CompositeRecovery {
    fallback: Option<Arc<dyn LlmRecovery>>,
}

impl LlmRecovery for CompositeRecovery {
    fn propose(&self, ctx: LlmRecoveryContext) -> Option<LlmRecoveryProposalRecord> {
        let fallback_ctx = LlmRecoveryContext {
            error: ctx.error.clone(),
            messages: ctx.messages.clone(),
            applied_recoveries: ctx.applied_recoveries.clone(),
            credential_provider: ctx.credential_provider.clone(),
        };
        CredentialsRecovery.propose(ctx).or_else(|| {
            self.fallback
                .as_ref()
                .and_then(|recovery| recovery.propose(fallback_ctx))
        })
    }
}

pub fn machine_engine_attach_bundle(
    options: CreateMachineEngineOptions,
) -> MachineEngineAttachBundle {
    let requester = Arc::new(create_machine_requester(
        options.llm_requester.clone(),
        super::requester::CreateMachineRequesterOptions {
            source: options.source.clone(),
            gate: options.gate.clone(),
            on_trace: options.on_trace.clone(),
        },
    ));
    let publish = options.on_event.clone();
    let machine_tools = Arc::new(create_machine_tools(
        super::tools::CreateMachineToolsOptions {
            tool_executor: options.tool_executor.clone(),
            tool_infos: options.tool_infos.clone(),
            turn_id: {
                let callback = options.tool_turn_id.clone();
                Arc::new(move || callback.as_ref().and_then(|callback| callback()).unwrap_or(0))
            },
            steer_signal: options.steer_signal.clone(),
            trace: options.trace.clone(),
            on_tool_call: Arc::new(move |payload: MachineToolCallPayload| {
                if let Some(publish) = &publish {
                    publish(MachineEngineEvent::ToolStarted {
                        tool_call_id: payload.tool_call_id,
                        name: payload.name,
                        args: payload.args,
                        display: payload.display,
                    });
                }
            }),
            on_tool_result: options.on_tool_result.clone(),
            on_batch_error: {
                let publish = options.on_event.clone();
                Arc::new(move |error| {
                    if let Some(publish) = &publish {
                        publish(MachineEngineEvent::ToolBatchFailed { error });
                    }
                })
            },
        },
    ));
    let credential_provider: Arc<dyn LlmCredentialProvider> = Arc::new(CurrentCredentialProvider {
        service: options.llm_requester.clone(),
        source: options.source.clone(),
    });
    let initial_turn_id = options.initial_turn_id.unwrap_or(0);
    let journal = engine_journal(options.journal.clone(), initial_turn_id);
    let store = create_event_store_sync(SyncEventStoreOptions {
        journal,
        slices: agent_slices(),
        drain_limit: None,
        on_error: None,
        mutable_replay: Some(true),
    })
    .unwrap_or_else(|error| panic!("failed to create machine event store: {error}"));
    machine_tools.sync();
    let turn_logic = Arc::new(create_turn_machine(
        requester.requester.clone(),
        Some(crate::human::agent::turn::CreateTurnMachineOptions {
            recovery: Some(Arc::new(CompositeRecovery {
                fallback: options.recovery.clone(),
            })),
            retry: Some(LlmRetryOptions {
                max_attempts_per_step: options.max_attempts_per_step,
                infinite_retry: None,
            }),
            abort_grace_ms: None,
            message_resolvers: Vec::new(),
            on_before_step: None,
        }),
    ));
    let tool_logic = Arc::new(create_tool_machine(machine_tools.executor.clone()));
    MachineEngineAttachBundle {
        store,
        turn_logic,
        tool_logic,
        tools: machine_tools.tool_definitions(),
        request: Some(LlmRequestConfig {
            model: options.model,
            system_prompt: options.system_prompt,
            credential_provider: Some(credential_provider),
            ..Default::default()
        }),
        requester,
        machine_tools,
        prompt_gate: options.prompt_gate,
    }
}

#[derive(Default)]
struct EngineState {
    current_step: u64,
    splitter: DeltaSplitter,
    pending_failure: Option<(u64, LlmErrorMessage)>,
    last_retry: Option<MachineEngineRetrySnapshot>,
    turn_active: bool,
    phase: Option<MachineEnginePhase>,
    active_tool_calls: indexmap::IndexMap<String, String>,
}

pub struct MachineEngine {
    reference: MachineEngineAttachRef,
    bundle: MachineEngineAttachBundle,
    options: CreateMachineEngineOptions,
    state: Arc<Mutex<EngineState>>,
    subscriptions: Mutex<Vec<AgentSubscription>>,
    current_journal: Mutex<Option<Arc<dyn SyncStoreJournal>>>,
}

impl MachineEngine {
    fn publish(&self, event: MachineEngineEvent) {
        if let Some(publish) = &self.options.on_event {
            publish(event);
        }
    }

    pub fn submit(&self, entry: UserEntry) {
        self.bundle.machine_tools.sync();
        self.reference.send(AgentEvent::InputSubmit { entry });
    }

    pub fn steer<I: Into<MachineSteerTarget>>(&self, id: I) {
        self.bundle.machine_tools.sync();
        self.reference.send(AgentEvent::InputSteer {
            id: id.into().into_ids(),
        });
    }

    pub fn notify(&self, entry: UserEntry) {
        self.bundle.machine_tools.sync();
        self.reference.send(AgentEvent::InputNotify { entry });
    }

    pub fn remind(&self, key: String, entry: AgentReminderEntry) {
        self.bundle.machine_tools.sync();
        self.reference.send(AgentEvent::InputRemind { key, entry });
    }

    pub fn cancel_queue_item(&self, id: String) {
        self.reference.send(AgentEvent::InputCancel { id });
    }

    pub fn abort(&self, reason: Option<Value>) {
        self.reference.send(AgentEvent::InputAbort { reason });
    }

    pub fn pause(&self) {
        self.reference.send(AgentEvent::InputPause);
    }

    pub fn resume(&self) {
        self.reference.send(AgentEvent::InputContinue);
    }

    pub async fn reset_history(&self, history: &[HistoryMessage]) -> Result<(), WireAsyncError> {
        let mut events = history
            .iter()
            .map(|message| {
                MESSAGE_APPENDED.create(Map::from_iter([(
                    "message".to_string(),
                    serde_json::to_value(message).unwrap_or(Value::Null),
                )]))
            })
            .collect::<Vec<_>>();
        let next_turn_id = self.reference.get_snapshot().context.turn_id;
        if next_turn_id > 0 {
            events.push(turn_ended_record(next_turn_id - 1));
        }
        let seed = seed_records(&events);
        let current = self.current_journal.lock().clone();
        let next = match current {
            Some(current) => seeded_store_journal(current, seed.read_sync()),
            None => seed,
        };
        self.bundle.store.reset(next).await.map_err(Into::into)
    }

    pub async fn reset_journal(
        &self,
        journal: Arc<dyn SyncStoreJournal>,
    ) -> Result<(), WireAsyncError> {
        *self.current_journal.lock() = Some(journal.clone());
        self.bundle.store.reset(journal).await.map_err(Into::into)
    }

    pub fn stop(&self) {
        for subscription in self.subscriptions.lock().drain(..) {
            subscription.unsubscribe();
        }
    }

    pub fn snapshot(&self) -> MachineEngineSnapshot {
        let snapshot = self.reference.get_snapshot();
        let state = self.state.lock();
        let turn = if state.turn_active {
            let phase = match state.phase.unwrap_or(MachineEnginePhase::Running) {
                MachineEnginePhase::ToolCall if state.active_tool_calls.is_empty() => {
                    MachineEnginePhase::Running
                }
                phase => phase,
            };
            Some(MachineEngineTurnSnapshot {
                turn_id: snapshot.context.turn_id.max(0) as u64,
                phase,
                step: state.current_step,
                retry: (phase == MachineEnginePhase::Retrying)
                    .then(|| state.last_retry.clone())
                    .flatten()
                    .or_else(|| {
                        (phase == MachineEnginePhase::Retrying).then(|| MachineEngineRetrySnapshot {
                            failed_attempt: 0,
                            next_attempt: 1,
                            max_attempts: resolve_max_attempts(Some(&LlmRetryOptions {
                                max_attempts_per_step: self.options.max_attempts_per_step,
                                infinite_retry: None,
                            })),
                            delay_ms: 0,
                            error_name: None,
                            status_code: None,
                        })
                    }),
                active_tool_calls: state
                    .active_tool_calls
                    .iter()
                    .map(|(tool_call_id, name)| MachineEngineToolCallSnapshot {
                        tool_call_id: tool_call_id.clone(),
                        name: name.clone(),
                    })
                    .collect(),
            })
        } else {
            None
        };
        MachineEngineSnapshot {
            running: snapshot.value == "running",
            aborting: snapshot.value == "running" && snapshot.context.abort_reason.is_some(),
            waiting_for_background: snapshot.value == "idle" && !snapshot.context.background.is_empty(),
            paused: snapshot.context.paused,
            queue_length: snapshot.context.queue.len(),
            queue_ids: snapshot
                .context
                .queue
                .iter()
                .map(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()))
                .collect(),
            queue: snapshot.context.queue,
            notification_count: snapshot.context.notifications.len(),
            reminder_count: snapshot.context.reminders.len(),
            background_count: snapshot.context.background.len(),
            turn,
        }
    }

    pub fn current_step(&self) -> u64 {
        self.state.lock().current_step
    }

    pub fn last_finish(&self) -> Option<AgentLLMRequestFinish> {
        self.bundle.requester.last_finish()
    }

    pub fn handle_tool_progress(&self, tool_call_id: &str, update: AgentToolUpdate) {
        self.bundle
            .machine_tools
            .handle_agent_progress(tool_call_id, update);
    }
}

pub enum MachineSteerTarget {
    One(String),
    Many(Vec<String>),
}

impl MachineSteerTarget {
    fn into_ids(self) -> Vec<String> {
        match self {
            Self::One(id) => vec![id],
            Self::Many(ids) => ids,
        }
    }
}

impl From<String> for MachineSteerTarget {
    fn from(value: String) -> Self {
        Self::One(value)
    }
}

impl From<&str> for MachineSteerTarget {
    fn from(value: &str) -> Self {
        Self::One(value.to_string())
    }
}

impl From<Vec<String>> for MachineSteerTarget {
    fn from(value: Vec<String>) -> Self {
        Self::Many(value)
    }
}

impl From<&[String]> for MachineSteerTarget {
    fn from(value: &[String]) -> Self {
        Self::Many(value.to_vec())
    }
}

pub fn attach_machine_engine(
    reference: MachineEngineAttachRef,
    bundle: MachineEngineAttachBundle,
    options: CreateMachineEngineOptions,
) -> Arc<MachineEngine> {
    let state = Arc::new(Mutex::new(EngineState::default()));
    let engine = Arc::new(MachineEngine {
        reference: reference.clone(),
        current_journal: Mutex::new(options.journal.clone()),
        bundle,
        options,
        state: state.clone(),
        subscriptions: Mutex::new(Vec::new()),
    });
    let handler = {
        let engine = engine.clone();
        Arc::new(move |event: AgentEmitted| engine.project(event))
    };
    let mut subscriptions = engine.subscriptions.lock();
    for event_type in [
        "turn.started",
        "step.started",
        "llm.sent",
        "llm.streaming.part",
        "llm.retrying",
        "llm.recovering",
        "llm.done",
        "llm.failed.syntax",
        "llm.failed.remote",
        "tool.update",
        "tool.detached",
        "tool.done",
        "tool.failed",
        "tool.aborted",
        "turn.reminders_consumed",
        "prompt.blocked",
        "prompt.gate_failed",
        "prompt.steered",
        "turn.aborting",
        "turn.done",
        "turn.failed",
        "turn.aborted",
    ] {
        subscriptions.push(reference.on(event_type, handler.clone()));
    }
    drop(subscriptions);
    engine
}

impl MachineEngine {
    fn project(&self, event: AgentEmitted) {
        match event {
            AgentEmitted::TurnStarted {
                turn_id,
                queue_item_id,
                entry,
                ..
            } => {
                *self.state.lock() = EngineState {
                    turn_active: true,
                    phase: Some(MachineEnginePhase::Running),
                    ..Default::default()
                };
                self.publish(MachineEngineEvent::TurnStarted {
                    machine_turn_id: turn_id.max(0) as u64,
                    queue_item_id,
                    entry,
                });
            }
            AgentEmitted::StepStarted { step } => {
                self.state.lock().current_step = step;
            }
            AgentEmitted::Llm(TurnLlmEvent::Event(LlmEvent::Sent { recovery })) => {
                let step = {
                    let mut state = self.state.lock();
                    state.splitter = DeltaSplitter::default();
                    state.last_retry = None;
                    state.phase = Some(MachineEnginePhase::Running);
                    state.current_step
                };
                self.bundle.machine_tools.begin_batch(None);
                self.publish(MachineEngineEvent::StepStarted { step, recovery });
            }
            AgentEmitted::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                LlmRequestEvent::StreamingPart { part },
            ))) => {
                let delta = self.state.lock().splitter.split(part);
                if let Some(delta) = delta {
                    self.publish(MachineEngineEvent::Delta { delta });
                }
            }
            AgentEmitted::Llm(TurnLlmEvent::Event(LlmEvent::Retrying {
                failed_attempt,
                next_attempt,
                max_attempts,
                delay_ms,
                error_name,
                error_message,
                status_code,
            })) => {
                let step = {
                    let mut state = self.state.lock();
                    state.pending_failure = None;
                    state.last_retry = Some(MachineEngineRetrySnapshot {
                        failed_attempt,
                        next_attempt,
                        max_attempts,
                        delay_ms,
                        error_name: Some(error_name.clone()),
                        status_code,
                    });
                    state.phase = Some(MachineEnginePhase::Retrying);
                    state.current_step
                };
                self.publish(MachineEngineEvent::Retrying {
                    step,
                    failed_attempt,
                    next_attempt,
                    max_attempts,
                    delay_ms,
                    error_name,
                    error_message,
                    status_code,
                    raw_error: self.bundle.requester.last_error(),
                });
            }
            AgentEmitted::Llm(TurnLlmEvent::Event(LlmEvent::Recovering {
                strategy,
                action,
                error_name,
                error_message,
                status_code,
            })) => {
                let step = {
                    let mut state = self.state.lock();
                    state.pending_failure = None;
                    state.current_step
                };
                self.publish(MachineEngineEvent::Recovering {
                    step,
                    strategy,
                    action,
                    error_name,
                    error_message,
                    status_code,
                });
            }
            AgentEmitted::Llm(TurnLlmEvent::Done { entry }) => {
                let finish = self.bundle.requester.last_finish();
                let step = {
                    let mut state = self.state.lock();
                    state.pending_failure = None;
                    state.last_retry = None;
                    state.phase = Some(if entry.message.tool_calls.is_empty() {
                        MachineEnginePhase::Running
                    } else {
                        MachineEnginePhase::ToolCall
                    });
                    state.active_tool_calls = entry
                        .message
                        .tool_calls
                        .iter()
                        .map(|call| (call.id.clone(), call.name.clone()))
                        .collect();
                    state.current_step
                };
                self.bundle.machine_tools.begin_batch(Some(entry.message.tool_calls.clone()));
                let meta = entry.meta.clone();
                self.publish(MachineEngineEvent::StepCompleted {
                    step,
                    usage: finish
                        .as_ref()
                        .map(|finish| finish.usage.clone())
                        .or_else(|| meta.as_ref().and_then(|meta| meta.usage.clone()))
                        .unwrap_or_else(empty_usage),
                    finish: finish.as_ref().map(|finish| FinishInfo {
                        finish_reason: finish.provider_finish_reason,
                        raw_finish_reason: finish.raw_finish_reason.clone(),
                    }).or_else(|| meta.as_ref().and_then(|meta| meta.finish.clone())),
                    message_id: finish
                        .as_ref()
                        .and_then(|finish| finish.provider_message_id.clone())
                        .or_else(|| meta.as_ref().and_then(|meta| meta.message_id.clone())),
                    model: finish
                        .as_ref()
                        .and_then(|finish| finish.model.clone())
                        .or_else(|| meta.as_ref().and_then(|meta| meta.model.as_ref().map(|model| model.model.clone()))),
                    timing: finish.as_ref().and_then(|finish| finish.timing.clone()),
                    trace_id: finish.and_then(|finish| finish.trace_id),
                    entry,
                });
            }
            AgentEmitted::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                LlmRequestEvent::FailedSyntax { error },
            )))
            | AgentEmitted::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                LlmRequestEvent::FailedRemote { error, .. },
            ))) => {
                let step = self.state.lock().current_step;
                self.state.lock().pending_failure = Some((step, error));
            }
            AgentEmitted::Tool(ToolEvent::Update {
                tool_call_id,
                update,
            }) => self.publish(MachineEngineEvent::ToolUpdate {
                tool_call_id,
                update,
            }),
            AgentEmitted::Tool(ToolEvent::Detached { tool_call_id, text }) => {
                self.state
                    .lock()
                    .active_tool_calls
                    .shift_remove(&tool_call_id);
                self.publish(MachineEngineEvent::ToolAsync { tool_call_id, text });
            }
            AgentEmitted::Tool(ToolEvent::Done {
                tool_call_id,
                result,
            }) => {
                self.state.lock().active_tool_calls.shift_remove(&tool_call_id);
                self.publish(MachineEngineEvent::ToolDone {
                    tool_call_id,
                    result,
                });
            }
            AgentEmitted::Tool(ToolEvent::Failed {
                tool_call_id,
                error,
            }) => {
                self.state.lock().active_tool_calls.shift_remove(&tool_call_id);
                self.publish(MachineEngineEvent::ToolFailed {
                    tool_call_id,
                    error: Value::String(error),
                });
            }
            AgentEmitted::Tool(ToolEvent::Aborted { tool_call_id }) => {
                self.state.lock().active_tool_calls.shift_remove(&tool_call_id);
                self.publish(MachineEngineEvent::ToolAborted { tool_call_id });
            }
            AgentEmitted::TurnRemindersConsumed { reminders } => {
                self.publish(MachineEngineEvent::RemindersConsumed { reminders });
            }
            AgentEmitted::PromptBlocked {
                queue_item_id,
                entry,
            } => self.publish(MachineEngineEvent::PromptBlocked {
                queue_item_id,
                entry,
            }),
            AgentEmitted::PromptGateFailed {
                queue_item_id,
                error,
                entry,
            } => self.publish(MachineEngineEvent::PromptGateFailed {
                queue_item_id,
                error,
                entry,
            }),
            AgentEmitted::PromptSteered {
                queue_item_ids,
                entries,
            } => self.publish(MachineEngineEvent::PromptSteered {
                queue_item_ids,
                entries,
            }),
            AgentEmitted::TurnAborting => self.publish(MachineEngineEvent::Aborting),
            AgentEmitted::TurnDone { messages, .. } => {
                self.state.lock().turn_active = false;
                self.publish(MachineEngineEvent::TurnSettled {
                    outcome: MachineTurnOutcome::Done,
                    error: None,
                    produced: messages,
                });
            }
            AgentEmitted::TurnFailed {
                error, messages, ..
            } => {
                let failure = self.state.lock().pending_failure.clone();
                if let Some((step, failure_error)) = failure {
                    self.publish(MachineEngineEvent::StepFailed {
                        step,
                        error: failure_error,
                        raw_error: self.bundle.requester.last_error(),
                    });
                }
                self.state.lock().turn_active = false;
                self.publish(MachineEngineEvent::TurnSettled {
                    outcome: MachineTurnOutcome::Failed,
                    error: Some(error),
                    produced: messages,
                });
            }
            AgentEmitted::TurnAborted { messages, .. } => {
                self.state.lock().turn_active = false;
                self.publish(MachineEngineEvent::TurnSettled {
                    outcome: MachineTurnOutcome::Aborted,
                    error: None,
                    produced: messages,
                });
            }
            _ => {}
        }
    }
}

fn turn_ended_record(turn_id: i64) -> ExternalEvent {
    TURN_ENDED.create(Map::from_iter([
        ("turnId".to_string(), Value::from(turn_id)),
        ("outcome".to_string(), Value::String("done".to_string())),
    ]))
}

fn append_immediately(journal: &Arc<dyn SyncStoreJournal>, input: crate::human::store::types::AppendInput) {
    let mut future = Box::pin(journal.append(input));
    let waker = futures_util::task::noop_waker_ref();
    let mut context = Context::from_waker(waker);
    loop {
        match Future::poll(future.as_mut(), &mut context) {
            Poll::Ready(result) => {
                result.unwrap_or_else(|error| panic!("failed to seed machine journal: {error}"));
                return;
            }
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn seed_records(events: &[ExternalEvent]) -> Arc<dyn SyncStoreJournal> {
    let journal = memory_journal(None);
    for event in events {
        append_immediately(
            &journal,
            crate::human::store::types::AppendInput {
                type_: event
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                kind: "event".to_string(),
                data: Some(json!(event)),
            },
        );
    }
    journal
}

pub fn engine_journal(
    base: Option<Arc<dyn SyncStoreJournal>>,
    initial_turn_id: u64,
) -> Arc<dyn SyncStoreJournal> {
    let Some(base) = base else {
        if initial_turn_id == 0 {
            return memory_journal(None);
        }
        return seed_records(&[turn_ended_record(initial_turn_id as i64 - 1)]);
    };
    if initial_turn_id == 0 {
        return base;
    }
    let seed = seed_records(&[turn_ended_record(initial_turn_id as i64 - 1)]).read_sync();
    if base.read_sync().is_empty() {
        seeded_store_journal(base, seed)
    } else {
        appended_store_journal(base, seed)
    }
}
