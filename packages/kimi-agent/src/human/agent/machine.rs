use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, FutureExt};
use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::human::agent::errors::{LOOP_MAX_STEPS_EXCEEDED_ERROR_CODE, TurnInterruptReason};
use crate::human::agent::events::{MESSAGE_APPENDED, TURN_ENDED, TURN_STARTED};
use crate::human::agent::origin::{SteerMessage, merge_steer_messages};
use crate::human::agent::slices::{AgentStoreState, TurnIndexState};
use crate::human::agent::turn::{
    AssistantEntry, EntryMeta, HistoryMessage, SystemEntry, TurnInput, TurnLlmEvent,
    TurnMachineLogic, TurnOutput, UserEntry, UserMeta, create_system_entry, create_user_entry,
};
use crate::human::agent::wait_for::{AgentSnapshotSource, ToolActorRef, create_wait_for_tasks};
use crate::human::eventStore::actor::{StoreActorEvent, StoreActorInput, store_actor};
use crate::human::eventStore::eventStore::EventStoreImpl;
use crate::human::eventStore::events::ExternalEvent;
use crate::human::llm::message::{Role, ToolCall, UserMessage, create_user_message};
use crate::human::llm::requester::actor::{LlmEvent, actor_event_to_xevent};
use crate::human::llm::requester::requester::LlmRequestConfig;
use crate::human::tool::executor::{ToolExecuteInput, ToolExecutor, ToolResult};
use crate::human::tool::machine::{
    ToolEvent, ToolInput, ToolMachineLogic, ToolOutput, tool_event_xevent,
};
use crate::human::tool::tool::ToolDefinition;
use crate::human::utils::abort::{AbortScope, create_abort_scope, with_abort};
use crate::human::xstate2::{
    Action, Actor, Context, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent, assign,
    custom, emit, enqueue_actions, from_callback, from_promise, send_to, stop_child,
};
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
    pub turn_logic: Arc<TurnMachineLogic>,
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

#[derive(Clone, serde::Serialize)]
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
        TurnLlmEvent::Event(LlmEvent::Sent { .. }) => "llm.sent",
        TurnLlmEvent::Event(LlmEvent::Retrying { .. }) => "llm.retrying",
        TurnLlmEvent::Event(LlmEvent::Recovering { .. }) => "llm.recovering",
        TurnLlmEvent::Event(LlmEvent::Request(event)) => {
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
    pub turn_logic: Option<Arc<TurnMachineLogic>>,
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

#[derive(Clone)]
struct ScopeFactoryActorInput {
    scope_factory: ScopeFactory,
    self_: AgentMachineSelf,
}

#[derive(Clone)]
struct PromptGateActorInput {
    gate: Option<PromptGate>,
    head: Option<UserEntry>,
}

#[derive(Clone)]
struct PromptGateActorOutput {
    id: Option<String>,
    block: bool,
    message: Option<UserMessage>,
    error: Option<Value>,
}

fn context_of(context: &Context) -> &AgentMachineContext {
    context
        .downcast_ref::<AgentMachineContext>()
        .expect("agent machine context")
}

fn assign_agent(
    apply: impl Fn(&mut AgentMachineContext, &XEvent) + Send + Sync + 'static,
) -> Action {
    assign(move |args| {
        let mut next = context_of(args.context).clone();
        apply(&mut next, args.event);
        Arc::new(next)
    })
}

fn forward_event() -> Action {
    custom(|_, args| args.self_.forward_to_parent(args.event.clone()))
}

fn emitted_xevent(emitted: AgentEmitted) -> XEvent {
    XEvent {
        type_: emitted.type_(),
        payload: Value::Null,
        restore: None,
        data: Some(Arc::new(emitted)),
    }
}

fn agent_event_of(event: &XEvent) -> Option<AgentEvent> {
    if let Some(agent_event) = event
        .data
        .as_ref()
        .and_then(|data| data.downcast_ref::<AgentEvent>())
    {
        return Some(agent_event.clone());
    }
    match event.type_.as_str() {
        "tool.update" | "tool.detached" | "tool.done" | "tool.failed" | "tool.aborted"
        | "tool.abort" => Some(AgentEvent::Tool(tool_event_of(event))),
        "llm.done" => {
            let entry = event
                .data
                .clone()
                .and_then(|data| data.downcast::<AssistantEntry>().ok())?;
            Some(AgentEvent::Llm(TurnLlmEvent::Done {
                entry: entry.as_ref().clone(),
            }))
        }
        type_ if type_.starts_with("llm.") => {
            let llm_event = event
                .data
                .clone()
                .and_then(|data| data.downcast::<LlmEvent>().ok())?;
            Some(AgentEvent::Llm(TurnLlmEvent::Event(
                llm_event.as_ref().clone(),
            )))
        }
        "step.started" => Some(AgentEvent::StepStarted {
            step: step_of(event),
        }),
        "turn.spawn_tools" => Some(AgentEvent::TurnSpawnTools {
            tool_calls: tool_calls_of(event),
        }),
        "turn.reminders_consumed" => Some(AgentEvent::TurnRemindersConsumed {
            reminders: reminders_of(event),
        }),
        _ => None,
    }
}

fn agent_emitted_of(event: &XEvent) -> Option<AgentEmitted> {
    if let Some(emitted) = event
        .data
        .as_ref()
        .and_then(|data| data.downcast_ref::<AgentEmitted>())
    {
        return Some(emitted.clone());
    }
    match agent_event_of(event)? {
        AgentEvent::Llm(event) => Some(AgentEmitted::Llm(event)),
        AgentEvent::Tool(event) => Some(AgentEmitted::Tool(event)),
        AgentEvent::StepStarted { step } => Some(AgentEmitted::StepStarted { step }),
        AgentEvent::TurnRemindersConsumed { reminders } => {
            Some(AgentEmitted::TurnRemindersConsumed { reminders })
        }
        _ => None,
    }
}

fn tool_event_of(event: &XEvent) -> ToolEvent {
    match event
        .data
        .clone()
        .and_then(|data| data.downcast::<ToolEvent>().ok())
    {
        Some(event) => event.as_ref().clone(),
        None => serde_json::from_value(event.payload.clone()).expect("tool event payload"),
    }
}

fn step_of(event: &XEvent) -> u64 {
    match event
        .data
        .clone()
        .and_then(|data| data.downcast::<u64>().ok())
    {
        Some(step) => *step,
        None => event
            .payload
            .get("step")
            .and_then(Value::as_u64)
            .expect("step.started step"),
    }
}

fn tool_calls_of(event: &XEvent) -> Vec<ToolCall> {
    match event
        .data
        .clone()
        .and_then(|data| data.downcast::<Vec<ToolCall>>().ok())
    {
        Some(tool_calls) => tool_calls.as_ref().clone(),
        None => serde_json::from_value(
            event
                .payload
                .get("toolCalls")
                .cloned()
                .expect("turn.spawn_tools toolCalls"),
        )
        .expect("turn.spawn_tools toolCalls"),
    }
}

fn reminders_of(event: &XEvent) -> Vec<HistoryMessage> {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<Vec<HistoryMessage>>().ok())
        .map(|reminders| reminders.as_ref().clone())
        .expect("turn.reminders_consumed reminders")
}

fn scope_factory_output_of(event: &XEvent) -> ScopeFactoryOutput {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<ScopeFactoryOutput>().ok())
        .map(|output| output.as_ref().clone())
        .expect("scope factory output")
}

fn prompt_gate_output_of(event: &XEvent) -> PromptGateActorOutput {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<PromptGateActorOutput>().ok())
        .map(|output| output.as_ref().clone())
        .expect("prompt gate output")
}

fn turn_output_of(event: &XEvent) -> TurnOutput {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<TurnOutput>().ok())
        .map(|output| output.as_ref().clone())
        .expect("turn output")
}

fn error_of(event: &XEvent) -> Value {
    match &event.payload {
        Value::Null => event
            .data
            .as_ref()
            .and_then(|data| data.downcast_ref::<Value>().cloned())
            .unwrap_or(Value::Null),
        payload => payload.clone(),
    }
}

fn store_state_of(event: &XEvent) -> AgentStoreState {
    serde_json::from_value(
        event
            .payload
            .get("state")
            .cloned()
            .expect("store state payload"),
    )
    .expect("store state payload")
}

fn store_branch_of(event: &XEvent) -> String {
    event
        .payload
        .get("branch")
        .and_then(Value::as_str)
        .expect("store branch payload")
        .to_string()
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
                    role: Role::User,
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

fn has_pending_work(context: &AgentMachineContext) -> bool {
    !context.notifications.is_empty() || !context.queue.is_empty()
}

fn has_background_work(context: &AgentMachineContext) -> bool {
    !context.background.is_empty()
}

fn history_meta_key(entry: &HistoryMessage) -> Option<&str> {
    match entry {
        HistoryMessage::System(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::User(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::Assistant(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::Tool(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
    }
}

fn first_prompt_id(context: &AgentMachineContext) -> Option<String> {
    context
        .queue
        .first()
        .and_then(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()))
}

fn user_meta_with_default(meta: Option<UserMeta>, source: &str) -> UserMeta {
    let mut meta = meta.unwrap_or_default();
    if meta.source.is_none() {
        meta.source = Some(source.to_string());
    }
    meta
}

fn system_reminder_meta(meta: Option<EntryMeta>, key: &str) -> EntryMeta {
    let mut next = EntryMeta {
        source: Some("reminder".to_string()),
        key: Some(key.to_string()),
    };
    if let Some(meta) = meta {
        if meta.source.is_some() {
            next.source = meta.source;
        }
        if meta.key.is_some() {
            next.key = meta.key;
        }
    }
    next
}

fn user_reminder_meta(meta: Option<UserMeta>, key: &str) -> UserMeta {
    let mut next = UserMeta {
        source: Some("reminder".to_string()),
        key: Some(key.to_string()),
        ..Default::default()
    };
    if let Some(meta) = meta {
        if meta.source.is_some() {
            next.source = meta.source;
        }
        if meta.key.is_some() {
            next.key = meta.key;
        }
        next.prompt_id = meta.prompt_id;
        next.origin = meta.origin;
        next.tracked = meta.tracked;
        next.created_at = meta.created_at;
        next.user_message_id = meta.user_message_id;
    }
    next
}

fn decode_state<T: DeserializeOwned + Default>(state: &AgentStoreState, key: &str) -> T {
    state
        .get(key)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn history_event(message: HistoryMessage) -> ExternalEvent {
    MESSAGE_APPENDED.create(Map::from_iter([(
        "message".to_string(),
        serde_json::to_value(message).expect("history message serializes"),
    )]))
}

fn turn_started_event(turn_id: i64, queue_item_id: Option<String>) -> ExternalEvent {
    let mut payload = Map::from_iter([("turnId".to_string(), json!(turn_id))]);
    if let Some(queue_item_id) = queue_item_id {
        payload.insert("queueItemId".to_string(), Value::String(queue_item_id));
    }
    TURN_STARTED.create(payload)
}

fn turn_ended_event(turn_id: i64, outcome: &str, error_message: Option<String>) -> ExternalEvent {
    let mut payload = Map::from_iter([
        ("turnId".to_string(), json!(turn_id)),
        ("outcome".to_string(), json!(outcome)),
    ]);
    if let Some(error_message) = error_message {
        payload.insert("errorMessage".to_string(), Value::String(error_message));
    }
    TURN_ENDED.create(payload)
}

fn store_append(events: Vec<ExternalEvent>) -> XEvent {
    StoreActorEvent::Append(events).into_xevent()
}

fn turn_notify(messages: Vec<HistoryMessage>) -> XEvent {
    XEvent {
        type_: "turn.notify".to_string(),
        payload: json!({ "messages": messages }),
        restore: None,
        data: Some(Arc::new(messages)),
    }
}

fn tool_abort() -> XEvent {
    tool_event_xevent(&ToolEvent::Abort)
}

fn interrupt_reason_value(error: &Value) -> TurnInterruptReason {
    if error.get("code").and_then(Value::as_str) == Some(LOOP_MAX_STEPS_EXCEEDED_ERROR_CODE) {
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

fn turn_output_produced(output: &TurnOutput) -> &[HistoryMessage] {
    match output {
        TurnOutput::Done { produced }
        | TurnOutput::Failed { produced, .. }
        | TurnOutput::Aborted { produced } => produced,
    }
}

fn turn_outcome_event(context: &AgentMachineContext, output: &TurnOutput) -> AgentEmitted {
    match output {
        TurnOutput::Failed { error, .. } => AgentEmitted::TurnFailed {
            error: error.clone(),
            messages: context.messages.clone(),
            interrupt_reason: interrupt_reason_value(error),
            branch_id: context.branch_id.clone(),
        },
        TurnOutput::Aborted { .. } => AgentEmitted::TurnAborted {
            messages: context.messages.clone(),
            branch_id: context.branch_id.clone(),
        },
        TurnOutput::Done { .. } => AgentEmitted::TurnDone {
            messages: context.messages.clone(),
            branch_id: context.branch_id.clone(),
        },
    }
}

fn turn_outcome_name(output: &TurnOutput) -> &'static str {
    match output {
        TurnOutput::Done { .. } => "done",
        TurnOutput::Failed { .. } => "failed",
        TurnOutput::Aborted { .. } => "aborted",
    }
}

fn steer_queued() -> Action {
    enqueue_actions(|args, enqueue| {
        let Some(AgentEvent::InputSteer { id }) = agent_event_of(args.event) else {
            return;
        };
        let context = context_of(args.context);
        let steered = context
            .queue
            .iter()
            .filter(|item| {
                item.meta
                    .as_ref()
                    .and_then(|meta| meta.prompt_id.as_ref())
                    .is_some_and(|prompt_id| id.contains(prompt_id))
            })
            .cloned()
            .collect::<Vec<_>>();
        if steered.is_empty() {
            return;
        }
        let merged = merge_steer_messages(
            &steered
                .iter()
                .map(|item| SteerMessage {
                    content: item.message.content.clone(),
                    origin: item.meta.as_ref().and_then(|meta| meta.origin.clone()),
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
        let mut origin = merged.origin;
        origin.in_turn = Some(true);
        let mut next = context.clone();
        next.queue.retain(|item| !steered.contains(item));
        next.notifications.push(create_user_entry(
            UserMessage {
                role: Role::User,
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
        enqueue.assign(Arc::new(next));
        enqueue.emit(emitted_xevent(AgentEmitted::PromptSteered {
            queue_item_ids: steered
                .iter()
                .filter_map(|item| item.meta.as_ref().and_then(|meta| meta.prompt_id.clone()))
                .collect(),
            entries: steered,
        }));
    })
}

fn commit_pending_to_history() -> Action {
    enqueue_actions(|args, enqueue| {
        let context = context_of(args.context);
        let head = context.queue.first().cloned();
        let mut events = context
            .notifications
            .iter()
            .cloned()
            .map(|entry| history_event(HistoryMessage::User(entry)))
            .collect::<Vec<_>>();
        if let Some(head) = &head {
            events.push(history_event(HistoryMessage::User(head.clone())));
        }
        if !events.is_empty() {
            enqueue.send_to("store", store_append(events));
        }
        let mut next = context.clone();
        next.messages
            .extend(context.notifications.iter().cloned().map(HistoryMessage::User));
        if let Some(head) = &head {
            next.messages.push(HistoryMessage::User(head.clone()));
        }
        next.notifications.clear();
        if !next.queue.is_empty() {
            next.queue.remove(0);
        }
        next.drained_id = head
            .as_ref()
            .and_then(|entry| entry.meta.as_ref().and_then(|meta| meta.prompt_id.clone()));
        next.drained_entry = head;
        enqueue.assign(Arc::new(next));
    })
}

fn reset_mirror() -> Action {
    assign_agent(|next, event| {
        if event.type_ != "store.reset" {
            return;
        }
        let state = store_state_of(event);
        next.messages = decode_state(&state, "history");
        next.notifications = decode_state(&state, "notifications");
        next.reminders = decode_state(&state, "reminders");
        next.turn_tools.clear();
        next.background.clear();
        next.scope = create_abort_scope();
        next.turn_id = decode_state::<TurnIndexState>(&state, "turnIndex").next_turn_id;
        next.active_turn_id = None;
        next.branch_id = store_branch_of(event);
    })
}

fn emit_reset() -> Action {
    emit(|args| {
        emitted_xevent(AgentEmitted::ContextReset {
            branch_id: context_of(args.context).branch_id.clone(),
        })
    })
}

fn abort_scope() -> Action {
    custom(|_, args| context_of(args.context).scope.abort())
}

fn spawn_store() -> Action {
    assign(|args| {
        args.spawn.spawn_logic(
            &store_actor(),
            "store",
            Arc::new(StoreActorInput {
                store: scope_factory_output_of(args.event).store,
            }) as Context,
        );
        let output = scope_factory_output_of(args.event);
        let mut next = context_of(args.context).clone();
        next.store = Some(output.store.clone());
        next.handle = output.handle.clone();
        next.turn_logic = Some(output.turn_logic.clone());
        next.tool_logic = Some(output.tool_logic.clone());
        next.tools = Some(output.tools.clone());
        if let Some(request) = output.request.clone() {
            next.request = request;
        }
        next.prompt_gate = output.prompt_gate.clone();
        Arc::new(next)
    })
}

fn spawn_turn_tools() -> Action {
    assign(|args| {
        let mut next = context_of(args.context).clone();
        let Some(AgentEvent::TurnSpawnTools { tool_calls }) = agent_event_of(args.event) else {
            return Arc::new(next);
        };
        let tool_logic = next.tool_logic.clone().expect("tool logic");
        let wait_for_tasks = create_wait_for_tasks(AgentMachineSelf::new(args.self_.clone()));
        for tool_call in tool_calls {
            let scope = with_abort(&next.scope.signal);
            let ref_ = args.spawn.spawn(
                tool_logic.machine(),
                &tool_call.id,
                Arc::new(ToolInput {
                    tool_call: tool_call.clone(),
                    signal: scope.signal.clone(),
                    wait_for_tasks: Some(wait_for_tasks.clone()),
                }),
            );
            next.turn_tools.insert(
                tool_call.id.clone(),
                ToolEntry {
                    tool_call,
                    scope,
                    ref_,
                },
            );
        }
        Arc::new(next)
    })
}

fn abort_spawned_tools() -> Action {
    enqueue_actions(|args, enqueue| {
        let Some(AgentEvent::TurnSpawnTools { tool_calls }) = agent_event_of(args.event) else {
            return;
        };
        let context = context_of(args.context);
        for tool_call in tool_calls {
            if let Some(entry) = context.turn_tools.get(&tool_call.id) {
                entry.scope.abort();
                enqueue.send_to_child(entry.ref_.clone(), tool_abort());
            }
        }
    })
}

fn remember_abort_reason() -> Action {
    assign_agent(|next, event| {
        next.abort_reason = match agent_event_of(event) {
            Some(AgentEvent::InputAbort { reason }) => reason,
            _ => None,
        };
    })
}

fn abort_turn() -> Action {
    send_to("turn", |args| {
        XEvent::new(
            "turn.abort",
            json!({ "reason": context_of(args.context).abort_reason }),
        )
    })
}

fn abort_turn_tools() -> Action {
    enqueue_actions(|args, enqueue| {
        for entry in context_of(args.context).turn_tools.values() {
            entry.scope.abort();
            enqueue.send_to_child(entry.ref_.clone(), tool_abort());
        }
    })
}

fn stop_turn_tools() -> Action {
    enqueue_actions(|args, enqueue| {
        for (tool_call_id, entry) in &context_of(args.context).turn_tools {
            entry.scope.abort();
            enqueue.stop_child(tool_call_id.clone());
        }
    })
}

fn spawn_turn(max_steps_per_turn: Option<u64>) -> Action {
    assign(move |args| {
        let context = context_of(args.context);
        let mut request = context.request.clone();
        request.tools = context.tools.as_ref().map(|tools| {
            tools
                .iter()
                .filter(|tool| tool.deferred.is_none())
                .map(ToolDefinition::description)
                .collect()
        });
        let turn_logic = context.turn_logic.as_ref().expect("turn logic");
        args.spawn.spawn(
            turn_logic.machine(),
            "turn",
            Arc::new(TurnInput {
                request,
                history: context.messages.clone(),
                max_steps: max_steps_per_turn,
                parent_signal: Some(context.scope.signal.clone()),
            }),
        );
        args.context.clone()
    })
}

fn store_reset_in_running() -> Vec<Action> {
    vec![abort_scope(), reset_mirror(), emit_reset(), forward_event()]
}

#[derive(Clone)]
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
    machine: Machine,
}

impl AgentMachineLogic {
    pub fn machine(&self) -> &Machine {
        &self.machine
    }
}

pub struct AgentSnapshot {
    pub context: AgentMachineContext,
    pub value: String,
    pub status: String,
}

pub struct AgentSubscription {
    subscription: crate::human::xstate2::Subscription,
}

impl AgentSubscription {
    pub fn unsubscribe(&self) {
        self.subscription.unsubscribe();
    }
}

#[derive(Clone)]
pub struct AgentMachineSelf {
    actor: Arc<Actor>,
}

impl AgentMachineSelf {
    pub fn new(actor: Arc<Actor>) -> Self {
        Self { actor }
    }

    pub fn send(&self, event: AgentEvent) {
        self.actor.send(event.into());
    }

    pub fn get_snapshot(&self) -> AgentSnapshot {
        let snapshot = self.actor.get_snapshot();
        AgentSnapshot {
            context: snapshot
                .context
                .downcast_ref::<AgentMachineContext>()
                .expect("agent machine context")
                .clone(),
            value: snapshot.value,
            status: snapshot.status,
        }
    }

    pub fn on(
        &self,
        type_: &str,
        handler: Arc<dyn Fn(AgentEmitted) + Send + Sync>,
    ) -> AgentSubscription {
        let subscription = self.actor.on(
            type_,
            Arc::new(move |event: &XEvent| {
                if let Some(emitted) = agent_emitted_of(event) {
                    handler(emitted);
                }
            }),
        );
        AgentSubscription { subscription }
    }
}

impl AgentSnapshotSource for AgentMachineSelf {
    fn snapshot_background(&self) -> IndexMap<String, ToolActorRef> {
        self.actor
            .get_snapshot()
            .context
            .downcast_ref::<AgentMachineContext>()
            .expect("agent machine context")
            .background
            .iter()
            .map(|(id, entry)| (id.clone(), entry.ref_.clone()))
            .collect()
    }
}

impl From<AgentEvent> for XEvent {
    fn from(event: AgentEvent) -> Self {
        match event {
            AgentEvent::Llm(TurnLlmEvent::Event(event)) => actor_event_to_xevent(event),
            AgentEvent::Llm(TurnLlmEvent::Done { entry }) => XEvent {
                type_: "llm.done".to_string(),
                payload: json!({ "entry": entry }),
                restore: None,
                data: Some(Arc::new(entry)),
            },
            AgentEvent::Tool(event) => tool_event_xevent(&event),
            AgentEvent::InputSubmit { entry } => XEvent {
                type_: "input.submit".to_string(),
                payload: json!({ "entry": entry }),
                restore: None,
                data: Some(Arc::new(AgentEvent::InputSubmit { entry })),
            },
            AgentEvent::InputNotify { entry } => XEvent {
                type_: "input.notify".to_string(),
                payload: json!({ "entry": entry }),
                restore: None,
                data: Some(Arc::new(AgentEvent::InputNotify { entry })),
            },
            AgentEvent::InputRemind { key, entry } => XEvent {
                type_: "input.remind".to_string(),
                payload: json!({ "key": key, "entry": entry }),
                restore: None,
                data: Some(Arc::new(AgentEvent::InputRemind { key, entry })),
            },
            AgentEvent::InputSteer { id } => XEvent {
                type_: "input.steer".to_string(),
                payload: json!({ "id": id }),
                restore: None,
                data: Some(Arc::new(AgentEvent::InputSteer { id })),
            },
            AgentEvent::InputCancel { id } => XEvent {
                type_: "input.cancel".to_string(),
                payload: json!({ "id": id }),
                restore: None,
                data: Some(Arc::new(AgentEvent::InputCancel { id })),
            },
            AgentEvent::InputAbort { reason } => XEvent {
                type_: "input.abort".to_string(),
                payload: json!({ "reason": reason }),
                restore: None,
                data: Some(Arc::new(AgentEvent::InputAbort { reason })),
            },
            AgentEvent::InputPause => XEvent::new("input.pause", Value::Null),
            AgentEvent::InputContinue => XEvent::new("input.continue", Value::Null),
            AgentEvent::InputClose => XEvent::new("input.close", Value::Null),
            AgentEvent::TurnSpawnTools { tool_calls } => XEvent {
                type_: "turn.spawn_tools".to_string(),
                payload: json!({ "toolCalls": tool_calls }),
                restore: None,
                data: Some(Arc::new(AgentEvent::TurnSpawnTools { tool_calls })),
            },
            AgentEvent::TurnDrain => XEvent::new("turn.drain", Value::Null),
            AgentEvent::TurnRemindersConsumed { reminders } => XEvent {
                type_: "turn.reminders_consumed".to_string(),
                payload: json!({ "reminders": reminders }),
                restore: None,
                data: Some(Arc::new(AgentEvent::TurnRemindersConsumed { reminders })),
            },
            AgentEvent::StepStarted { step } => XEvent {
                type_: "step.started".to_string(),
                payload: json!({ "step": step }),
                restore: None,
                data: Some(Arc::new(step)),
            },
            AgentEvent::StoreReady { state, branch } => XEvent {
                type_: "store.ready".to_string(),
                payload: json!({ "state": state, "branch": branch }),
                restore: None,
                data: Some(Arc::new(AgentEvent::StoreReady { state, branch })),
            },
            AgentEvent::StoreChanged { state } => XEvent {
                type_: "store.changed".to_string(),
                payload: json!({ "state": state }),
                restore: None,
                data: Some(Arc::new(AgentEvent::StoreChanged { state })),
            },
            AgentEvent::StoreReset { state, branch } => XEvent {
                type_: "store.reset".to_string(),
                payload: json!({ "state": state, "branch": branch }),
                restore: None,
                data: Some(Arc::new(AgentEvent::StoreReset { state, branch })),
            },
            AgentEvent::StoreError { error } => XEvent {
                type_: "store.error".to_string(),
                payload: json!({ "error": error }),
                restore: None,
                data: Some(Arc::new(AgentEvent::StoreError { error })),
            },
            AgentEvent::TurnDone { output } => XEvent {
                type_: "xstate.done.actor.turn".to_string(),
                payload: Value::Null,
                restore: None,
                data: Some(Arc::new(output)),
            },
            AgentEvent::TurnError { error } => XEvent {
                type_: "xstate.error.actor.turn".to_string(),
                payload: error.clone(),
                restore: None,
                data: Some(Arc::new(error)),
            },
        }
    }
}

pub fn create_agent_machine(options: CreateAgentMachineOptions) -> Arc<AgentMachineLogic> {
    let abort_timeout_ms = options.abort_timeout_ms.unwrap_or(10_000);
    let max_steps_per_turn = options.max_steps_per_turn;
    let controller_guard = from_callback(|input| {
        let scope = input
            .runtime
            .downcast::<AbortScope>()
            .expect("controller guard input")
            .as_ref()
            .clone();
        Box::new(move || scope.abort())
    });
    let scope_factory = from_promise(|input| {
        async move {
            let runtime = input
                .input
                .downcast::<ScopeFactoryActorInput>()
                .expect("scope factory input");
            let output = (runtime.scope_factory)(runtime.self_.clone(), input.signal.clone()).await?;
            Ok(Arc::new(output) as Context)
        }
        .boxed()
    });
    let prompt_gate = from_promise(|input| {
        async move {
            let runtime = input
                .input
                .downcast::<PromptGateActorInput>()
                .expect("prompt gate input");
            let id = runtime
                .head
                .as_ref()
                .and_then(|head| head.meta.as_ref().and_then(|meta| meta.prompt_id.clone()));
            let output = match (runtime.gate.clone(), runtime.head.clone()) {
                (Some(gate), Some(head)) => match gate(id.clone(), head.message.clone()).await {
                    Ok(PromptGateVerdict::Bool(block)) => PromptGateActorOutput {
                        id,
                        block,
                        message: None,
                        error: None,
                    },
                    Ok(PromptGateVerdict::Detailed { block, message }) => PromptGateActorOutput {
                        id,
                        block,
                        message,
                        error: None,
                    },
                    Err(error) => PromptGateActorOutput {
                        id,
                        block: false,
                        message: None,
                        error: Some(Value::String(error.to_string())),
                    },
                },
                _ => PromptGateActorOutput {
                    id,
                    block: false,
                    message: None,
                    error: None,
                },
            };
            Ok(Arc::new(output) as Context)
        }
        .boxed()
    });
    let dispose_scope = from_promise(|input| {
        async move {
            let handle = input
                .input
                .downcast::<Option<AgentScopeHandle>>()
                .expect("dispose scope input");
            if let Some(handle) = handle.as_ref() {
                handle.dispose_async().await?;
            }
            Ok(Arc::new(()) as Context)
        }
        .boxed()
    });

    let linking = StateNode {
        invoke: Some(InvokeDef {
            id: "scopeFactoryActor".to_string(),
            src: "scopeFactoryActor".to_string(),
            input: Arc::new(|args| InvokeInput {
                runtime: Arc::new(ScopeFactoryActorInput {
                    scope_factory: context_of(args.context).input.scope_factory.clone(),
                    self_: AgentMachineSelf::new(args.self_.clone()),
                }),
                restore: None,
            }),
            on_done: vec![Transition {
                target: Some("#agent.restoring".to_string()),
                actions: vec![
                    spawn_store(),
                    emit(|_| emitted_xevent(AgentEmitted::AgentAttached)),
                ],
                ..Default::default()
            }],
            on_error: vec![Transition {
                target: Some("#agent.disposed".to_string()),
                actions: vec![emit(|args| emitted_xevent(AgentEmitted::AgentFailed {
                    error: error_of(args.event),
                }))],
                ..Default::default()
            }],
        }),
        on: HashMap::from([(
            "input.close".to_string(),
            vec![Transition {
                target: Some("#agent.disposed".to_string()),
                ..Default::default()
            }],
        )]),
        ..Default::default()
    };

    let restoring = StateNode {
        on: HashMap::from([(
            "store.ready".to_string(),
            vec![Transition {
                target: Some("idle".to_string()),
                actions: vec![assign_agent(|next, event| {
                    let state = store_state_of(event);
                    let stored_notifications = decode_state::<Vec<UserEntry>>(&state, "notifications");
                    let stored_reminders = decode_state::<Vec<HistoryMessage>>(&state, "reminders");
                    let stored_queue = decode_state::<Vec<UserEntry>>(&state, "queue");
                    next.messages = decode_state(&state, "history");
                    next.notifications = [stored_notifications, next.notifications.clone()].concat();
                    next.reminders = [
                        stored_reminders
                            .into_iter()
                            .filter(|entry| {
                                !next
                                    .reminders
                                    .iter()
                                    .any(|local| history_meta_key(local) == history_meta_key(entry))
                            })
                            .collect(),
                        next.reminders.clone(),
                    ]
                    .concat();
                    next.queue = [stored_queue, next.queue.clone()].concat();
                    next.turn_id = decode_state::<TurnIndexState>(&state, "turnIndex").next_turn_id;
                    next.branch_id = store_branch_of(event);
                })],
                ..Default::default()
            }],
        )]),
        ..Default::default()
    };

    let ready_always = vec![
        Transition {
            target: Some("gating".to_string()),
            guard: Some(Arc::new(|context, _| {
                let context = context_of(context);
                context.prompt_gate.is_some() && !context.queue.is_empty() && !context.paused
            })),
            ..Default::default()
        },
        Transition {
            target: Some("#agent.running".to_string()),
            guard: Some(Arc::new(|context, _| {
                let context = context_of(context);
                has_pending_work(context) && !context.paused
            })),
            actions: vec![commit_pending_to_history()],
            ..Default::default()
        },
        Transition {
            target: Some("waiting".to_string()),
            guard: Some(Arc::new(|context, _| has_background_work(context_of(context)))),
            ..Default::default()
        },
    ];
    let waiting_always = vec![
        Transition {
            target: Some("gating".to_string()),
            guard: Some(Arc::new(|context, _| {
                let context = context_of(context);
                context.prompt_gate.is_some() && !context.queue.is_empty() && !context.paused
            })),
            ..Default::default()
        },
        Transition {
            target: Some("#agent.running".to_string()),
            guard: Some(Arc::new(|context, _| {
                let context = context_of(context);
                has_pending_work(context) && !context.paused
            })),
            actions: vec![commit_pending_to_history()],
            ..Default::default()
        },
    ];
    let gating = StateNode {
        invoke: Some(InvokeDef {
            id: "promptGateActor".to_string(),
            src: "promptGateActor".to_string(),
            input: Arc::new(|args| {
                let context = context_of(args.context);
                InvokeInput {
                    runtime: Arc::new(PromptGateActorInput {
                        gate: context.prompt_gate.clone(),
                        head: context.queue.first().cloned(),
                    }),
                    restore: None,
                }
            }),
            on_done: vec![
                Transition {
                    target: Some("ready".to_string()),
                    guard: Some(Arc::new(|context, event| {
                        let context = context_of(context);
                        let output = prompt_gate_output_of(event);
                        context.paused || first_prompt_id(context) != output.id
                    })),
                    ..Default::default()
                },
                Transition {
                    target: Some("ready".to_string()),
                    guard: Some(Arc::new(|_, event| {
                        prompt_gate_output_of(event).error.is_some()
                    })),
                    actions: vec![
                        emit(|args| {
                            let context = context_of(args.context);
                            emitted_xevent(AgentEmitted::PromptGateFailed {
                                queue_item_id: first_prompt_id(context),
                                error: prompt_gate_output_of(args.event)
                                    .error
                                    .unwrap_or(Value::Null),
                                entry: context.queue.first().cloned(),
                            })
                        }),
                        assign_agent(|next, _| {
                            if !next.queue.is_empty() {
                                next.queue.remove(0);
                            }
                        }),
                    ],
                    ..Default::default()
                },
                Transition {
                    target: Some("ready".to_string()),
                    guard: Some(Arc::new(|_, event| prompt_gate_output_of(event).block)),
                    actions: vec![
                        emit(|args| {
                            let context = context_of(args.context);
                            emitted_xevent(AgentEmitted::PromptBlocked {
                                queue_item_id: first_prompt_id(context),
                                entry: context.queue.first().cloned(),
                            })
                        }),
                        assign_agent(|next, _| {
                            if !next.queue.is_empty() {
                                next.queue.remove(0);
                            }
                        }),
                    ],
                    ..Default::default()
                },
                Transition {
                    target: Some("#agent.running".to_string()),
                    actions: vec![
                        assign_agent(|next, event| {
                            let rewritten = prompt_gate_output_of(event).message;
                            let Some(head) = next.queue.first().cloned() else {
                                return;
                            };
                            let Some(rewritten) = rewritten else {
                                return;
                            };
                            next.queue[0] = UserEntry {
                                message: rewritten,
                                meta: head.meta,
                            };
                        }),
                        commit_pending_to_history(),
                    ],
                    ..Default::default()
                },
            ],
            on_error: vec![Transition {
                target: Some("ready".to_string()),
                actions: vec![
                    emit(|args| {
                        let context = context_of(args.context);
                        emitted_xevent(AgentEmitted::PromptGateFailed {
                            queue_item_id: first_prompt_id(context),
                            error: error_of(args.event),
                            entry: context.queue.first().cloned(),
                        })
                    }),
                    assign_agent(|next, _| {
                        if !next.queue.is_empty() {
                            next.queue.remove(0);
                        }
                    }),
                ],
                ..Default::default()
            }],
        }),
        ..Default::default()
    };
    let idle = StateNode {
        initial: Some("ready".to_string()),
        on: HashMap::from([(
            "input.continue".to_string(),
            vec![Transition {
                target: Some("running".to_string()),
                guard: Some(Arc::new(|context, _| {
                    let context = context_of(context);
                    !has_pending_work(context) && history_ends_mid_tool_chain(&context.messages)
                })),
                actions: vec![
                    assign_agent(|next, _| next.paused = false),
                    commit_pending_to_history(),
                ],
                ..Default::default()
            }],
        )]),
        states: HashMap::from([
            (
                "ready".to_string(),
                StateNode {
                    always: ready_always,
                    ..Default::default()
                },
            ),
            (
                "waiting".to_string(),
                StateNode {
                    always: waiting_always,
                    ..Default::default()
                },
            ),
            ("gating".to_string(), gating),
        ]),
        ..Default::default()
    };

    let mut running_on: HashMap<String, Vec<Transition>> = HashMap::new();
    running_on.insert(
        "xstate.done.actor.turn".to_string(),
        vec![Transition {
            target: Some("#agent.idle".to_string()),
            actions: vec![
                assign_agent(|next, event| {
                    next.messages
                        .extend(turn_output_produced(&turn_output_of(event)).iter().cloned());
                }),
                emit(|args| {
                    emitted_xevent(turn_outcome_event(
                        context_of(args.context),
                        &turn_output_of(args.event),
                    ))
                }),
                send_to("store", |args| {
                    let output = turn_output_of(args.event);
                    let context = context_of(args.context);
                    let mut events = turn_output_produced(&output)
                        .iter()
                        .cloned()
                        .map(history_event)
                        .collect::<Vec<_>>();
                    events.push(turn_ended_event(
                        context.active_turn_id.unwrap_or(context.turn_id),
                        turn_outcome_name(&output),
                        match &output {
                            TurnOutput::Failed { error, .. } => Some(error_message(error)),
                            _ => None,
                        },
                    ));
                    store_append(events)
                }),
            ],
            ..Default::default()
        }],
    );
    running_on.insert(
        "xstate.error.actor.turn".to_string(),
        vec![Transition {
            target: Some("#agent.idle".to_string()),
            actions: vec![
                emit(|args| {
                    let context = context_of(args.context);
                    emitted_xevent(AgentEmitted::TurnFailed {
                        error: error_of(args.event),
                        messages: context.messages.clone(),
                        interrupt_reason: interrupt_reason_value(&error_of(args.event)),
                        branch_id: context.branch_id.clone(),
                    })
                }),
                send_to("store", |args| {
                    let context = context_of(args.context);
                    store_append(vec![turn_ended_event(
                        context.active_turn_id.unwrap_or(context.turn_id),
                        "failed",
                        Some(error_message(&error_of(args.event))),
                    )])
                }),
            ],
            ..Default::default()
        }],
    );
    running_on.insert(
        "store.reset".to_string(),
        vec![Transition {
            target: Some("#agent.idle".to_string()),
            actions: store_reset_in_running(),
            ..Default::default()
        }],
    );
    running_on.insert(
        "input.pause".to_string(),
        vec![Transition {
            actions: vec![
                assign_agent(|next, _| next.paused = true),
                send_to("turn", |_| XEvent::new("turn.pause", Value::Null)),
            ],
            ..Default::default()
        }],
    );
    running_on.insert(
        "input.continue".to_string(),
        vec![Transition {
            actions: vec![
                assign_agent(|next, _| next.paused = false),
                send_to("turn", |_| XEvent::new("turn.continue", Value::Null)),
            ],
            ..Default::default()
        }],
    );
    running_on.insert(
        "turn.drain".to_string(),
        vec![Transition {
            actions: vec![enqueue_actions(|args, enqueue| {
                let context = context_of(args.context);
                let mut messages = context
                    .notifications
                    .iter()
                    .cloned()
                    .map(HistoryMessage::User)
                    .collect::<Vec<_>>();
                messages.extend(context.reminders.iter().cloned());
                enqueue.send_to("turn", turn_notify(messages.clone()));
                if messages.is_empty() {
                    return;
                }
                let mut next = context.clone();
                next.notifications.clear();
                next.reminders.clear();
                enqueue.assign(Arc::new(next));
            })],
            ..Default::default()
        }],
    );
    running_on.insert(
        "tool.detached".to_string(),
        vec![Transition {
            guard: Some(Arc::new(|context, event| {
                let ToolEvent::Detached { tool_call_id, .. } = tool_event_of(event) else {
                    return false;
                };
                context_of(context).turn_tools.contains_key(&tool_call_id)
            })),
            actions: vec![
                assign_agent(|next, event| {
                    let ToolEvent::Detached { tool_call_id, .. } = tool_event_of(event) else {
                        return;
                    };
                    if let Some(entry) = next.turn_tools.shift_remove(&tool_call_id) {
                        next.background.insert(tool_call_id, entry);
                    }
                }),
                send_to("turn", |args| args.event.clone()),
                emit(|args| args.event.clone()),
                forward_event(),
            ],
            ..Default::default()
        }],
    );
    for event_type in ["tool.done", "tool.failed", "tool.aborted"] {
        running_on.insert(
            event_type.to_string(),
            vec![Transition {
                guard: Some(Arc::new(|context, event| {
                    let tool_call_id = match tool_event_of(event) {
                        ToolEvent::Done { tool_call_id, .. }
                        | ToolEvent::Failed { tool_call_id, .. }
                        | ToolEvent::Aborted { tool_call_id } => tool_call_id,
                        ToolEvent::Update { .. } | ToolEvent::Detached { .. } | ToolEvent::Abort => {
                            return false;
                        }
                    };
                    let context = context_of(context);
                    !context.background.contains_key(&tool_call_id)
                        && context.turn_tools.contains_key(&tool_call_id)
                })),
                actions: vec![
                    send_to("turn", |args| args.event.clone()),
                    emit(|args| args.event.clone()),
                    forward_event(),
                ],
                ..Default::default()
            }],
        );
    }
    for event_type in [
        "llm.sent",
        "step.started",
        "llm.streaming.*",
        "llm.done",
        "llm.failed.syntax",
        "llm.failed.remote",
        "llm.retrying",
        "llm.recovering",
        "turn.reminders_consumed",
    ] {
        running_on.insert(
            event_type.to_string(),
            vec![Transition {
                actions: vec![emit(|args| args.event.clone()), forward_event()],
                ..Default::default()
            }],
        );
    }

    let running_active = StateNode {
        on: HashMap::from([
            (
                "turn.spawn_tools".to_string(),
                vec![Transition {
                    actions: vec![spawn_turn_tools()],
                    ..Default::default()
                }],
            ),
            (
                "input.steer".to_string(),
                vec![Transition {
                    guard: Some(Arc::new(|context, event| {
                        let Some(AgentEvent::InputSteer { id }) = agent_event_of(event) else {
                            return false;
                        };
                        context_of(context).queue.iter().any(|item| {
                            item.meta
                                .as_ref()
                                .and_then(|meta| meta.prompt_id.as_ref())
                                .is_some_and(|prompt_id| id.contains(prompt_id))
                        })
                    })),
                    actions: vec![
                        steer_queued(),
                        send_to("turn", |_| XEvent::new("turn.steer", Value::Null)),
                    ],
                    ..Default::default()
                }],
            ),
            (
                "input.abort".to_string(),
                vec![Transition {
                    target: Some("aborting".to_string()),
                    actions: vec![
                        remember_abort_reason(),
                        abort_turn(),
                        abort_turn_tools(),
                        emit(|_| emitted_xevent(AgentEmitted::TurnAborting)),
                    ],
                    ..Default::default()
                }],
            ),
        ]),
        ..Default::default()
    };
    let running_aborting = StateNode {
        after: vec![(
            "abortTimeout".to_string(),
            Transition {
                actions: vec![abort_turn(), stop_turn_tools()],
                ..Default::default()
            },
        )],
        on: HashMap::from([
            (
                "turn.spawn_tools".to_string(),
                vec![Transition {
                    actions: vec![spawn_turn_tools(), abort_spawned_tools()],
                    ..Default::default()
                }],
            ),
            (
                "input.abort".to_string(),
                vec![Transition {
                    actions: vec![remember_abort_reason(), abort_turn(), stop_turn_tools()],
                    ..Default::default()
                }],
            ),
        ]),
        ..Default::default()
    };
    let running = StateNode {
        entry: vec![
            assign_agent(|next, _| {
                next.active_turn_id = Some(next.turn_id);
                next.abort_reason = None;
            }),
            emit(|args| {
                let context = context_of(args.context);
                emitted_xevent(AgentEmitted::TurnStarted {
                    turn_id: context.turn_id,
                    branch_id: context.branch_id.clone(),
                    queue_item_id: context.drained_id.clone(),
                    entry: context.drained_entry.clone(),
                })
            }),
            send_to("store", |args| {
                let context = context_of(args.context);
                store_append(vec![turn_started_event(
                    context.turn_id,
                    context.drained_id.clone(),
                )])
            }),
            spawn_turn(max_steps_per_turn),
        ],
        exit: vec![
            stop_child(|_| Some("turn".to_string())),
            abort_turn_tools(),
            stop_turn_tools(),
            assign_agent(|next, _| next.turn_tools.clear()),
            assign_agent(|next, _| next.turn_id += 1),
        ],
        initial: Some("active".to_string()),
        on: running_on,
        states: HashMap::from([
            ("active".to_string(), running_active),
            ("aborting".to_string(), running_aborting),
        ]),
        ..Default::default()
    };

    let closing = StateNode {
        entry: vec![abort_scope(), abort_turn_tools(), stop_turn_tools()],
        invoke: Some(InvokeDef {
            id: "disposeScopeActor".to_string(),
            src: "disposeScopeActor".to_string(),
            input: Arc::new(|args| InvokeInput {
                runtime: Arc::new(context_of(args.context).handle.clone()),
                restore: None,
            }),
            on_done: vec![Transition {
                target: Some("#agent.disposed".to_string()),
                ..Default::default()
            }],
            on_error: vec![Transition {
                target: Some("#agent.disposed".to_string()),
                ..Default::default()
            }],
        }),
        ..Default::default()
    };
    let disposed = StateNode {
        final_: true,
        ..Default::default()
    };

    let mut root_on: HashMap<String, Vec<Transition>> = HashMap::new();
    root_on.insert(
        "input.close".to_string(),
        vec![Transition {
            target: Some(".closing".to_string()),
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.submit".to_string(),
        vec![Transition {
            actions: vec![assign_agent(|next, event| {
                let Some(AgentEvent::InputSubmit { entry }) = agent_event_of(event) else {
                    return;
                };
                next.queue.push(create_user_entry(
                    entry.message,
                    user_meta_with_default(entry.meta, "input"),
                ));
            })],
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.notify".to_string(),
        vec![Transition {
            actions: vec![assign_agent(|next, event| {
                let Some(AgentEvent::InputNotify { entry }) = agent_event_of(event) else {
                    return;
                };
                next.notifications.push(create_user_entry(
                    entry.message,
                    user_meta_with_default(entry.meta, "notify"),
                ));
            })],
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.remind".to_string(),
        vec![Transition {
            actions: vec![assign_agent(|next, event| {
                let Some(AgentEvent::InputRemind { key, entry }) = agent_event_of(event) else {
                    return;
                };
                next.reminders
                    .retain(|entry| history_meta_key(entry) != Some(key.as_str()));
                match entry {
                    AgentReminderEntry::System(entry) => {
                        next.reminders.push(HistoryMessage::System(create_system_entry(
                            entry.message,
                            system_reminder_meta(entry.meta, &key),
                        )));
                    }
                    AgentReminderEntry::User(entry) => {
                        next.reminders.push(HistoryMessage::User(create_user_entry(
                            entry.message,
                            user_reminder_meta(entry.meta, &key),
                        )));
                    }
                }
            })],
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.steer".to_string(),
        vec![Transition {
            actions: vec![steer_queued()],
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.cancel".to_string(),
        vec![Transition {
            actions: vec![assign_agent(|next, event| {
                let Some(AgentEvent::InputCancel { id }) = agent_event_of(event) else {
                    return;
                };
                next.queue.retain(|item| {
                    item.meta
                        .as_ref()
                        .and_then(|meta| meta.prompt_id.as_deref())
                        != Some(id.as_str())
                });
            })],
            ..Default::default()
        }],
    );
    root_on.insert(
        "store.reset".to_string(),
        vec![Transition {
            target: Some(".idle".to_string()),
            actions: store_reset_in_running(),
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.pause".to_string(),
        vec![Transition {
            actions: vec![assign_agent(|next, _| next.paused = true)],
            ..Default::default()
        }],
    );
    root_on.insert(
        "input.continue".to_string(),
        vec![Transition {
            actions: vec![assign_agent(|next, _| next.paused = false)],
            ..Default::default()
        }],
    );
    root_on.insert(
        "store.error".to_string(),
        vec![Transition {
            actions: vec![forward_event()],
            ..Default::default()
        }],
    );
    root_on.insert("store.changed".to_string(), vec![Transition::default()]);
    root_on.insert(
        "tool.update".to_string(),
        vec![Transition {
            actions: vec![emit(|args| args.event.clone()), forward_event()],
            ..Default::default()
        }],
    );
    for event_type in ["tool.done", "tool.failed"] {
        root_on.insert(
            event_type.to_string(),
            vec![Transition {
                guard: Some(Arc::new(|context, event| {
                    let tool_call_id = match tool_event_of(event) {
                        ToolEvent::Done { tool_call_id, .. }
                        | ToolEvent::Failed { tool_call_id, .. } => tool_call_id,
                        _ => return false,
                    };
                    context_of(context).background.contains_key(&tool_call_id)
                })),
                actions: vec![
                    assign_agent(|next, event| {
                        let (tool_call_id, output) = match tool_event_of(event) {
                            ToolEvent::Done { tool_call_id, result } => {
                                (tool_call_id, ToolOutput::Succeeded { result })
                            }
                            ToolEvent::Failed { tool_call_id, error } => {
                                (tool_call_id, ToolOutput::Failed { error })
                            }
                            _ => return,
                        };
                        if let Some(entry) = next.background.shift_remove(&tool_call_id) {
                            next.notifications.push(completion_notification(
                                &entry.tool_call,
                                &output,
                            ));
                        }
                    }),
                    emit(|args| args.event.clone()),
                    forward_event(),
                ],
                ..Default::default()
            }],
        );
    }

    let machine = Machine {
        id: "agent".to_string(),
        initial: "linking".to_string(),
        context: Arc::new(|input| {
            let input = input.downcast::<AgentInput>().expect("agent input");
            Arc::new(AgentMachineContext {
                request: input.request.clone(),
                input: input.as_ref().clone(),
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
            }) as Context
        }),
        invoke: Some(InvokeDef {
            id: "controllerGuard".to_string(),
            src: "controllerGuard".to_string(),
            input: Arc::new(|args| InvokeInput {
                runtime: Arc::new(context_of(args.context).scope.clone()),
                restore: None,
            }),
            on_done: Vec::new(),
            on_error: Vec::new(),
        }),
        states: HashMap::from([
            ("linking".to_string(), linking),
            ("restoring".to_string(), restoring),
            ("idle".to_string(), idle),
            ("running".to_string(), running),
            ("closing".to_string(), closing),
            ("disposed".to_string(), disposed),
        ]),
        on: root_on,
        actors: HashMap::from([
            ("storeActor".to_string(), store_actor()),
            ("controllerGuard".to_string(), controller_guard),
            ("scopeFactoryActor".to_string(), scope_factory),
            ("promptGateActor".to_string(), prompt_gate),
            ("disposeScopeActor".to_string(), dispose_scope),
        ]),
        delays: HashMap::from([(
            "abortTimeout".to_string(),
            Arc::new(move |_: &Context| Duration::from_millis(abort_timeout_ms))
                as Arc<dyn Fn(&Context) -> Duration + Send + Sync>,
        )]),
        output: None,
    };
    Arc::new(AgentMachineLogic { machine })
}
