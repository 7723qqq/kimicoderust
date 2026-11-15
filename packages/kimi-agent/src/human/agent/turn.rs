use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::human::agent::context_usage::{ContextUsagePrefix, estimate_used_context_tokens};
use crate::human::agent::errors::MaxStepsExceededError;
use crate::human::agent::origin::PromptOrigin;
use crate::human::llm::empty_response::empty_response_error;
use crate::human::llm::errors::LlmErrorMessage;
use crate::human::llm::finish_reason::{FinishInfo, NO_FINISH};
use crate::human::llm::message::{
    AssistantMessage, ContentPart, Message, MessageAccumulator, StreamedMessagePart, SystemMessage,
    ToolCall, ToolMessage, create_tool_message, salvage_interrupted_message,
};
pub use crate::human::llm::message::UserMessage;
use crate::human::llm::model::LlmModel;
use crate::human::llm::requester::actor::{
    LlmEvent, LlmInput, MessageResolver, actor_event_to_xevent, create_request_actor,
};
use crate::human::llm::requester::recovery::{
    LlmRecovery, LlmRecoveryContext, LlmRecoveryProposalRecord, LlmRecoveryRecord,
};
use crate::human::llm::requester::requester::{
    LlmRequestConfig, LlmRequestContent, LlmRequestEvent, LlmRequester,
};
use crate::human::llm::requester::retry::{
    LlmRetryOptions, read_retry_after_ms, resolve_max_attempts, retry_backoff_delay,
    retry_error_fields, should_retry,
};
use crate::human::llm::tool_call_id_normalizer::{
    ToolCallIdNormalizer, ToolCallIdResponseNormalizer, ToolCallIdSeed, ToolCallIdSeedMessage,
};
use crate::human::llm::usage::{TokenUsage, TokenUsagePatch, empty_usage};
use crate::human::tool::executor::ToolResult;
use crate::human::tool::machine::{ToolEvent, ToolOutput};
use crate::human::utils::abort::{AbortScope, create_abort_scope, with_abort};
use crate::human::xstate2::{
    Action, ActorLogic, Context, InvokeDef, InvokeInput, Machine, StateNode, Transition, XEvent,
    assign, custom, from_promise, raise,
};
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

pub type SystemMeta = EntryMeta;
pub type ToolMeta = EntryMeta;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<PromptOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantModelMeta {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AssistantModelMeta>,
    pub usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

impl Default for AssistantMeta {
    fn default() -> Self {
        Self {
            source: None,
            key: None,
            model: None,
            usage: empty_usage(),
            headers: None,
            finish: None,
            message_id: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMetaInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AssistantModelMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish: Option<FinishInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry<M, F> {
    pub message: M,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<F>,
}

pub type SystemEntry = HistoryEntry<SystemMessage, SystemMeta>;
pub type UserEntry = HistoryEntry<UserMessage, UserMeta>;
pub type ToolEntry = HistoryEntry<ToolMessage, ToolMeta>;
pub type AssistantEntry = HistoryEntry<AssistantMessage, AssistantMeta>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HistoryMessage {
    System(SystemEntry),
    User(UserEntry),
    Assistant(AssistantEntry),
    Tool(ToolEntry),
}

impl HistoryMessage {
    pub fn message(&self) -> Message {
        match self {
            Self::System(entry) => Message::System(entry.message.clone()),
            Self::User(entry) => Message::User(entry.message.clone()),
            Self::Assistant(entry) => Message::Assistant(entry.message.clone()),
            Self::Tool(entry) => Message::Tool(entry.message.clone()),
        }
    }

    pub fn meta_source(&self) -> Option<&str> {
        match self {
            Self::System(entry) => entry.meta.as_ref().and_then(|meta| meta.source.as_deref()),
            Self::User(entry) => entry.meta.as_ref().and_then(|meta| meta.source.as_deref()),
            Self::Assistant(entry) => entry.meta.as_ref().and_then(|meta| meta.source.as_deref()),
            Self::Tool(entry) => entry.meta.as_ref().and_then(|meta| meta.source.as_deref()),
        }
    }
}

pub fn create_user_entry(message: UserMessage, meta: UserMeta) -> UserEntry {
    UserEntry {
        message,
        meta: Some(meta),
    }
}

pub fn create_system_entry(message: SystemMessage, meta: SystemMeta) -> SystemEntry {
    SystemEntry {
        message,
        meta: Some(meta),
    }
}

pub fn create_tool_entry(message: ToolMessage, meta: ToolMeta) -> ToolEntry {
    ToolEntry {
        message,
        meta: Some(meta),
    }
}

pub fn create_assistant_entry(message: AssistantMessage, meta: AssistantMeta) -> AssistantEntry {
    AssistantEntry {
        message,
        meta: Some(meta),
    }
}

pub fn to_input_messages(history: &[HistoryMessage]) -> Vec<Message> {
    history.iter().map(HistoryMessage::message).collect()
}

pub struct HistoryAccumulator {
    inner: MessageAccumulator,
    response: Option<ToolCallIdResponseNormalizer>,
    usage: Option<TokenUsage>,
    headers: Option<HashMap<String, String>>,
    finish: Option<FinishInfo>,
    message_id: Option<String>,
    meta: Option<AssistantMetaInput>,
    finished: Option<AssistantMessage>,
}

impl HistoryAccumulator {
    pub fn push(&mut self, part: StreamedMessagePart) -> StreamedMessagePart {
        self.finished = None;
        if let StreamedMessagePart::ToolCall(call) = &part
            && let Some(response) = &mut self.response
        {
            let id = response.remap_streamed_id(&call.id, call._stream_index.clone());
            if id != call.id {
                let remapped = StreamedMessagePart::ToolCall(ToolCall {
                    id,
                    raw_id: call.raw_id.clone().or_else(|| Some(call.id.clone())),
                    ..call.clone()
                });
                self.inner.push(remapped.clone());
                return remapped;
            }
        }
        self.inner.push(part.clone());
        part
    }

    pub fn rollback(&mut self) {
        if let Some(response) = &mut self.response {
            response.rollback();
        }
    }

    pub fn has_tool_call(&self) -> bool {
        self.inner.has_tool_call()
    }

    pub fn push_usage(&mut self, value: TokenUsagePatch) {
        let previous = self.usage.clone().unwrap_or_else(empty_usage);
        let raw = match value.raw {
            Some(raw) => {
                let mut merged = previous.raw.clone().unwrap_or_default();
                merged.extend(raw);
                Some(merged)
            }
            None => previous.raw,
        };
        self.usage = Some(TokenUsage {
            input_other: value.input_other.unwrap_or(previous.input_other),
            output: value.output.unwrap_or(previous.output),
            input_cache_read: value.input_cache_read.unwrap_or(previous.input_cache_read),
            input_cache_creation: value
                .input_cache_creation
                .unwrap_or(previous.input_cache_creation),
            raw,
        });
    }

    pub fn push_headers(&mut self, value: HashMap<String, String>) {
        self.headers = Some(value);
    }

    pub fn push_finish(&mut self, value: FinishInfo) {
        self.finish = Some(value);
    }

    pub fn push_message_id(&mut self, value: String) {
        self.message_id = Some(value);
    }

    pub fn finish(&mut self, extra: Option<AssistantMetaInput>) -> AssistantEntry {
        let message = if let Some(message) = &self.finished {
            message.clone()
        } else {
            let message = self.inner.finish();
            self.finished = Some(message.clone());
            message
        };
        let base = self.meta.clone().unwrap_or_default();
        let extra = extra.unwrap_or_default();
        let usage = self.usage.clone().or(extra.usage).or(base.usage).unwrap_or_else(empty_usage);
        let meta = AssistantMeta {
            source: extra.source.or(base.source),
            key: extra.key.or(base.key),
            model: extra.model.or(base.model),
            usage,
            headers: self.headers.clone(),
            finish: self.finish.clone(),
            message_id: self.message_id.clone(),
        };
        create_assistant_entry(message, meta)
    }
}

pub fn create_history_accumulator(
    meta: Option<AssistantMetaInput>,
    tool_call_ids: Option<ToolCallIdNormalizer>,
) -> HistoryAccumulator {
    HistoryAccumulator {
        inner: MessageAccumulator::new(),
        response: tool_call_ids.map(|ids| ids.begin_response()),
        usage: None,
        headers: None,
        finish: None,
        message_id: None,
        meta,
        finished: None,
    }
}

fn model_meta(model: &LlmModel) -> AssistantMetaInput {
    AssistantMetaInput {
        model: Some(AssistantModelMeta {
            provider: model.provider.clone(),
            model: model.model.clone(),
        }),
        ..Default::default()
    }
}

#[derive(Clone)]
pub struct TurnInput {
    pub request: LlmRequestConfig,
    pub history: Vec<HistoryMessage>,
    pub max_steps: Option<u64>,
    pub parent_signal: Option<CancellationToken>,
}

#[derive(Clone)]
pub enum TurnLlmEvent {
    Event(LlmEvent),
    Done { entry: AssistantEntry },
}

#[derive(Clone)]
pub enum TurnOutput {
    Done { produced: Vec<HistoryMessage> },
    Failed { error: Value, produced: Vec<HistoryMessage> },
    Aborted { produced: Vec<HistoryMessage> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Failed,
    Aborted,
}

#[derive(Clone)]
pub struct TurnMachineContext {
    pub input: TurnInput,
    pub produced: Vec<HistoryMessage>,
    pub accumulator: Arc<Mutex<HistoryAccumulator>>,
    pub tool_call_ids: ToolCallIdNormalizer,
    pub llm_scope: AbortScope,
    pub pending_tool_calls: Vec<ToolCall>,
    pub outcomes: HashMap<String, ToolOutput>,
    pub steps: u64,
    pub step: u64,
    pub attempt: u32,
    pub delay_ms: u64,
    pub applied_recoveries: Vec<LlmRecoveryRecord>,
    pub attempt_message_override: Option<Vec<Message>>,
    pub paused: bool,
    pub outcome: Option<TurnOutcome>,
    pub error: Option<Value>,
}

fn tool_outcome_entry(tool_call: &ToolCall, output: &ToolOutput) -> HistoryMessage {
    let entry = match output {
        ToolOutput::Failed { error } => create_tool_entry(
            create_tool_message(&tool_call.id, error.as_str()),
            EntryMeta {
                source: Some("tool".to_string()),
                key: None,
            },
        ),
        ToolOutput::Aborted => create_tool_entry(
            create_tool_message(&tool_call.id, "aborted"),
            EntryMeta {
                source: Some("tool".to_string()),
                key: None,
            },
        ),
        ToolOutput::Succeeded { result } => create_tool_entry(
            create_tool_message(&tool_call.id, result.content.clone()),
            EntryMeta {
                source: Some("tool".to_string()),
                key: None,
            },
        ),
    };
    HistoryMessage::Tool(entry)
}

fn async_ack_outcome(tool_call: &ToolCall, text: &str) -> ToolOutput {
    ToolOutput::Succeeded {
        result: ToolResult {
            content: vec![ContentPart::Text {
                text: if text.is_empty() {
                    format!("async running: {}", tool_call.name)
                } else {
                    text.to_string()
                },
                meta: None,
            }],
            is_error: None,
        },
    }
}

#[derive(Clone)]
pub struct TurnBeforeStepContext {
    pub messages: Vec<HistoryMessage>,
    pub request: LlmRequestConfig,
}

pub type TurnBeforeStep = Arc<
    dyn Fn(TurnBeforeStepContext) -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync,
>;

#[derive(Clone, Default)]
pub struct CreateTurnMachineOptions {
    pub recovery: Option<Arc<dyn LlmRecovery>>,
    pub retry: Option<LlmRetryOptions>,
    pub abort_grace_ms: Option<u64>,
    pub message_resolvers: Vec<Arc<dyn MessageResolver>>,
    pub on_before_step: Option<TurnBeforeStep>,
}

#[derive(Clone)]
pub struct TurnMachineLogic {
    machine: Machine,
}

impl TurnMachineLogic {
    pub fn machine(&self) -> &Machine {
        &self.machine
    }
}

struct FailureEvaluated {
    cause: LlmRequestEvent,
    proposal: Option<LlmRecoveryProposalRecord>,
}

fn context_of(context: &Context) -> &TurnMachineContext {
    context
        .downcast_ref::<TurnMachineContext>()
        .expect("turn machine context")
}

fn typed_llm_event(event: &XEvent) -> LlmEvent {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<LlmEvent>().ok())
        .map(|event| (*event).clone())
        .expect("typed llm event")
}

fn typed_tool_event(event: &XEvent) -> ToolEvent {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<ToolEvent>().ok())
        .map(|event| (*event).clone())
        .expect("typed tool event")
}

fn notify_messages(event: &XEvent) -> Vec<HistoryMessage> {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<Vec<HistoryMessage>>().ok())
        .map(|messages| (*messages).clone())
        .expect("turn.notify messages")
}

fn failure_evaluated(event: &XEvent) -> Arc<Mutex<FailureEvaluated>> {
    event
        .data
        .clone()
        .and_then(|data| data.downcast::<Mutex<FailureEvaluated>>().ok())
        .expect("turn.failure.evaluated")
}

fn seed_messages(history: &[HistoryMessage]) -> Vec<ToolCallIdSeedMessage> {
    to_input_messages(history)
        .iter()
        .map(|message| match message {
            Message::Assistant(message) => ToolCallIdSeedMessage {
                role: Some("assistant".to_string()),
                tool_calls: Some(
                    message
                        .tool_calls
                        .iter()
                        .map(|call| ToolCallIdSeed { id: call.id.clone() })
                        .collect(),
                ),
                tool_call_id: None,
            },
            Message::Tool(message) => ToolCallIdSeedMessage {
                role: Some("tool".to_string()),
                tool_calls: None,
                tool_call_id: Some(message.tool_call_id.clone()),
            },
            Message::System(message) => ToolCallIdSeedMessage {
                role: Some("system".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::User(message) => ToolCallIdSeedMessage {
                role: Some("user".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
        })
        .collect()
}

fn max_steps_exceeded(context: &TurnMachineContext) -> bool {
    context
        .input
        .max_steps
        .is_some_and(|max_steps| max_steps > 0 && context.steps >= max_steps)
}

fn base_messages(context: &TurnMachineContext) -> Vec<Message> {
    to_input_messages(&[context.input.history.clone(), context.produced.clone()].concat())
}

fn attempt_messages(context: &TurnMachineContext) -> Vec<Message> {
    context
        .attempt_message_override
        .clone()
        .unwrap_or_else(|| base_messages(context))
}

fn collect_tool_outcomes(context: &TurnMachineContext) -> TurnMachineContext {
    let mut next = context.clone();
    let produced = context
        .pending_tool_calls
        .iter()
        .map(|tool_call| {
            tool_outcome_entry(
                tool_call,
                context
                    .outcomes
                    .get(&tool_call.id)
                    .expect("tool outcome collected"),
            )
        })
        .collect::<Vec<_>>();
    next.produced.extend(produced);
    next.pending_tool_calls.clear();
    next.outcomes.clear();
    next
}

fn abort_outcomes(context: &TurnMachineContext) -> HashMap<String, ToolOutput> {
    let mut outcomes = context.outcomes.clone();
    for tool_call in &context.pending_tool_calls {
        outcomes
            .entry(tool_call.id.clone())
            .or_insert(ToolOutput::Aborted);
    }
    outcomes
}

fn empty_error_of(context: &TurnMachineContext) -> Option<LlmErrorMessage> {
    let entry = context.accumulator.lock().finish(None);
    let finish = entry
        .meta
        .as_ref()
        .and_then(|meta| meta.finish.clone())
        .unwrap_or(NO_FINISH);
    empty_response_error(&entry.message, &context.input.request.model, &finish)
}

fn propose_recovery(
    recovery: &Option<Arc<dyn LlmRecovery>>,
    context: &TurnMachineContext,
    cause: &LlmRequestEvent,
) -> Option<LlmRecoveryProposalRecord> {
    let LlmRequestEvent::FailedRemote { error, .. } = cause else {
        return None;
    };
    let recovery = recovery.as_ref()?;
    let messages = base_messages(context);
    let proposal = recovery.propose(LlmRecoveryContext {
        error: error.clone(),
        messages: messages.clone(),
        applied_recoveries: context.applied_recoveries.clone(),
        credential_provider: context.input.request.credential_provider.clone(),
    })?;
    if proposal.attempt_message_override.as_ref() == Some(&messages) {
        return None;
    }
    Some(proposal)
}

fn assign_turn(
    apply: impl Fn(&mut TurnMachineContext, &XEvent) + Send + Sync + 'static,
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

fn discard_attempt_stream() -> Action {
    custom(|_, args| {
        let context = context_of(args.context);
        context.accumulator.lock().rollback();
        let model = context.input.request.model.clone();
        let tool_call_ids = context.tool_call_ids.clone();
        *context.accumulator.lock() =
            create_history_accumulator(Some(model_meta(&model)), Some(tool_call_ids));
    })
}

fn salvage(aborted: bool) -> Action {
    assign_turn(move |next, _| {
        let partial = next.accumulator.lock().finish(Some(AssistantMetaInput {
            source: Some("salvaged".to_string()),
            ..Default::default()
        }));
        if let Some(salvaged) = salvage_interrupted_message(&partial.message) {
            next.produced.push(HistoryMessage::Assistant(
                create_assistant_entry(salvaged, partial.meta.unwrap_or_default()),
            ));
        }
        if aborted {
            next.outcome = Some(TurnOutcome::Aborted);
        }
    })
}

fn collect_aborted() -> Action {
    assign_turn(|next, _| {
        let mut with_outcomes = next.clone();
        with_outcomes.outcomes = abort_outcomes(next);
        let collected = collect_tool_outcomes(&with_outcomes);
        next.produced = collected.produced;
        next.pending_tool_calls = collected.pending_tool_calls;
        next.outcomes = collected.outcomes;
        next.outcome = Some(TurnOutcome::Aborted);
    })
}

fn collect_tool_outcomes_action() -> Action {
    assign_turn(|next, _| {
        let collected = collect_tool_outcomes(next);
        next.produced = collected.produced;
        next.pending_tool_calls = collected.pending_tool_calls;
        next.outcomes = collected.outcomes;
    })
}

fn signal_reminders_consumed() -> Action {
    custom(|_, args| {
        let reminders = notify_messages(args.event)
            .into_iter()
            .filter(|entry| entry.meta_source() == Some("reminder"))
            .collect::<Vec<_>>();
        if reminders.is_empty() {
            return;
        }
        args.self_.forward_to_parent(XEvent {
            type_: "turn.reminders_consumed".to_string(),
            payload: json!({ "reminders": reminders }),
            restore: None,
            data: Some(Arc::new(reminders)),
        });
    })
}

fn llm_done_entry(context: &TurnMachineContext) -> AssistantEntry {
    context.accumulator.lock().finish(Some(AssistantMetaInput {
        source: Some("llm".to_string()),
        ..Default::default()
    }))
}

fn forward_llm_done() -> Action {
    custom(|_, args| {
        let entry = llm_done_entry(context_of(args.context));
        args.self_.forward_to_parent(XEvent {
            type_: "llm.done".to_string(),
            payload: json!({ "entry": entry }),
            restore: None,
            data: Some(Arc::new(entry)),
        });
    })
}

fn on_before_step_actor(on_before_step: Option<TurnBeforeStep>) -> ActorLogic {
    from_promise(move |input| {
        let on_before_step = on_before_step.clone();
        let context = input
            .input
            .downcast::<TurnBeforeStepContext>()
            .expect("before step input");
        async move {
            if let Some(on_before_step) = on_before_step {
                on_before_step(context.as_ref().clone()).await?;
            }
            Ok(Arc::new(()) as Context)
        }
        .boxed()
    })
}

pub fn create_turn_machine(
    requester: Arc<dyn LlmRequester>,
    options: Option<CreateTurnMachineOptions>,
) -> TurnMachineLogic {
    let options = options.unwrap_or_default();
    let abort_grace_ms = options.abort_grace_ms.unwrap_or(2_500);
    let machine = Machine {
        id: "turn".to_string(),
        initial: "gating".to_string(),
        context: Arc::new(|input| {
            let input = input.downcast::<TurnInput>().expect("turn input");
            let tool_call_ids = ToolCallIdNormalizer::new();
            tool_call_ids.seed_from(&seed_messages(&input.history));
            let accumulator = Arc::new(Mutex::new(create_history_accumulator(
                Some(model_meta(&input.request.model)),
                Some(tool_call_ids.clone()),
            )));
            Arc::new(TurnMachineContext {
                input: input.as_ref().clone(),
                produced: Vec::new(),
                accumulator,
                tool_call_ids,
                llm_scope: create_abort_scope(),
                pending_tool_calls: Vec::new(),
                outcomes: HashMap::new(),
                steps: 1,
                step: 0,
                attempt: 1,
                delay_ms: 0,
                applied_recoveries: Vec::new(),
                attempt_message_override: None,
                paused: false,
                outcome: None,
                error: None,
            }) as Context
        }),
        actors: HashMap::from([
            (
                "llmActor".to_string(),
                create_request_actor(requester, options.message_resolvers.clone()),
            ),
            (
                "onBeforeStepActor".to_string(),
                on_before_step_actor(options.on_before_step.clone()),
            ),
        ]),
        delays: HashMap::from([
            (
                "retryDelay".to_string(),
                Arc::new(|context: &Context| Duration::from_millis(context_of(context).delay_ms))
                    as Arc<dyn Fn(&Context) -> Duration + Send + Sync>,
            ),
            (
                "abortGrace".to_string(),
                Arc::new(move |_: &Context| Duration::from_millis(abort_grace_ms))
                    as Arc<dyn Fn(&Context) -> Duration + Send + Sync>,
            ),
        ]),
        on: HashMap::from([
            (
                "turn.pause".to_string(),
                vec![Transition {
                    actions: vec![assign_turn(|next, _| next.paused = true)],
                    ..Default::default()
                }],
            ),
            (
                "turn.continue".to_string(),
                vec![Transition {
                    actions: vec![assign_turn(|next, _| next.paused = false)],
                    ..Default::default()
                }],
            ),
        ]),
        states: turn_states(&options),
        output: Some(Arc::new(|context| {
            let context = context_of(context);
            Some(Arc::new(match context.outcome {
                Some(TurnOutcome::Failed) => TurnOutput::Failed {
                    error: context.error.clone().unwrap_or(Value::Null),
                    produced: context.produced.clone(),
                },
                Some(TurnOutcome::Aborted) => TurnOutput::Aborted {
                    produced: context.produced.clone(),
                },
                None => TurnOutput::Done {
                    produced: context.produced.clone(),
                },
            }) as Context)
        })),
        ..Default::default()
    };
    TurnMachineLogic { machine }
}

fn turn_states(options: &CreateTurnMachineOptions) -> HashMap<String, StateNode> {
    let has_on_before_step = options.on_before_step.is_some();
    let recovery_for_raise = options.recovery.clone();
    let retry_for_guard = options.retry.clone();
    let retry_for_event = options.retry.clone();
    let mut states = HashMap::new();
    states.insert(
        "gating".to_string(),
        StateNode {
            always: vec![Transition {
                target: Some("thinking".to_string()),
                guard: Some(Arc::new(move |_, _| !has_on_before_step)),
                actions: Vec::new(),
            }],
            invoke: Some(InvokeDef {
                id: "onBeforeStepActor".to_string(),
                src: "onBeforeStepActor".to_string(),
                input: Arc::new(|args| {
                    let context = context_of(args.context);
                    InvokeInput {
                        runtime: Arc::new(TurnBeforeStepContext {
                            messages: [context.input.history.clone(), context.produced.clone()]
                                .concat(),
                            request: context.input.request.clone(),
                        }),
                        restore: None,
                    }
                }),
                on_done: vec![Transition {
                    target: Some("thinking".to_string()),
                    ..Default::default()
                }],
                on_error: vec![Transition {
                    target: Some("done".to_string()),
                    ..Default::default()
                }],
            }),
            on: HashMap::from([(
                "turn.abort".to_string(),
                vec![Transition {
                    target: Some("aborted".to_string()),
                    actions: vec![assign_turn(|next, _| {
                        next.outcome = Some(TurnOutcome::Aborted)
                    })],
                    ..Default::default()
                }],
            )]),
            ..Default::default()
        },
    );
    states.insert(
        "thinking".to_string(),
        StateNode {
            entry: vec![
                assign_turn(|next, _| {
                    next.accumulator = Arc::new(Mutex::new(create_history_accumulator(
                        Some(model_meta(&next.input.request.model)),
                        Some(next.tool_call_ids.clone()),
                    )));
                }),
                assign_turn(|next, _| {
                    next.llm_scope = next
                        .input
                        .parent_signal
                        .as_ref()
                        .map(with_abort)
                        .unwrap_or_else(create_abort_scope);
                }),
                assign_turn(|next, _| next.step += 1),
                custom(|_, args| {
                    let step = context_of(args.context).step;
                    args.self_.forward_to_parent(XEvent {
                        type_: "step.started".to_string(),
                        payload: json!({ "step": step }),
                        restore: None,
                        data: Some(Arc::new(step)),
                    });
                }),
            ],
            invoke: Some(InvokeDef {
                id: "llmActor".to_string(),
                src: "llmActor".to_string(),
                input: Arc::new(|args| {
                    let context = context_of(args.context);
                    let entries =
                        [context.input.history.clone(), context.produced.clone()].concat();
                    let prefix = ContextUsagePrefix {
                        system_prompt: context.input.request.system_prompt.clone(),
                        tools: context.input.request.tools.clone(),
                    };
                    InvokeInput {
                        runtime: Arc::new(LlmInput {
                            config: context.input.request.clone(),
                            content: LlmRequestContent {
                                messages: attempt_messages(context),
                                used_context_tokens: Some(estimate_used_context_tokens(
                                    &entries,
                                    Some(&prefix),
                                )),
                            },
                            signal: context.llm_scope.signal.clone(),
                        }),
                        restore: None,
                    }
                }),
                on_done: Vec::new(),
                on_error: vec![Transition {
                    target: Some("failed".to_string()),
                    actions: vec![assign_turn(|next, _| {
                        next.outcome = Some(TurnOutcome::Failed);
                        next.error = Some(Value::Null);
                    })],
                    ..Default::default()
                }],
            }),
            on: HashMap::from([
                (
                    "llm.sent".to_string(),
                    vec![Transition {
                        actions: vec![
                            custom(|_, args| {
                                let recovery =
                                    context_of(args.context).applied_recoveries.last().cloned();
                                args.self_
                                    .forward_to_parent(actor_event_to_xevent(LlmEvent::Sent {
                                        recovery,
                                    }));
                            }),
                            discard_attempt_stream(),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.request.retrying".to_string(),
                    vec![Transition {
                        actions: vec![discard_attempt_stream()],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.streaming.headers".to_string(),
                    vec![Transition {
                        actions: vec![
                            forward_event(),
                            custom(|_, args| {
                                let LlmEvent::Request(LlmRequestEvent::StreamingHeaders {
                                    headers,
                                }) = typed_llm_event(args.event)
                                else {
                                    return;
                                };
                                context_of(args.context)
                                    .accumulator
                                    .lock()
                                    .push_headers(headers);
                            }),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.streaming.part".to_string(),
                    vec![Transition {
                        actions: vec![custom(|_, args| {
                            let LlmEvent::Request(LlmRequestEvent::StreamingPart { part }) =
                                typed_llm_event(args.event)
                            else {
                                return;
                            };
                            let part = context_of(args.context).accumulator.lock().push(part);
                            args.self_.forward_to_parent(actor_event_to_xevent(
                                LlmEvent::Request(LlmRequestEvent::StreamingPart { part }),
                            ));
                        })],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.streaming.usage".to_string(),
                    vec![Transition {
                        actions: vec![
                            forward_event(),
                            custom(|_, args| {
                                let LlmEvent::Request(LlmRequestEvent::StreamingUsage { usage }) =
                                    typed_llm_event(args.event)
                                else {
                                    return;
                                };
                                context_of(args.context)
                                    .accumulator
                                    .lock()
                                    .push_usage(usage);
                            }),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.streaming.finish".to_string(),
                    vec![Transition {
                        actions: vec![
                            forward_event(),
                            custom(|_, args| {
                                let LlmEvent::Request(LlmRequestEvent::StreamingFinish {
                                    finish,
                                }) = typed_llm_event(args.event)
                                else {
                                    return;
                                };
                                context_of(args.context)
                                    .accumulator
                                    .lock()
                                    .push_finish(finish);
                            }),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.streaming.message_id".to_string(),
                    vec![Transition {
                        actions: vec![
                            forward_event(),
                            custom(|_, args| {
                                let LlmEvent::Request(LlmRequestEvent::StreamingMessageId {
                                    message_id,
                                }) = typed_llm_event(args.event)
                                else {
                                    return;
                                };
                                context_of(args.context)
                                    .accumulator
                                    .lock()
                                    .push_message_id(message_id);
                            }),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.done".to_string(),
                    vec![
                        Transition {
                            target: Some("acting".to_string()),
                            guard: Some(Arc::new(|context, _| {
                                !context_of(context)
                                    .accumulator
                                    .lock()
                                    .finish(None)
                                    .message
                                    .tool_calls
                                    .is_empty()
                            })),
                            actions: vec![
                                forward_llm_done(),
                                assign_turn(|next, _| {
                                    let entry = llm_done_entry(next);
                                    next.produced.push(HistoryMessage::Assistant(entry.clone()));
                                    next.pending_tool_calls = entry.message.tool_calls.clone();
                                }),
                            ],
                        },
                        Transition {
                            guard: Some(Arc::new(|context, _| {
                                empty_error_of(context_of(context)).is_some()
                            })),
                            actions: vec![raise(|args| {
                                let error = empty_error_of(context_of(args.context))
                                    .expect("empty response error");
                                actor_event_to_xevent(LlmEvent::Request(
                                    LlmRequestEvent::FailedRemote {
                                        error,
                                        raw_error: None,
                                    },
                                ))
                            })],
                            ..Default::default()
                        },
                        Transition {
                            target: Some("done".to_string()),
                            actions: vec![
                                forward_llm_done(),
                                assign_turn(|next, _| {
                                    let entry = llm_done_entry(next);
                                    next.produced.push(HistoryMessage::Assistant(entry));
                                }),
                            ],
                            ..Default::default()
                        },
                    ],
                ),
                (
                    "llm.failed.syntax".to_string(),
                    vec![Transition {
                        target: Some("failed".to_string()),
                        actions: vec![
                            forward_event(),
                            assign_turn(|next, event| {
                                let LlmEvent::Request(LlmRequestEvent::FailedSyntax { error }) =
                                    typed_llm_event(event)
                                else {
                                    return;
                                };
                                next.outcome = Some(TurnOutcome::Failed);
                                next.error =
                                    Some(serde_json::to_value(error).unwrap_or(Value::Null));
                            }),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "llm.failed.remote".to_string(),
                    vec![Transition {
                        actions: vec![raise({
                            let recovery = recovery_for_raise.clone();
                            move |args| {
                                let LlmEvent::Request(cause) = typed_llm_event(args.event) else {
                                    panic!("llm.failed.remote carries a request event");
                                };
                                let proposal =
                                    propose_recovery(&recovery, context_of(args.context), &cause);
                                XEvent {
                                    type_: "turn.failure.evaluated".to_string(),
                                    payload: Value::Null,
                                    restore: None,
                                    data: Some(Arc::new(Mutex::new(FailureEvaluated {
                                        cause,
                                        proposal,
                                    }))),
                                }
                            }
                        })],
                        ..Default::default()
                    }],
                ),
                (
                    "turn.failure.evaluated".to_string(),
                    vec![
                        Transition {
                            target: Some("thinking".to_string()),
                            guard: Some(Arc::new(|_, event| {
                                failure_evaluated(event).lock().proposal.is_some()
                            })),
                            actions: vec![
                                custom(|_, args| {
                                    let evaluated = failure_evaluated(args.event);
                                    let mut evaluated = evaluated.lock();
                                    context_of(args.context).accumulator.lock().rollback();
                                    if let Some(proposal) = evaluated.proposal.as_mut()
                                        && let Some(before) = proposal.before_next_attempt.take()
                                    {
                                        before();
                                    }
                                }),
                                assign_turn(|next, event| {
                                    let evaluated = failure_evaluated(event);
                                    let evaluated = evaluated.lock();
                                    let Some(proposal) = evaluated.proposal.as_ref() else {
                                        return;
                                    };
                                    next.applied_recoveries.push(LlmRecoveryRecord {
                                        strategy: proposal.strategy.clone(),
                                        action: proposal.action.clone(),
                                    });
                                    if let Some(message_override) =
                                        proposal.attempt_message_override.clone()
                                    {
                                        next.attempt_message_override = Some(message_override);
                                    }
                                    next.attempt = 1;
                                }),
                                custom(|_, args| {
                                    let context = context_of(args.context);
                                    let Some(record) = context.applied_recoveries.last() else {
                                        return;
                                    };
                                    let evaluated = failure_evaluated(args.event);
                                    let evaluated = evaluated.lock();
                                    let LlmRequestEvent::FailedRemote { error, .. } =
                                        &evaluated.cause
                                    else {
                                        return;
                                    };
                                    let fields = retry_error_fields(error);
                                    args.self_.forward_to_parent(actor_event_to_xevent(
                                        LlmEvent::Recovering {
                                            strategy: record.strategy.clone(),
                                            action: record.action.clone(),
                                            error_name: fields.error_name,
                                            error_message: fields.error_message,
                                            status_code: fields.status_code,
                                        },
                                    ));
                                }),
                            ],
                            ..Default::default()
                        },
                        Transition {
                            target: Some("retrying".to_string()),
                            guard: {
                                let retry = retry_for_guard.clone();
                                Some(Arc::new(move |context, event| {
                                    let evaluated = failure_evaluated(event);
                                    let evaluated = evaluated.lock();
                                    let LlmRequestEvent::FailedRemote { error, .. } =
                                        &evaluated.cause
                                    else {
                                        return false;
                                    };
                                    should_retry(
                                        retry.as_ref(),
                                        context_of(context).attempt,
                                        error,
                                    )
                                }))
                            },
                            actions: vec![
                                custom(|_, args| {
                                    context_of(args.context).accumulator.lock().rollback();
                                }),
                                assign_turn(|next, event| {
                                    let evaluated = failure_evaluated(event);
                                    let evaluated = evaluated.lock();
                                    let LlmRequestEvent::FailedRemote { error, .. } =
                                        &evaluated.cause
                                    else {
                                        return;
                                    };
                                    next.delay_ms = read_retry_after_ms(error)
                                        .map(|delay| delay as u64)
                                        .unwrap_or_else(|| retry_backoff_delay(next.attempt - 1));
                                }),
                                custom({
                                    let retry = retry_for_event.clone();
                                    move |_, args| {
                                        let evaluated = failure_evaluated(args.event);
                                        let evaluated = evaluated.lock();
                                        let LlmRequestEvent::FailedRemote { error, .. } =
                                            &evaluated.cause
                                        else {
                                            return;
                                        };
                                        let context = context_of(args.context);
                                        let fields = retry_error_fields(error);
                                        args.self_.forward_to_parent(actor_event_to_xevent(
                                            LlmEvent::Retrying {
                                                failed_attempt: context.attempt,
                                                next_attempt: context.attempt + 1,
                                                max_attempts: resolve_max_attempts(retry.as_ref()),
                                                delay_ms: context.delay_ms,
                                                error_name: fields.error_name,
                                                error_message: fields.error_message,
                                                status_code: fields.status_code,
                                            },
                                        ));
                                    }
                                }),
                            ],
                            ..Default::default()
                        },
                        Transition {
                            target: Some("failed".to_string()),
                            actions: vec![
                                custom(|_, args| {
                                    let evaluated = failure_evaluated(args.event);
                                    let cause = evaluated.lock().cause.clone();
                                    args.self_
                                        .forward_to_parent(actor_event_to_xevent(LlmEvent::Request(
                                            cause,
                                        )));
                                }),
                                assign_turn(|next, event| {
                                    let evaluated = failure_evaluated(event);
                                    let evaluated = evaluated.lock();
                                    let LlmRequestEvent::FailedRemote {
                                        error, raw_error, ..
                                    } = &evaluated.cause
                                    else {
                                        return;
                                    };
                                    next.outcome = Some(TurnOutcome::Failed);
                                    next.error = Some(raw_error.clone().unwrap_or_else(|| {
                                        serde_json::to_value(error).unwrap_or(Value::Null)
                                    }));
                                }),
                            ],
                            ..Default::default()
                        },
                    ],
                ),
                (
                    "turn.abort".to_string(),
                    vec![Transition {
                        target: Some("aborted".to_string()),
                        actions: vec![
                            custom(|_, args| {
                                context_of(args.context).llm_scope.abort();
                            }),
                            salvage(true),
                        ],
                        ..Default::default()
                    }],
                ),
                (
                    "turn.steer".to_string(),
                    vec![Transition {
                        target: Some("draining".to_string()),
                        guard: Some(Arc::new(|context, _| {
                            !context_of(context).accumulator.lock().has_tool_call()
                        })),
                        actions: vec![
                            custom(|_, args| {
                                context_of(args.context).llm_scope.abort();
                            }),
                            salvage(false),
                        ],
                        ..Default::default()
                    }],
                ),
            ]),
            ..Default::default()
        },
    );
    states.insert(
        "retrying".to_string(),
        StateNode {
            entry: vec![assign_turn(|next, _| next.attempt += 1)],
            after: vec![(
                "retryDelay".to_string(),
                Transition {
                    target: Some("thinking".to_string()),
                    ..Default::default()
                },
            )],
            on: HashMap::from([
                (
                    "turn.abort".to_string(),
                    vec![Transition {
                        target: Some("aborted".to_string()),
                        actions: vec![assign_turn(|next, _| {
                            next.outcome = Some(TurnOutcome::Aborted)
                        })],
                        ..Default::default()
                    }],
                ),
                (
                    "turn.steer".to_string(),
                    vec![Transition {
                        target: Some("draining".to_string()),
                        ..Default::default()
                    }],
                ),
            ]),
            ..Default::default()
        },
    );
    states.insert(
        "acting".to_string(),
        StateNode {
            entry: vec![custom(|_, args| {
                let tool_calls = context_of(args.context).pending_tool_calls.clone();
                args.self_.forward_to_parent(XEvent {
                    type_: "turn.spawn_tools".to_string(),
                    payload: json!({ "toolCalls": tool_calls }),
                    restore: None,
                    data: Some(Arc::new(tool_calls)),
                });
            })],
            initial: Some("running".to_string()),
            always: vec![
                Transition {
                    target: Some("aborted".to_string()),
                    guard: Some(Arc::new(|context, _| {
                        let context = context_of(context);
                        context.outcome == Some(TurnOutcome::Aborted)
                            && context
                                .pending_tool_calls
                                .iter()
                                .all(|tool_call| context.outcomes.contains_key(&tool_call.id))
                    })),
                    actions: vec![collect_tool_outcomes_action()],
                },
                Transition {
                    target: Some("draining".to_string()),
                    guard: Some(Arc::new(|context, _| {
                        let context = context_of(context);
                        context
                            .pending_tool_calls
                            .iter()
                            .all(|tool_call| context.outcomes.contains_key(&tool_call.id))
                    })),
                    actions: vec![collect_tool_outcomes_action()],
                },
            ],
            on: HashMap::from([
                (
                    "tool.detached".to_string(),
                    vec![Transition {
                        guard: Some(Arc::new(|context, event| {
                            let ToolEvent::Detached { tool_call_id, .. } = typed_tool_event(event)
                            else {
                                return false;
                            };
                            !context_of(context).outcomes.contains_key(&tool_call_id)
                        })),
                        actions: vec![assign_turn(|next, event| {
                            let ToolEvent::Detached {
                                tool_call_id,
                                text,
                            } = typed_tool_event(event)
                            else {
                                return;
                            };
                            let Some(tool_call) = next
                                .pending_tool_calls
                                .iter()
                                .find(|call| call.id == tool_call_id)
                                .cloned()
                            else {
                                return;
                            };
                            next.outcomes
                                .insert(tool_call_id, async_ack_outcome(&tool_call, &text));
                        })],
                        ..Default::default()
                    }],
                ),
                (
                    "tool.done".to_string(),
                    vec![Transition {
                        actions: vec![assign_turn(|next, event| {
                            let ToolEvent::Done {
                                tool_call_id,
                                result,
                            } = typed_tool_event(event)
                            else {
                                return;
                            };
                            next.outcomes
                                .insert(tool_call_id, ToolOutput::Succeeded { result });
                        })],
                        ..Default::default()
                    }],
                ),
                (
                    "tool.failed".to_string(),
                    vec![Transition {
                        actions: vec![assign_turn(|next, event| {
                            let ToolEvent::Failed {
                                tool_call_id,
                                error,
                            } = typed_tool_event(event)
                            else {
                                return;
                            };
                            next.outcomes
                                .insert(tool_call_id, ToolOutput::Failed { error });
                        })],
                        ..Default::default()
                    }],
                ),
                (
                    "tool.aborted".to_string(),
                    vec![Transition {
                        actions: vec![assign_turn(|next, event| {
                            let ToolEvent::Aborted { tool_call_id } = typed_tool_event(event) else {
                                return;
                            };
                            next.outcomes.insert(tool_call_id, ToolOutput::Aborted);
                        })],
                        ..Default::default()
                    }],
                ),
            ]),
            states: HashMap::from([
                (
                    "running".to_string(),
                    StateNode {
                        on: HashMap::from([(
                            "turn.abort".to_string(),
                            vec![Transition {
                                target: Some("aborting".to_string()),
                                actions: vec![assign_turn(|next, _| {
                                    next.outcome = Some(TurnOutcome::Aborted)
                                })],
                                ..Default::default()
                            }],
                        )]),
                        ..Default::default()
                    },
                ),
                (
                    "aborting".to_string(),
                    StateNode {
                        after: vec![(
                            "abortGrace".to_string(),
                            Transition {
                                target: Some("#turn.aborted".to_string()),
                                actions: vec![collect_aborted()],
                                ..Default::default()
                            },
                        )],
                        on: HashMap::from([(
                            "turn.abort".to_string(),
                            vec![Transition {
                                target: Some("#turn.aborted".to_string()),
                                actions: vec![collect_aborted()],
                                ..Default::default()
                            }],
                        )]),
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        },
    );
    states.insert(
        "draining".to_string(),
        StateNode {
            entry: vec![custom(|_, args| {
                args.self_
                    .forward_to_parent(XEvent::new("turn.drain", Value::Null));
            })],
            on: HashMap::from([
                (
                    "turn.notify".to_string(),
                    vec![
                        Transition {
                            target: Some("done".to_string()),
                            guard: Some(Arc::new(|context, _| context_of(context).paused)),
                            actions: vec![
                                assign_turn(|next, event| {
                                    next.produced.extend(notify_messages(event));
                                }),
                                signal_reminders_consumed(),
                            ],
                            ..Default::default()
                        },
                        Transition {
                            target: Some("failed".to_string()),
                            guard: Some(Arc::new(|context, event| {
                                notify_messages(event).is_empty()
                                    && max_steps_exceeded(context_of(context))
                            })),
                            actions: vec![assign_turn(|next, _| {
                                let max_steps = next.input.max_steps.unwrap_or(0);
                                let error = MaxStepsExceededError::new(max_steps, None);
                                next.outcome = Some(TurnOutcome::Failed);
                                next.error = Some(json!({
                                    "name": error.name(),
                                    "code": error.code,
                                    "message": error.to_string(),
                                    "details": error.details,
                                }));
                            })],
                            ..Default::default()
                        },
                        Transition {
                            target: Some("gating".to_string()),
                            actions: vec![
                                assign_turn(|next, event| {
                                    let messages = notify_messages(event);
                                    next.steps = if messages.is_empty() {
                                        next.steps + 1
                                    } else {
                                        1
                                    };
                                    next.produced.extend(messages);
                                    next.attempt = 1;
                                    next.applied_recoveries.clear();
                                    next.attempt_message_override = None;
                                }),
                                signal_reminders_consumed(),
                            ],
                            ..Default::default()
                        },
                    ],
                ),
                (
                    "turn.abort".to_string(),
                    vec![Transition {
                        target: Some("aborted".to_string()),
                        actions: vec![assign_turn(|next, _| {
                            next.outcome = Some(TurnOutcome::Aborted)
                        })],
                        ..Default::default()
                    }],
                ),
            ]),
            ..Default::default()
        },
    );
    for name in ["done", "failed", "aborted"] {
        states.insert(
            name.to_string(),
            StateNode {
                final_: true,
                ..Default::default()
            },
        );
    }
    states
}
