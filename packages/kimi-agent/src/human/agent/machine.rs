use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::human::agent::errors::TurnInterruptReason;
use crate::human::agent::events::{MESSAGE_APPENDED, TURN_ENDED, TURN_STARTED};
use crate::human::agent::origin::{SteerMessage, merge_steer_messages};
use crate::human::agent::slices::{AgentStoreState, TurnIndexState};
use crate::human::agent::turn::{
    AssistantEntry, EntryMeta, HistoryMessage, SystemEntry, TurnLlmEvent, TurnOutput,
    TurnParentEvent, TurnSignal, TurnState, TurnToolEvent, UserEntry, UserMeta,
    create_system_entry, create_user_entry,
};
use crate::human::agent::wait_for::{AgentSnapshotSource, ToolActorRef, create_wait_for_tasks};
use crate::human::eventStore::eventStore::{Cause, EventStoreImpl};
use crate::human::eventStore::events::ExternalEvent;
use crate::human::llm::message::{ToolCall, UserMessage, create_user_message};
use crate::human::llm::requester::requester::LlmRequestConfig;
use crate::human::tool::executor::{ToolExecuteInput, ToolExecutor, ToolResult};
use crate::human::tool::machine::{
    ToolEvent, ToolInput, ToolMachineLogic, ToolOutput,
};
use crate::human::tool::tool::ToolDefinition;
use crate::human::utils::abort::{AbortScope, create_abort_scope, with_abort};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct AgentScopeHandle {
    dispose: Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>,
}

impl AgentScopeHandle {
    pub fn new(
        dispose: impl Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync + 'static,
    ) -> Self {
        Self {
            dispose: Arc::new(dispose),
        }
    }

    pub async fn dispose_async(&self) -> Result<(), WireAsyncError> {
        (self.dispose)().await
    }
}

#[derive(Clone)]
pub struct AgentInput {
    pub request: LlmRequestConfig,
    pub store: Option<Arc<EventStoreImpl>>,
    pub session: Option<Value>,
    pub scope_factory: ScopeFactory,
}

pub type ScopeFactory = Arc<
    dyn Fn(
            AgentMachineSelf,
            CancellationToken,
        ) -> BoxFuture<'static, Result<ScopeFactoryOutput, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub enum PromptGateVerdict {
    Bool(bool),
    Detailed { block: bool, message: Option<UserMessage> },
}

pub type PromptGate = Arc<
    dyn Fn(
            Option<String>,
            UserMessage,
        ) -> BoxFuture<'static, Result<PromptGateVerdict, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ScopeFactoryOutput {
    pub handle: Option<AgentScopeHandle>,
    pub store: Arc<EventStoreImpl>,
    pub turn_logic: Arc<crate::human::agent::turn::TurnMachineLogic>,
    pub tool_logic: Arc<ToolMachineLogic>,
    pub tools: Vec<ToolDefinition>,
    pub request: Option<LlmRequestConfig>,
    pub prompt_gate: Option<PromptGate>,
}

#[derive(Clone)]
pub enum AgentEvent {
    Llm(TurnLlmEvent),
    Tool(ToolEvent),
    InputSubmit { entry: UserEntry },
    InputNotify { entry: UserEntry },
    InputRemind { key: String, entry: AgentReminderEntry },
    InputSteer { id: Vec<String> },
    InputCancel { id: String },
    InputAbort { reason: Option<Value> },
    InputPause,
    InputContinue,
    InputClose,
    TurnSpawnTools { tool_calls: Vec<ToolCall> },
    TurnDrain,
    TurnRemindersConsumed { reminders: Vec<HistoryMessage> },
    StepStarted { step: u64 },
    StoreReady { state: AgentStoreState, branch: String },
    StoreChanged { state: AgentStoreState },
    StoreReset { state: AgentStoreState, branch: String },
    StoreError { error: Value },
    TurnDone { output: TurnOutput },
    TurnError { error: Value },
}

#[derive(Clone)]
pub enum AgentReminderEntry {
    System(SystemEntry),
    User(UserEntry),
}

#[derive(Clone)]
pub enum AgentEmitted {
    Llm(TurnLlmEvent),
    Tool(ToolEvent),
    TurnStarted {
        turn_id: i64,
        branch_id: String,
        queue_item_id: Option<String>,
        entry: Option<UserEntry>,
    },
    StepStarted {
        step: u64,
    },
    TurnAborting,
    TurnRemindersConsumed {
        reminders: Vec<HistoryMessage>,
    },
    TurnDone {
        messages: Vec<HistoryMessage>,
        branch_id: String,
    },
    TurnFailed {
        error: Value,
        messages: Vec<HistoryMessage>,
        interrupt_reason: TurnInterruptReason,
        branch_id: String,
    },
    TurnAborted {
        messages: Vec<HistoryMessage>,
        branch_id: String,
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
    ContextReset {
        branch_id: String,
    },
    AgentAttached,
    AgentFailed {
        error: Value,
    },
}

impl AgentEmitted {
    pub fn type_(&self) -> String {
        match self {
            Self::Llm(event) => turn_llm_event_type(event).to_string(),
            Self::Tool(event) => tool_event_type(event).to_string(),
            Self::TurnStarted { .. } => "turn.started".to_string(),
            Self::StepStarted { .. } => "step.started".to_string(),
            Self::TurnAborting => "turn.aborting".to_string(),
            Self::TurnRemindersConsumed { .. } => "turn.reminders_consumed".to_string(),
            Self::TurnDone { .. } => "turn.done".to_string(),
            Self::TurnFailed { .. } => "turn.failed".to_string(),
            Self::TurnAborted { .. } => "turn.aborted".to_string(),
            Self::PromptBlocked { .. } => "prompt.blocked".to_string(),
            Self::PromptGateFailed { .. } => "prompt.gate_failed".to_string(),
            Self::PromptSteered { .. } => "prompt.steered".to_string(),
            Self::ContextReset { .. } => "context.reset".to_string(),
            Self::AgentAttached => "agent.attached".to_string(),
            Self::AgentFailed { .. } => "agent.failed".to_string(),
        }
    }
}

fn turn_llm_event_type(event: &TurnLlmEvent) -> &'static str {
    match event {
        TurnLlmEvent::Done { .. } => "llm.done",
        TurnLlmEvent::Event(crate::human::llm::requester::actor::LlmEvent::Sent { .. }) => "llm.sent",
        TurnLlmEvent::Event(crate::human::llm::requester::actor::LlmEvent::Retrying { .. }) => "llm.retrying",
        TurnLlmEvent::Event(crate::human::llm::requester::actor::LlmEvent::Recovering { .. }) => "llm.recovering",
        TurnLlmEvent::Event(crate::human::llm::requester::actor::LlmEvent::Request(event)) => {
            use crate::human::llm::requester::requester::LlmRequestEvent;
            match event {
                LlmRequestEvent::Sent => "llm.sent",
                LlmRequestEvent::StreamingHeaders { .. } => "llm.streaming.headers",
                LlmRequestEvent::StreamingPart { .. } => "llm.streaming.part",
                LlmRequestEvent::StreamingUsage { .. } => "llm.streaming.usage",
                LlmRequestEvent::StreamingFinish { .. } => "llm.streaming.finish",
                LlmRequestEvent::StreamingMessageId { .. } => "llm.streaming.message_id",
                LlmRequestEvent::FailedSyntax { .. } => "llm.failed.syntax",
                LlmRequestEvent::FailedRemote { .. } => "llm.failed.remote",
                LlmRequestEvent::RequestRetrying => "llm.request.retrying",
                LlmRequestEvent::Done => "llm.done",
            }
        }
    }
}

fn tool_event_type(event: &ToolEvent) -> &'static str {
    match event {
        ToolEvent::Update { .. } => "tool.update",
        ToolEvent::Detached { .. } => "tool.detached",
        ToolEvent::Done { .. } => "tool.done",
        ToolEvent::Failed { .. } => "tool.failed",
        ToolEvent::Aborted { .. } => "tool.aborted",
        ToolEvent::Abort => "tool.abort",
    }
}

#[derive(Clone)]
pub struct ToolEntry {
    pub tool_call: ToolCall,
    pub scope: AbortScope,
    pub ref_: ToolActorRef,
}

#[derive(Clone)]
pub struct AgentMachineContext {
    pub input: AgentInput,
    pub request: LlmRequestConfig,
    pub store: Option<Arc<EventStoreImpl>>,
    pub handle: Option<AgentScopeHandle>,
    pub turn_logic: Option<Arc<crate::human::agent::turn::TurnMachineLogic>>,
    pub tool_logic: Option<Arc<ToolMachineLogic>>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub prompt_gate: Option<PromptGate>,
    pub messages: Vec<HistoryMessage>,
    pub turn_tools: IndexMap<String, ToolEntry>,
    pub background: IndexMap<String, ToolEntry>,
    pub scope: AbortScope,
    pub notifications: Vec<UserEntry>,
    pub reminders: Vec<HistoryMessage>,
    pub queue: Vec<UserEntry>,
    pub turn_id: i64,
    pub active_turn_id: Option<i64>,
    pub branch_id: String,
    pub drained_id: Option<String>,
    pub drained_entry: Option<UserEntry>,
    pub paused: bool,
    pub abort_reason: Option<Value>,
}

fn completion_notification(tool_call: &ToolCall, output: &ToolOutput) -> UserEntry {
    match output {
        ToolOutput::Failed { error } => create_user_entry(
            create_user_message(&format!(
                "[async tool failed] {} (tool_call_id={})\n{}",
                tool_call.name, tool_call.id, error
            )),
            UserMeta {
                source: Some("async-tool".to_string()),
                ..Default::default()
            },
        ),
        ToolOutput::Aborted => create_user_entry(
            create_user_message(&format!(
                "[async tool aborted] {} (tool_call_id={})",
                tool_call.name, tool_call.id
            )),
            UserMeta {
                source: Some("async-tool".to_string()),
                ..Default::default()
            },
        ),
        ToolOutput::Succeeded { result } => {
            let mut content = vec![crate::human::llm::message::ContentPart::Text {
                text: format!(
                    "[async tool completed] {} (tool_call_id={})",
                    tool_call.name, tool_call.id
                ),
                meta: None,
            }];
            content.extend(result.content.clone());
            create_user_entry(
                UserMessage {
                    role: crate::human::llm::message::Role::User,
                    content,
                },
                UserMeta {
                    source: Some("async-tool".to_string()),
                    ..Default::default()
                },
            )
        }
    }
}

fn history_ends_mid_tool_chain(messages: &[HistoryMessage]) -> bool {
    match messages.last() {
        Some(HistoryMessage::Tool(_)) => true,
        Some(HistoryMessage::Assistant(entry)) => !entry.message.tool_calls.is_empty(),
        _ => false,
    }
}

fn history_meta_key(entry: &HistoryMessage) -> Option<&str> {
    match entry {
        HistoryMessage::System(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::User(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::Assistant(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::Tool(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
    }
}

fn decode_state<T: DeserializeOwned>(state: &AgentStoreState, key: &str) -> T
where
    T: Default,
{
    state
        .get(key)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn history_event(message: HistoryMessage) -> ExternalEvent {
    MESSAGE_APPENDED.create(Map::from_iter([(
        "message".to_string(),
        serde_json::to_value(message).unwrap(),
    )]))
}

fn interrupt_reason_value(error: &Value) -> TurnInterruptReason {
    if error.get("code").and_then(Value::as_str)
        == Some(crate::human::agent::errors::LOOP_MAX_STEPS_EXCEEDED_ERROR_CODE)
    {
        TurnInterruptReason::MaxSteps
    } else {
        TurnInterruptReason::Error
    }
}

fn error_message(error: &Value) -> String {
    match error {
        Value::String(message) => message.clone(),
        Value::Object(object)
            if object.get("name").and_then(Value::as_str) == Some("MaxStepsExceededError")
                && object.get("message").and_then(Value::as_str).is_some() =>
        {
            format!(
                "MaxStepsExceededError: {}",
                object.get("message").and_then(Value::as_str).unwrap()
            )
        }
        Value::Object(_) => "[object Object]".to_string(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                _ => error_message(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Null => "null".to_string(),
        _ => error.to_string(),
    }
}

struct DispatchExecutor {
    by_name: HashMap<String, ToolDefinition>,
}

#[async_trait]
impl ToolExecutor for DispatchExecutor {
    async fn execute(&self, input: ToolExecuteInput) -> Result<ToolResult, WireAsyncError> {
        let Some(tool) = self.by_name.get(&input.tool_call.name) else {
            return Ok(ToolResult {
                content: vec![crate::human::llm::message::ContentPart::Text {
                    text: format!("unknown tool: {}", input.tool_call.name),
                    meta: None,
                }],
                is_error: Some(true),
            });
        };
        tool.execute(input).await
    }
}

pub fn dispatch_tools(tools: Vec<ToolDefinition>) -> Result<Arc<dyn ToolExecutor>, WireAsyncError> {
    let mut by_name = HashMap::new();
    for tool in tools {
        if by_name.contains_key(&tool.name) {
            return Err(format!("duplicate tool name: '{}'", tool.name).into());
        }
        by_name.insert(tool.name.clone(), tool);
    }
    Ok(Arc::new(DispatchExecutor { by_name }))
}

#[derive(Clone, Default)]
pub struct CreateAgentMachineOptions {
    pub abort_timeout_ms: Option<u64>,
    pub max_steps_per_turn: Option<u64>,
}

pub struct AgentMachineLogic {
    options: CreateAgentMachineOptions,
}

impl AgentMachineLogic {
    pub fn start(
        &self,
        input: AgentInput,
        parent: Arc<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> Arc<AgentActor> {
        let actor = Arc::new(AgentActor {
            options: self.options.clone(),
            context: Mutex::new(AgentMachineContext {
                request: input.request.clone(),
                input,
                store: None,
                handle: None,
                turn_logic: None,
                tool_logic: None,
                tools: None,
                prompt_gate: None,
                messages: Vec::new(),
                turn_tools: IndexMap::new(),
                background: IndexMap::new(),
                scope: create_abort_scope(),
                notifications: Vec::new(),
                reminders: Vec::new(),
                queue: Vec::new(),
                turn_id: 0,
                active_turn_id: None,
                branch_id: "main".to_string(),
                drained_id: None,
                drained_entry: None,
                paused: false,
                abort_reason: None,
            }),
            state: Mutex::new(AgentState::Linking),
            parent,
            listeners: Mutex::new(HashMap::new()),
            next_listener_id: AtomicU64::new(0),
            turn: Mutex::new(None),
            store_unsubscribe: Mutex::new(None),
            append_lock: Arc::new(tokio::sync::Mutex::new(())),
        });
        actor.link();
        actor
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentState {
    Linking,
    Restoring,
    IdleReady,
    IdleWaiting,
    Gating,
    RunningActive,
    RunningAborting,
    Closing,
    Disposed,
}

pub struct AgentSnapshot {
    pub context: AgentMachineContext,
    pub value: String,
    pub status: String,
}

pub struct AgentSubscription {
    unsubscribe: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl AgentSubscription {
    pub fn unsubscribe(&self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
    }
}

#[derive(Clone)]
pub struct AgentMachineSelf {
    actor: Arc<AgentActor>,
}

impl AgentMachineSelf {
    pub fn send(&self, event: AgentEvent) {
        self.actor.send(event);
    }

    pub fn get_snapshot(&self) -> AgentSnapshot {
        self.actor.get_snapshot()
    }

    pub fn on(
        &self,
        type_: &str,
        handler: Arc<dyn Fn(AgentEmitted) + Send + Sync>,
    ) -> AgentSubscription {
        self.actor.on(type_, handler)
    }
}

impl AgentSnapshotSource for AgentMachineSelf {
    fn snapshot_background(&self) -> IndexMap<String, ToolActorRef> {
        self.actor
            .context
            .lock()
            .background
            .iter()
            .map(|(id, entry)| (id.clone(), entry.ref_.clone()))
            .collect()
    }
}

type AgentListener = Arc<dyn Fn(AgentEmitted) + Send + Sync>;

pub struct AgentActor {
    options: CreateAgentMachineOptions,
    context: Mutex<AgentMachineContext>,
    state: Mutex<AgentState>,
    parent: Arc<dyn Fn(AgentEvent) + Send + Sync>,
    listeners: Mutex<HashMap<u64, (String, AgentListener)>>,
    next_listener_id: AtomicU64,
    turn: Mutex<Option<Arc<crate::human::agent::turn::TurnActor>>>,
    store_unsubscribe: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    append_lock: Arc<tokio::sync::Mutex<()>>,
}

impl AgentActor {
    pub fn send(self: &Arc<Self>, event: AgentEvent) {
        if *self.state.lock() == AgentState::Disposed {
            return;
        }
        match event {
            AgentEvent::InputClose => self.close(),
            AgentEvent::InputSubmit { entry } => {
                let meta = entry.meta.clone();
                let entry = create_user_entry(
                    entry.message,
                    user_meta_with_default(meta, "input"),
                );
                self.context.lock().queue.push(entry);
                self.evaluate_idle();
            }
            AgentEvent::InputNotify { entry } => {
                let entry = create_user_entry(
                    entry.message,
                    user_meta_with_default(entry.meta, "notify"),
                );
                self.context.lock().notifications.push(entry);
                self.evaluate_idle();
            }
            AgentEvent::InputRemind { key, entry } => {
                let mut context = self.context.lock();
                context.reminders.retain(|entry| history_meta_key(entry) != Some(&key));
                let reminder = match entry {
                    AgentReminderEntry::System(entry) => {
                        let mut meta = entry.meta.unwrap_or_default();
                        if meta.source.is_none() {
                            meta.source = Some("reminder".to_string());
                        }
                        if meta.key.is_none() {
                            meta.key = Some(key.clone());
                        }
                        HistoryMessage::System(create_system_entry(entry.message, meta))
                    }
                    AgentReminderEntry::User(entry) => {
                        let mut meta = entry.meta.unwrap_or_default();
                        if meta.source.is_none() {
                            meta.source = Some("reminder".to_string());
                        }
                        if meta.key.is_none() {
                            meta.key = Some(key.clone());
                        }
                        HistoryMessage::User(create_user_entry(entry.message, meta))
                    }
                };
                context.reminders.push(reminder);
            }
            AgentEvent::InputSteer { id } => self.steer(id),
            AgentEvent::InputCancel { id } => {
                self.context
                    .lock()
                    .queue
                    .retain(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.as_deref()) != Some(id.as_str()));
            }
            AgentEvent::InputAbort { reason } => self.abort(reason),
            AgentEvent::InputPause => {
                self.context.lock().paused = true;
                if let Some(turn) = self.turn.lock().clone() {
                    turn.send(crate::human::agent::turn::TurnEvent::Pause);
                }
            }
            AgentEvent::InputContinue => self.continue_(),
            AgentEvent::StoreReady { state, branch } => self.restore(state, branch),
            AgentEvent::StoreReset { state, branch } => self.reset(state, branch),
            AgentEvent::StoreChanged { .. } => {}
            AgentEvent::StoreError { error } => self.forward(AgentEvent::StoreError { error }),
            AgentEvent::Tool(event) => self.handle_tool(event),
            AgentEvent::TurnSpawnTools { tool_calls } => self.spawn_turn_tools(tool_calls),
            AgentEvent::TurnDrain => {
                if matches!(
                    *self.state.lock(),
                    AgentState::RunningActive | AgentState::RunningAborting
                ) {
                    self.drain_turn();
                }
            }
            AgentEvent::TurnRemindersConsumed { reminders } => {
                if matches!(
                    *self.state.lock(),
                    AgentState::RunningActive | AgentState::RunningAborting
                ) {
                    self.emit(AgentEmitted::TurnRemindersConsumed {
                        reminders: reminders.clone(),
                    });
                    self.forward(AgentEvent::TurnRemindersConsumed { reminders });
                }
            }
            AgentEvent::StepStarted { step } => {
                if matches!(
                    *self.state.lock(),
                    AgentState::RunningActive | AgentState::RunningAborting
                ) {
                    self.emit(AgentEmitted::StepStarted { step });
                    self.forward(AgentEvent::StepStarted { step });
                }
            }
            AgentEvent::Llm(event) => {
                if matches!(
                    *self.state.lock(),
                    AgentState::RunningActive | AgentState::RunningAborting
                ) && matches!(
                    turn_llm_event_type(&event),
                    "llm.sent"
                        | "llm.streaming.headers"
                        | "llm.streaming.part"
                        | "llm.streaming.usage"
                        | "llm.streaming.finish"
                        | "llm.streaming.message_id"
                        | "llm.done"
                        | "llm.failed.syntax"
                        | "llm.failed.remote"
                        | "llm.retrying"
                        | "llm.recovering"
                ) {
                    self.emit(AgentEmitted::Llm(event.clone()));
                    self.forward(AgentEvent::Llm(event));
                }
            }
            AgentEvent::TurnDone { output } => self.finish_turn(output),
            AgentEvent::TurnError { error } => self.fail_turn(error),
        }
    }

    pub fn get_snapshot(&self) -> AgentSnapshot {
        let state = *self.state.lock();
        AgentSnapshot {
            context: self.context.lock().clone(),
            value: match state {
                AgentState::Linking => "linking",
                AgentState::Restoring => "restoring",
                AgentState::IdleReady | AgentState::IdleWaiting | AgentState::Gating => "idle",
                AgentState::RunningActive | AgentState::RunningAborting => "running",
                AgentState::Closing => "closing",
                AgentState::Disposed => "disposed",
            }
            .to_string(),
            status: if state == AgentState::Disposed {
                "done"
            } else {
                "active"
            }
            .to_string(),
        }
    }

    pub fn on(
        self: &Arc<Self>,
        type_: &str,
        handler: AgentListener,
    ) -> AgentSubscription {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, (type_.to_string(), handler));
        let weak = Arc::downgrade(self);
        AgentSubscription {
            unsubscribe: Mutex::new(Some(Box::new(move || {
                if let Some(actor) = Weak::upgrade(&weak) {
                    actor.listeners.lock().remove(&id);
                }
            }))),
        }
    }

    fn machine_self(self: &Arc<Self>) -> AgentMachineSelf {
        AgentMachineSelf {
            actor: self.clone(),
        }
    }

    fn emit(&self, event: AgentEmitted) {
        let type_ = event.type_();
        for (wanted, listener) in self.listeners.lock().values() {
            if wanted == &type_ {
                listener(event.clone());
            }
        }
    }

    fn forward(&self, event: AgentEvent) {
        (self.parent)(event);
    }

    fn link(self: &Arc<Self>) {
        let this = self.clone();
        let self_ = self.machine_self();
        let (factory, signal) = {
            let context = self.context.lock();
            (context.input.scope_factory.clone(), context.scope.signal.clone())
        };
        tokio::spawn(async move {
            match factory(self_, signal).await {
                Ok(output) => this.attach(output),
                Err(error) => {
                    if *this.state.lock() != AgentState::Disposed {
                        *this.state.lock() = AgentState::Disposed;
                        this.emit(AgentEmitted::AgentFailed {
                            error: Value::String(error.to_string()),
                        });
                    }
                }
            }
        });
    }

    fn attach(self: &Arc<Self>, output: ScopeFactoryOutput) {
        if *self.state.lock() != AgentState::Linking {
            return;
        }
        {
            let mut context = self.context.lock();
            context.store = Some(output.store.clone());
            context.handle = output.handle;
            context.turn_logic = Some(output.turn_logic);
            context.tool_logic = Some(output.tool_logic);
            context.tools = Some(output.tools);
            if let Some(request) = output.request {
                context.request = request;
            }
            context.prompt_gate = output.prompt_gate;
        }
        *self.state.lock() = AgentState::Restoring;
        self.emit(AgentEmitted::AgentAttached);
        self.send(AgentEvent::StoreReady {
            state: output.store.get_state(),
            branch: output.store.ref_().branch,
        });
        let this = self.clone();
        let store = output.store.clone();
        let unsubscribe = output.store.subscribe(Arc::new(move |state, cause| match cause {
            Cause::Reset { .. } => this.send(AgentEvent::StoreReset {
                state,
                branch: store.ref_().branch,
            }),
            _ => this.send(AgentEvent::StoreChanged { state }),
        }));
        *self.store_unsubscribe.lock() = Some(unsubscribe);
    }

    fn restore(self: &Arc<Self>, state: AgentStoreState, branch: String) {
        if *self.state.lock() != AgentState::Restoring {
            return;
        }
        let history = decode_state::<Vec<HistoryMessage>>(&state, "history");
        let stored_notifications = decode_state::<Vec<UserEntry>>(&state, "notifications");
        let stored_reminders = decode_state::<Vec<HistoryMessage>>(&state, "reminders");
        let stored_queue = decode_state::<Vec<UserEntry>>(&state, "queue");
        let turn_index = decode_state::<TurnIndexState>(&state, "turnIndex");
        {
            let mut context = self.context.lock();
            context.messages = history;
            context.notifications = [stored_notifications, context.notifications.clone()].concat();
            context.reminders = [
                stored_reminders
                    .into_iter()
                    .filter(|entry| {
                        !context
                            .reminders
                            .iter()
                            .any(|local| history_meta_key(local) == history_meta_key(entry))
                    })
                    .collect(),
                context.reminders.clone(),
            ]
            .concat();
            context.queue = [stored_queue, context.queue.clone()].concat();
            context.turn_id = turn_index.next_turn_id;
            context.branch_id = branch;
        }
        *self.state.lock() = AgentState::IdleReady;
        self.evaluate_idle();
    }

    fn reset(self: &Arc<Self>, state: AgentStoreState, branch: String) {
        let was_running = matches!(
            *self.state.lock(),
            AgentState::RunningActive | AgentState::RunningAborting
        );
        let old_scope = self.context.lock().scope.clone();
        old_scope.abort();
        if was_running {
            self.turn.lock().take();
            self.abort_turn_tools();
            self.turn_tools_stop();
        }
        let turn_index = decode_state::<TurnIndexState>(&state, "turnIndex");
        {
            let mut context = self.context.lock();
            context.messages = decode_state(&state, "history");
            context.notifications = decode_state(&state, "notifications");
            context.reminders = decode_state(&state, "reminders");
            context.turn_tools.clear();
            context.background.clear();
            context.scope = create_abort_scope();
            context.turn_id = turn_index.next_turn_id;
            context.active_turn_id = None;
            context.branch_id = branch.clone();
        }
        *self.state.lock() = AgentState::IdleReady;
        self.emit(AgentEmitted::ContextReset {
            branch_id: branch.clone(),
        });
        self.forward(AgentEvent::StoreReset { state, branch });
        self.evaluate_idle();
    }

    fn evaluate_idle(self: &Arc<Self>) {
        if !matches!(*self.state.lock(), AgentState::IdleReady | AgentState::IdleWaiting) {
            return;
        }
        let (has_gate, has_pending, paused, has_background) = {
            let context = self.context.lock();
            (
                context.prompt_gate.is_some() && !context.queue.is_empty(),
                !context.notifications.is_empty() || !context.queue.is_empty(),
                context.paused,
                !context.background.is_empty(),
            )
        };
        if has_gate && !paused {
            self.start_gating();
        } else if has_pending && !paused {
            self.commit_pending_to_history();
            self.start_running();
        } else {
            *self.state.lock() = if has_background {
                AgentState::IdleWaiting
            } else {
                AgentState::IdleReady
            };
        }
    }

    fn start_gating(self: &Arc<Self>) {
        *self.state.lock() = AgentState::Gating;
        let (gate, head) = {
            let context = self.context.lock();
            (context.prompt_gate.clone().unwrap(), context.queue.first().cloned())
        };
        let Some(head) = head else {
            *self.state.lock() = AgentState::IdleReady;
            self.evaluate_idle();
            return;
        };
        let id = head.meta.as_ref().and_then(|meta| meta.prompt_id.clone());
        let this = self.clone();
        tokio::spawn(async move {
            let result = gate(id.clone(), head.message.clone()).await;
            if *this.state.lock() != AgentState::Gating {
                return;
            }
            let (paused, current_id) = {
                let context = this.context.lock();
                (
                    context.paused,
                    context.queue.first().and_then(|entry| {
                        entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone())
                    }),
                )
            };
            if paused || current_id != id {
                *this.state.lock() = AgentState::IdleReady;
                this.evaluate_idle();
                return;
            }
            match result {
                Err(error) => this.gate_failed(Value::String(error.to_string())),
                Ok(PromptGateVerdict::Detailed {
                    block: false,
                    message: Some(message),
                }) => {
                    this.context.lock().queue[0].message = message;
                    this.commit_pending_to_history();
                    this.start_running();
                }
                Ok(verdict) => {
                    let block = match verdict {
                        PromptGateVerdict::Bool(block) => block,
                        PromptGateVerdict::Detailed { block, .. } => block,
                    };
                    if block {
                        this.emit(AgentEmitted::PromptBlocked {
                            queue_item_id: id,
                            entry: Some(head),
                        });
                        this.context.lock().queue.remove(0);
                        *this.state.lock() = AgentState::IdleReady;
                        this.evaluate_idle();
                    } else {
                        this.commit_pending_to_history();
                        this.start_running();
                    }
                }
            }
        });
    }

    fn gate_failed(self: &Arc<Self>, error: Value) {
        let (id, entry) = {
            let context = self.context.lock();
            (
                context.queue.first().and_then(|entry| {
                    entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone())
                }),
                context.queue.first().cloned(),
            )
        };
        self.emit(AgentEmitted::PromptGateFailed {
            queue_item_id: id,
            error,
            entry,
        });
        if !self.context.lock().queue.is_empty() {
            self.context.lock().queue.remove(0);
        }
        *self.state.lock() = AgentState::IdleReady;
        self.evaluate_idle();
    }

    fn commit_pending_to_history(&self) {
        let (entries, head) = {
            let context = self.context.lock();
            (context.notifications.clone(), context.queue.first().cloned())
        };
        let events = entries
            .iter()
            .cloned()
            .map(HistoryMessage::User)
            .chain(head.clone().map(HistoryMessage::User))
            .map(history_event)
            .collect::<Vec<_>>();
        self.append_events(events);
        let mut context = self.context.lock();
        context.messages.extend(entries);
        if let Some(head) = &head {
            context.messages.push(HistoryMessage::User(head.clone()));
        }
        context.notifications.clear();
        if !context.queue.is_empty() {
            context.queue.remove(0);
        }
        context.drained_id = head
            .as_ref()
            .and_then(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()));
        context.drained_entry = head;
    }

    fn start_running(self: &Arc<Self>) {
        let (turn_id, branch_id, queue_item_id, entry, request, history, signal, logic) = {
            let mut context = self.context.lock();
            context.active_turn_id = Some(context.turn_id);
            context.abort_reason = None;
            let mut request = context.request.clone();
            request.tools = context.tools.as_ref().map(|tools| {
                tools
                    .iter()
                    .filter(|tool| tool.deferred != Some(true))
                    .map(ToolDefinition::description)
                    .collect()
            });
            (
                context.turn_id,
                context.branch_id.clone(),
                context.drained_id.clone(),
                context.drained_entry.clone(),
                request,
                context.messages.clone(),
                context.scope.signal.clone(),
                context.turn_logic.clone().unwrap(),
            )
        };
        *self.state.lock() = AgentState::RunningActive;
        self.emit(AgentEmitted::TurnStarted {
            turn_id,
            branch_id: branch_id.clone(),
            queue_item_id: queue_item_id.clone(),
            entry: entry.clone(),
        });
        let mut event = Map::from_iter([("turnId".to_string(), json!(turn_id))]);
        if let Some(queue_item_id) = queue_item_id {
            event.insert("queueItemId".to_string(), Value::String(queue_item_id));
        }
        self.append_events(vec![TURN_STARTED.create(event)]);
        let turn = logic.start(
            crate::human::agent::turn::TurnInput {
                request,
                history,
                max_steps: self.options.max_steps_per_turn,
                parent_signal: Some(signal),
            },
            Arc::new({
                let this = self.clone();
                move |event| this.handle_turn_event(event)
            }),
        );
        *self.turn.lock() = Some(turn.clone());
        turn.on_finish(Arc::new({
            let this = self.clone();
            move || {
                if let Some(turn) = this.turn.lock().clone()
                    && let Some(output) = turn.output()
                {
                    this.send(AgentEvent::TurnDone { output });
                }
            }
        }));
    }

    fn handle_turn_event(self: &Arc<Self>, event: TurnParentEvent) {
        match event {
            TurnParentEvent::Llm(event) => self.send(AgentEvent::Llm(event)),
            TurnParentEvent::Signal(TurnSignal::StepStarted { step }) => {
                self.send(AgentEvent::StepStarted { step });
            }
            TurnParentEvent::Signal(TurnSignal::SpawnTools { tool_calls }) => {
                self.send(AgentEvent::TurnSpawnTools { tool_calls });
            }
            TurnParentEvent::Signal(TurnSignal::Drain) => self.send(AgentEvent::TurnDrain),
            TurnParentEvent::Signal(TurnSignal::RemindersConsumed { reminders }) => {
                self.send(AgentEvent::TurnRemindersConsumed { reminders });
            }
        }
    }

    fn finish_turn(self: &Arc<Self>, output: TurnOutput) {
        if !matches!(*self.state.lock(), AgentState::RunningActive | AgentState::RunningAborting) {
            return;
        }
        let produced = match &output {
            TurnOutput::Done { produced }
            | TurnOutput::Failed { produced, .. }
            | TurnOutput::Aborted { produced } => produced.clone(),
        };
        self.context.lock().messages.extend(produced.clone());
        let (messages, branch_id, turn_id) = {
            let context = self.context.lock();
            (
                context.messages.clone(),
                context.branch_id.clone(),
                context.active_turn_id.unwrap_or(context.turn_id),
            )
        };
        let (outcome, error) = match &output {
            TurnOutput::Done { .. } => {
                self.emit(AgentEmitted::TurnDone {
                    messages,
                    branch_id,
                });
                ("done", None)
            }
            TurnOutput::Aborted { .. } => {
                self.emit(AgentEmitted::TurnAborted {
                    messages,
                    branch_id,
                });
                ("aborted", None)
            }
            TurnOutput::Failed { error, .. } => {
                self.emit(AgentEmitted::TurnFailed {
                    error: error.clone(),
                    messages,
                    interrupt_reason: interrupt_reason_value(error),
                    branch_id,
                });
                ("failed", Some(error_message(error)))
            }
        };
        let mut events = produced.into_iter().map(history_event).collect::<Vec<_>>();
        let mut event = Map::from_iter([
            ("turnId".to_string(), json!(turn_id)),
            ("outcome".to_string(), json!(outcome)),
        ]);
        if let Some(error) = error {
            event.insert("errorMessage".to_string(), Value::String(error));
        }
        events.push(TURN_ENDED.create(event));
        self.append_events(events);
        self.exit_running();
    }

    fn fail_turn(self: &Arc<Self>, error: Value) {
        if !matches!(*self.state.lock(), AgentState::RunningActive | AgentState::RunningAborting) {
            return;
        }
        let (messages, branch_id, turn_id) = {
            let context = self.context.lock();
            (
                context.messages.clone(),
                context.branch_id.clone(),
                context.active_turn_id.unwrap_or(context.turn_id),
            )
        };
        self.emit(AgentEmitted::TurnFailed {
            error: error.clone(),
            messages,
            interrupt_reason: interrupt_reason_value(&error),
            branch_id,
        });
        self.append_events(vec![TURN_ENDED.create(Map::from_iter([
            ("turnId".to_string(), json!(turn_id)),
            ("outcome".to_string(), json!("failed")),
            ("errorMessage".to_string(), json!(error_message(&error))),
        ]))]);
        self.exit_running();
    }

    fn exit_running(self: &Arc<Self>) {
        self.abort_turn_tools();
        self.turn_tools_stop();
        *self.turn.lock() = None;
        self.context.lock().turn_id += 1;
        *self.state.lock() = AgentState::IdleReady;
        self.evaluate_idle();
    }

    fn drain_turn(&self) {
        let messages = {
            let context = self.context.lock();
            [context.notifications.clone(), context.reminders.clone()].concat()
        };
        if let Some(turn) = self.turn.lock().clone() {
            turn.send(crate::human::agent::turn::TurnEvent::Notify {
                messages: messages.clone(),
            });
        }
        if !messages.is_empty() {
            let mut context = self.context.lock();
            context.notifications.clear();
            context.reminders.clear();
        }
    }

    fn steer(self: &Arc<Self>, ids: Vec<String>) {
        let steered = {
            let context = self.context.lock();
            context
                .queue
                .iter()
                .filter(|entry| {
                    entry
                        .meta
                        .as_ref()
                        .and_then(|meta| meta.prompt_id.as_ref())
                        .is_some_and(|id| ids.contains(id))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if steered.is_empty() {
            return;
        }
        let merged = merge_steer_messages(
            &steered
                .iter()
                .map(|entry| SteerMessage {
                    content: entry.message.content.clone(),
                    origin: entry.meta.as_ref().and_then(|meta| meta.origin.clone()),
                })
                .collect::<Vec<_>>(),
        );
        let prompt_id = if steered.len() == 1 {
            steered[0]
                .meta
                .as_ref()
                .and_then(|meta| meta.prompt_id.clone())
        } else {
            None
        };
        {
            let mut context = self.context.lock();
            context.queue.retain(|entry| {
                !entry
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.prompt_id.as_ref())
                    .is_some_and(|id| ids.contains(id))
            });
            let mut origin = merged.origin;
            origin.in_turn = Some(true);
            context.notifications.push(create_user_entry(
                UserMessage {
                    role: crate::human::llm::message::Role::User,
                    content: merged.content,
                },
                UserMeta {
                    source: Some("input".to_string()),
                    prompt_id: prompt_id.clone(),
                    user_message_id: prompt_id,
                    origin: Some(origin),
                    ..Default::default()
                },
            ));
        }
        self.emit(AgentEmitted::PromptSteered {
            queue_item_ids: steered
                .iter()
                .filter_map(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()))
                .collect(),
            entries: steered,
        });
        if *self.state.lock() == AgentState::RunningActive
            && let Some(turn) = self.turn.lock().clone()
        {
            turn.send(crate::human::agent::turn::TurnEvent::Steer);
        }
        self.evaluate_idle();
    }

    fn continue_(self: &Arc<Self>) {
        self.context.lock().paused = false;
        if let Some(turn) = self.turn.lock().clone() {
            turn.send(crate::human::agent::turn::TurnEvent::Continue);
        }
        let should_resume = {
            let context = self.context.lock();
            matches!(*self.state.lock(), AgentState::IdleReady | AgentState::IdleWaiting)
                && context.notifications.is_empty()
                && context.queue.is_empty()
                && history_ends_mid_tool_chain(&context.messages)
        };
        if should_resume {
            self.commit_pending_to_history();
            self.start_running();
        } else {
            self.evaluate_idle();
        }
    }

    fn spawn_turn_tools(self: &Arc<Self>, tool_calls: Vec<ToolCall>) {
        if !matches!(*self.state.lock(), AgentState::RunningActive | AgentState::RunningAborting) {
            return;
        }
        let logic = self.context.lock().tool_logic.clone().unwrap();
        let wait_for_tasks = create_wait_for_tasks(self.machine_self());
        for tool_call in tool_calls {
            let scope = with_abort(&self.context.lock().scope.signal);
            let this = self.clone();
            let actor = logic.start(
                ToolInput {
                    tool_call: tool_call.clone(),
                    signal: scope.signal.clone(),
                    wait_for_tasks: Some(wait_for_tasks.clone()),
                },
                Arc::new(move |event| this.send(AgentEvent::Tool(event))),
            );
            self.context.lock().turn_tools.insert(
                tool_call.id.clone(),
                ToolEntry {
                    tool_call,
                    scope: scope.clone(),
                    ref_: actor.clone(),
                },
            );
            if *self.state.lock() == AgentState::RunningAborting {
                scope.abort();
                actor.send(ToolEvent::Abort);
            }
        }
    }

    fn handle_tool(self: &Arc<Self>, event: ToolEvent) {
        if matches!(event, ToolEvent::Update { .. }) {
            self.emit(AgentEmitted::Tool(event.clone()));
            self.forward(AgentEvent::Tool(event));
            return;
        }
        let id = match &event {
            ToolEvent::Update { tool_call_id, .. }
            | ToolEvent::Detached { tool_call_id, .. }
            | ToolEvent::Done { tool_call_id, .. }
            | ToolEvent::Failed { tool_call_id, .. }
            | ToolEvent::Aborted { tool_call_id } => tool_call_id.clone(),
            ToolEvent::Abort => return,
        };
        let in_background = self.context.lock().background.contains_key(&id);
        if in_background && matches!(event, ToolEvent::Done { .. } | ToolEvent::Failed { .. }) {
            let entry = self.context.lock().background.remove(&id);
            if let Some(entry) = entry {
                let output = match &event {
                    ToolEvent::Done { result, .. } => ToolOutput::Succeeded {
                        result: result.clone(),
                    },
                    ToolEvent::Failed { error, .. } => ToolOutput::Failed {
                        error: error.clone(),
                    },
                    _ => unreachable!(),
                };
                let notification = completion_notification(&entry.tool_call, &output);
                self.context.lock().notifications.push(notification);
            }
            self.emit(AgentEmitted::Tool(event.clone()));
            self.forward(AgentEvent::Tool(event));
            self.evaluate_idle();
            return;
        }
        if !matches!(*self.state.lock(), AgentState::RunningActive | AgentState::RunningAborting) {
            return;
        }
        if !self.context.lock().turn_tools.contains_key(&id) {
            return;
        }
        match event {
            ToolEvent::Detached { ref tool_call_id, .. } => {
                let entry = self.context.lock().turn_tools.remove(tool_call_id);
                if let Some(entry) = entry {
                    self.context.lock().background.insert(id.clone(), entry);
                }
                self.forward_tool_to_turn(&event);
                self.emit(AgentEmitted::Tool(event.clone()));
                self.forward(AgentEvent::Tool(event));
            }
            ToolEvent::Done { .. } | ToolEvent::Failed { .. } | ToolEvent::Aborted { .. } => {
                self.forward_tool_to_turn(&event);
                self.emit(AgentEmitted::Tool(event.clone()));
                self.forward(AgentEvent::Tool(event));
            }
            _ => {}
        }
    }

    fn forward_tool_to_turn(&self, event: &ToolEvent) {
        let Some(turn) = self.turn.lock().clone() else {
            return;
        };
        let event = match event {
            ToolEvent::Detached { tool_call_id, text } => TurnToolEvent::Detached {
                tool_call_id: tool_call_id.clone(),
                text: text.clone(),
            },
            ToolEvent::Done { tool_call_id, result } => TurnToolEvent::Done {
                tool_call_id: tool_call_id.clone(),
                result: result.clone(),
            },
            ToolEvent::Failed { tool_call_id, error } => TurnToolEvent::Failed {
                tool_call_id: tool_call_id.clone(),
                error: Value::String(error.clone()),
            },
            ToolEvent::Aborted { tool_call_id } => TurnToolEvent::Aborted {
                tool_call_id: tool_call_id.clone(),
            },
            _ => return,
        };
        turn.send(crate::human::agent::turn::TurnEvent::Tool(event));
    }

    fn abort(self: &Arc<Self>, reason: Option<Value>) {
        if *self.state.lock() == AgentState::RunningActive {
            self.context.lock().abort_reason = reason;
            *self.state.lock() = AgentState::RunningAborting;
            self.abort_turn();
            self.abort_turn_tools();
            self.emit(AgentEmitted::TurnAborting);
            let this = self.clone();
            let delay = self.options.abort_timeout_ms.unwrap_or(10_000);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if *this.state.lock() == AgentState::RunningAborting {
                    this.abort_turn();
                    this.turn_tools_stop();
                }
            });
        } else if *self.state.lock() == AgentState::RunningAborting {
            self.context.lock().abort_reason = reason;
            self.abort_turn();
            self.turn_tools_stop();
        }
    }

    fn abort_turn(&self) {
        if let Some(turn) = self.turn.lock().clone() {
            turn.send(crate::human::agent::turn::TurnEvent::Abort {
                reason: self.context.lock().abort_reason.clone(),
            });
        }
    }

    fn abort_turn_tools(&self) {
        for entry in self.context.lock().turn_tools.values() {
            entry.scope.abort();
            entry.ref_.send(ToolEvent::Abort);
        }
    }

    fn turn_tools_stop(&self) {
        let entries = std::mem::take(&mut self.context.lock().turn_tools);
        for (_, entry) in entries {
            entry.scope.abort();
            entry.ref_.send(ToolEvent::Abort);
        }
    }

    fn close(self: &Arc<Self>) {
        let state = *self.state.lock();
        if state == AgentState::Disposed {
            return;
        }
        if state == AgentState::Linking {
            self.context.lock().scope.abort();
            *self.state.lock() = AgentState::Disposed;
            return;
        }
        if matches!(state, AgentState::RunningActive | AgentState::RunningAborting) {
            self.context.lock().turn_id += 1;
            self.turn.lock().take();
        }
        *self.state.lock() = AgentState::Closing;
        self.context.lock().scope.abort();
        self.abort_turn_tools();
        self.turn_tools_stop();
        let handle = self.context.lock().handle.clone();
        let this = self.clone();
        tokio::spawn(async move {
            if let Some(handle) = handle {
                let _ = handle.dispose_async().await;
            }
            if let Some(unsubscribe) = this.store_unsubscribe.lock().take() {
                unsubscribe();
            }
            if *this.state.lock() == AgentState::Closing {
                *this.state.lock() = AgentState::Disposed;
            }
        });
    }

    fn append_events(&self, events: Vec<ExternalEvent>) {
        if events.is_empty() {
            return;
        }
        let Some(store) = self.context.lock().store.clone() else {
            return;
        };
        let append_lock = self.append_lock.clone();
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = append_lock.lock().await;
            if let Err(error) = store.dispatch(events).await {
                this.send(AgentEvent::StoreError {
                    error: json!({
                        "code": error.code,
                        "message": error.message,
                    }),
                });
            }
        });
    }
}

fn user_meta_with_default(meta: Option<UserMeta>, source: &str) -> UserMeta {
    let mut meta = meta.unwrap_or_default();
    if meta.source.is_none() {
        meta.source = Some(source.to_string());
    }
    meta
}

pub fn create_agent_machine(options: CreateAgentMachineOptions) -> Arc<AgentMachineLogic> {
    Arc::new(AgentMachineLogic { options })
}
