use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::human::agent::context_usage::{ContextUsagePrefix, estimate_used_context_tokens};
use crate::human::agent::errors::MaxStepsExceededError;
use crate::human::agent::origin::PromptOrigin;
use crate::human::llm::empty_response::empty_response_error;
use crate::human::llm::errors::{LlmErrorMessage, is_abort_error, to_llm_error_message};
use crate::human::llm::finish_reason::{FinishInfo, NO_FINISH};
use crate::human::llm::message::{
    AssistantMessage, ContentPart, Message, MessageAccumulator, StreamedMessagePart, SystemMessage,
    ToolCall, ToolMessage, create_tool_message, salvage_interrupted_message,
};
pub use crate::human::llm::message::UserMessage;
use crate::human::llm::model::LlmModel;
use crate::human::llm::requester::actor::{LlmEvent, MessageResolveContext, MessageResolver};
use crate::human::llm::requester::recovery::{
    LlmRecovery, LlmRecoveryContext, LlmRecoveryProposalRecord, LlmRecoveryRecord,
};
use crate::human::llm::requester::requester::{
    LlmRequestConfig, LlmRequestContent, LlmRequestControl, LlmRequestEvent, LlmRequester,
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
use crate::human::tool::machine::ToolOutput;
use crate::human::utils::abort::{AbortScope, create_abort_scope, with_abort};
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
pub enum TurnToolEvent {
    Detached { tool_call_id: String, text: String },
    Done { tool_call_id: String, result: ToolResult },
    Failed { tool_call_id: String, error: Value },
    Aborted { tool_call_id: String },
}

pub enum TurnEvent {
    Llm(LlmEvent),
    Tool(TurnToolEvent),
    Notify { messages: Vec<HistoryMessage> },
    Pause,
    Continue,
    Abort { reason: Option<Value> },
    Steer,
    FailureEvaluated {
        cause: LlmRequestEvent,
        proposal: Option<LlmRecoveryProposalRecord>,
    },
}

#[derive(Clone)]
pub enum TurnLlmEvent {
    Event(LlmEvent),
    Done { entry: AssistantEntry },
}

#[derive(Clone)]
pub enum TurnSignal {
    StepStarted { step: u64 },
    SpawnTools { tool_calls: Vec<ToolCall> },
    Drain,
    RemindersConsumed { reminders: Vec<HistoryMessage> },
}

#[derive(Clone)]
pub enum TurnParentEvent {
    Llm(TurnLlmEvent),
    Signal(TurnSignal),
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
    requester: Arc<dyn LlmRequester>,
    options: CreateTurnMachineOptions,
}

impl TurnMachineLogic {
    pub fn start(
        &self,
        input: TurnInput,
        parent: Arc<dyn Fn(TurnParentEvent) + Send + Sync>,
    ) -> Arc<TurnActor> {
        let mut tool_call_ids = ToolCallIdNormalizer::new();
        let seed = to_input_messages(&input.history)
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
            .collect::<Vec<_>>();
        tool_call_ids.seed_from(&seed);
        let accumulator = Arc::new(Mutex::new(create_history_accumulator(
            Some(model_meta(&input.request.model)),
            Some(tool_call_ids.clone()),
        )));
        let actor = Arc::new(TurnActor {
            requester: self.requester.clone(),
            options: self.options.clone(),
            context: Mutex::new(TurnMachineContext {
                input,
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
            }),
            state: Mutex::new(TurnState::Gating),
            parent,
            listeners: Mutex::new(Vec::new()),
            finish_listeners: Mutex::new(Vec::new()),
            finish_notified: AtomicBool::new(false),
        });
        actor.enter_gating();
        actor
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Gating,
    Thinking,
    Retrying,
    ActingRunning,
    ActingAborting,
    Draining,
    Done,
    Failed,
    Aborted,
}

pub struct TurnActor {
    requester: Arc<dyn LlmRequester>,
    options: CreateTurnMachineOptions,
    context: Mutex<TurnMachineContext>,
    state: Mutex<TurnState>,
    parent: Arc<dyn Fn(TurnParentEvent) + Send + Sync>,
    listeners: Mutex<Vec<Arc<dyn Fn(TurnParentEvent) + Send + Sync>>>,
    finish_listeners: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    finish_notified: AtomicBool,
}

impl TurnActor {
    pub fn send(self: &Arc<Self>, event: TurnEvent) {
        if self.is_final() {
            return;
        }
        match event {
            TurnEvent::Pause => {
                self.context.lock().paused = true;
            }
            TurnEvent::Continue => {
                self.context.lock().paused = false;
            }
            TurnEvent::Llm(event) if self.state() == TurnState::Thinking => {
                self.handle_llm(event);
            }
            TurnEvent::FailureEvaluated { cause, proposal }
                if self.state() == TurnState::Thinking =>
            {
                self.handle_failure(cause, proposal);
            }
            TurnEvent::Tool(event) if self.is_acting() => {
                self.handle_tool(event);
            }
            TurnEvent::Notify { messages } if self.state() == TurnState::Draining => {
                self.handle_notify(messages);
            }
            TurnEvent::Abort { reason } => self.handle_abort(reason),
            TurnEvent::Steer => self.handle_steer(),
            _ => {}
        }
        self.notify_finished();
    }

    pub fn on_finish(&self, listener: Arc<dyn Fn() + Send + Sync>) {
        self.finish_listeners.lock().push(listener);
        self.notify_finished();
    }

    fn notify_finished(&self) {
        if !self.is_final() || self.finish_notified.swap(true, Ordering::AcqRel) {
            return;
        }
        for listener in self.finish_listeners.lock().iter() {
            listener();
        }
    }

    pub fn subscribe(&self, listener: Arc<dyn Fn(TurnParentEvent) + Send + Sync>) {
        self.listeners.lock().push(listener);
    }

    pub fn snapshot(&self) -> (TurnMachineContext, TurnState) {
        (self.context.lock().clone(), self.state())
    }

    pub fn output(&self) -> Option<TurnOutput> {
        if !self.is_final() {
            return None;
        }
        let context = self.context.lock();
        match context.outcome {
            Some(TurnOutcome::Failed) => Some(TurnOutput::Failed {
                error: context.error.clone().unwrap_or(Value::Null),
                produced: context.produced.clone(),
            }),
            Some(TurnOutcome::Aborted) => Some(TurnOutput::Aborted {
                produced: context.produced.clone(),
            }),
            None => Some(TurnOutput::Done {
                produced: context.produced.clone(),
            }),
        }
    }

    fn state(&self) -> TurnState {
        *self.state.lock()
    }

    fn is_final(&self) -> bool {
        matches!(self.state(), TurnState::Done | TurnState::Failed | TurnState::Aborted)
    }

    fn is_acting(&self) -> bool {
        matches!(self.state(), TurnState::ActingRunning | TurnState::ActingAborting)
    }

    fn emit(&self, event: TurnParentEvent) {
        for listener in self.listeners.lock().iter() {
            listener(event.clone());
        }
        (self.parent)(event);
    }

    fn enter_gating(self: &Arc<Self>) {
        if self.is_final() {
            return;
        }
        *self.state.lock() = TurnState::Gating;
        let Some(on_before_step) = &self.options.on_before_step else {
            self.enter_thinking();
            return;
        };
        let on_before_step = on_before_step.clone();
        let (messages, request) = {
            let context = self.context.lock();
            (
                [context.input.history.clone(), context.produced.clone()].concat(),
                context.input.request.clone(),
            )
        };
        let this = self.clone();
        tokio::spawn(async move {
            if on_before_step(TurnBeforeStepContext { messages, request })
                .await
                .is_ok()
            {
                if this.state() == TurnState::Gating {
                    this.enter_thinking();
                }
            } else if this.state() == TurnState::Gating {
                *this.state.lock() = TurnState::Done;
                this.notify_finished();
            }
        });
    }

    fn enter_thinking(self: &Arc<Self>) {
        if self.is_final() {
            return;
        }
        *self.state.lock() = TurnState::Thinking;
        let (request, content, signal) = {
            let mut context = self.context.lock();
            context.accumulator = Arc::new(Mutex::new(create_history_accumulator(
                Some(model_meta(&context.input.request.model)),
                Some(context.tool_call_ids.clone()),
            )));
            context.llm_scope = context
                .input
                .parent_signal
                .as_ref()
                .map(with_abort)
                .unwrap_or_else(create_abort_scope);
            context.step += 1;
            let entries = [context.input.history.clone(), context.produced.clone()].concat();
            let messages = context
                .attempt_message_override
                .clone()
                .unwrap_or_else(|| to_input_messages(&entries));
            let used_context_tokens = estimate_used_context_tokens(
                &entries,
                Some(&ContextUsagePrefix {
                    system_prompt: context.input.request.system_prompt.clone(),
                    tools: context.input.request.tools.clone(),
                }),
            );
            (
                context.input.request.clone(),
                LlmRequestContent {
                    messages,
                    used_context_tokens: Some(used_context_tokens),
                },
                context.llm_scope.signal.clone(),
            )
        };
        let step = self.context.lock().step;
        self.emit(TurnParentEvent::Signal(TurnSignal::StepStarted { step }));
        let this = self.clone();
        let event_actor = self.clone();
        let on_event = Arc::new(move |event| {
            if matches!(event, LlmRequestEvent::Sent) {
                let recovery = event_actor
                    .context
                    .lock()
                    .applied_recoveries
                    .last()
                    .cloned();
                event_actor.send(TurnEvent::Llm(LlmEvent::Sent { recovery }));
            } else {
                event_actor.send(TurnEvent::Llm(LlmEvent::Request(event)));
            }
        });
        tokio::spawn(async move {
            let result = async {
                let mut config = request;
                if let Some(provider) = &config.credential_provider {
                    let credential = provider.resolve().await?;
                    config.model = crate::human::credentials::credentials::apply_credential(
                        &config.model,
                        credential,
                    );
                }
                let mut messages = content.messages.clone();
                for resolver in &this.options.message_resolvers {
                    messages = resolver
                        .resolve(
                            messages,
                            MessageResolveContext {
                                model: config.model.clone(),
                                signal: signal.clone(),
                            },
                        )
                        .await?;
                }
                this.requester
                    .generate(
                        config,
                        LlmRequestContent {
                            messages,
                            ..content
                        },
                        LlmRequestControl {
                            signal: signal.clone(),
                            on_event,
                        },
                    )
                    .await
            }
            .await;
            if let Err(error) = result {
                if is_abort_error(&error) || signal.is_cancelled() {
                    return;
                }
                let message = to_llm_error_message(&error);
                this.send(TurnEvent::Llm(LlmEvent::Request(
                    LlmRequestEvent::FailedRemote {
                        error: message,
                        raw_error: Some(Value::String(error.to_string())),
                    },
                )));
            }
        });
    }

    fn discard_attempt_stream(&self) {
        let (tool_call_ids, model) = {
            let context = self.context.lock();
            context.accumulator.lock().rollback();
            (context.tool_call_ids.clone(), context.input.request.model.clone())
        };
        self.context.lock().accumulator = Arc::new(Mutex::new(create_history_accumulator(
            Some(model_meta(&model)),
            Some(tool_call_ids),
        )));
    }

    fn handle_llm(self: &Arc<Self>, event: LlmEvent) {
        match event {
            LlmEvent::Sent { .. } => {
                let recovery = self.context.lock().applied_recoveries.last().cloned();
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Sent {
                    recovery,
                })));
                self.discard_attempt_stream();
            }
            LlmEvent::Request(LlmRequestEvent::RequestRetrying) => {
                self.discard_attempt_stream();
            }
            LlmEvent::Request(LlmRequestEvent::StreamingHeaders { headers }) => {
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                    LlmRequestEvent::StreamingHeaders {
                        headers: headers.clone(),
                    },
                ))));
                self.context.lock().accumulator.lock().push_headers(headers);
            }
            LlmEvent::Request(LlmRequestEvent::StreamingPart { part }) => {
                let part = self.context.lock().accumulator.lock().push(part);
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                    LlmRequestEvent::StreamingPart { part },
                ))));
            }
            LlmEvent::Request(LlmRequestEvent::StreamingUsage { usage }) => {
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                    LlmRequestEvent::StreamingUsage {
                        usage: usage.clone(),
                    },
                ))));
                self.context.lock().accumulator.lock().push_usage(usage);
            }
            LlmEvent::Request(LlmRequestEvent::StreamingFinish { finish }) => {
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                    LlmRequestEvent::StreamingFinish {
                        finish: finish.clone(),
                    },
                ))));
                self.context.lock().accumulator.lock().push_finish(finish);
            }
            LlmEvent::Request(LlmRequestEvent::StreamingMessageId { message_id }) => {
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                    LlmRequestEvent::StreamingMessageId {
                        message_id: message_id.clone(),
                    },
                ))));
                self.context
                    .lock()
                    .accumulator
                    .lock()
                    .push_message_id(message_id);
            }
            LlmEvent::Request(LlmRequestEvent::Done) => self.handle_done(),
            LlmEvent::Request(LlmRequestEvent::FailedSyntax { error }) => {
                self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
                    LlmRequestEvent::FailedSyntax {
                        error: error.clone(),
                    },
                ))));
                self.fail(serde_json::to_value(error).unwrap_or(Value::Null));
            }
            LlmEvent::Request(event @ LlmRequestEvent::FailedRemote { .. }) => {
                self.handle_failure(event, None);
            }
            LlmEvent::Retrying { .. } | LlmEvent::Recovering { .. } | LlmEvent::Request(_) => {}
        }
    }

    fn handle_done(self: &Arc<Self>) {
        let entry = self.context.lock().accumulator.lock().finish(Some(AssistantMetaInput {
            source: Some("llm".to_string()),
            ..Default::default()
        }));
        if !entry.message.tool_calls.is_empty() {
            self.emit(TurnParentEvent::Llm(TurnLlmEvent::Done {
                entry: entry.clone(),
            }));
            {
                let mut context = self.context.lock();
                context.produced.push(HistoryMessage::Assistant(entry.clone()));
                context.pending_tool_calls = entry.message.tool_calls.clone();
            }
            *self.state.lock() = TurnState::ActingRunning;
            let calls = self.context.lock().pending_tool_calls.clone();
            self.emit(TurnParentEvent::Signal(TurnSignal::SpawnTools {
                tool_calls: calls,
            }));
            self.check_tool_completion();
            return;
        }
        let (model, finish) = {
            let context = self.context.lock();
            (
                context.input.request.model.clone(),
                entry.meta.as_ref().and_then(|meta| meta.finish.clone()).unwrap_or(NO_FINISH),
            )
        };
        if let Some(error) = empty_response_error(&entry.message, &model, &finish) {
            self.handle_failure(
                LlmRequestEvent::FailedRemote {
                    error,
                    raw_error: None,
                },
                None,
            );
            return;
        }
        self.emit(TurnParentEvent::Llm(TurnLlmEvent::Done {
            entry: entry.clone(),
        }));
        self.context.lock().produced.push(HistoryMessage::Assistant(entry));
        *self.state.lock() = TurnState::Done;
    }

    fn propose_recovery(&self, cause: &LlmRequestEvent) -> Option<LlmRecoveryProposalRecord> {
        let LlmRequestEvent::FailedRemote { error, .. } = cause else {
            return None;
        };
        let recovery = self.options.recovery.as_ref()?;
        let (messages, applied_recoveries, credential_provider) = {
            let context = self.context.lock();
            let entries = [context.input.history.clone(), context.produced.clone()].concat();
            (
                to_input_messages(&entries),
                context.applied_recoveries.clone(),
                context.input.request.credential_provider.clone(),
            )
        };
        let proposal = recovery.propose(LlmRecoveryContext {
            error: error.clone(),
            messages: messages.clone(),
            applied_recoveries,
            credential_provider,
        })?;
        if proposal.attempt_message_override.as_ref() == Some(&messages) {
            return None;
        }
        Some(proposal)
    }

    fn handle_failure(
        self: &Arc<Self>,
        cause: LlmRequestEvent,
        proposal: Option<LlmRecoveryProposalRecord>,
    ) {
        let LlmRequestEvent::FailedRemote { error, raw_error } = &cause else {
            return;
        };
        let proposal = proposal.or_else(|| self.propose_recovery(&cause));
        if let Some(proposal) = proposal {
            self.context.lock().accumulator.lock().rollback();
            if let Some(before) = proposal.before_next_attempt {
                before();
            }
            let record = LlmRecoveryRecord {
                strategy: proposal.strategy,
                action: proposal.action,
            };
            {
                let mut context = self.context.lock();
                context.applied_recoveries.push(record.clone());
                if proposal.attempt_message_override.is_some() {
                    context.attempt_message_override = proposal.attempt_message_override;
                }
                context.attempt = 1;
            }
            let fields = retry_error_fields(error);
            self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(
                LlmEvent::Recovering {
                    strategy: record.strategy,
                    action: record.action,
                    error_name: fields.error_name,
                    error_message: fields.error_message,
                    status_code: fields.status_code,
                },
            )));
            self.enter_thinking();
            return;
        }
        let attempt = self.context.lock().attempt;
        if should_retry(self.options.retry.as_ref(), attempt, error) {
            self.context.lock().accumulator.lock().rollback();
            let delay = read_retry_after_ms(error)
                .map(|delay| delay as u64)
                .unwrap_or_else(|| retry_backoff_delay(attempt - 1));
            self.context.lock().delay_ms = delay;
            let fields = retry_error_fields(error);
            self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Retrying {
                failed_attempt: attempt,
                next_attempt: attempt + 1,
                max_attempts: resolve_max_attempts(self.options.retry.as_ref()),
                delay_ms: delay,
                error_name: fields.error_name,
                error_message: fields.error_message,
                status_code: fields.status_code,
            })));
            *self.state.lock() = TurnState::Retrying;
            self.context.lock().attempt += 1;
            let this = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                if this.state() == TurnState::Retrying {
                    this.enter_thinking();
                }
            });
            return;
        }
        self.emit(TurnParentEvent::Llm(TurnLlmEvent::Event(LlmEvent::Request(
            cause.clone(),
        ))));
        self.fail(raw_error.clone().unwrap_or_else(|| {
            serde_json::to_value(error).unwrap_or(Value::Null)
        }));
    }

    fn handle_tool(self: &Arc<Self>, event: TurnToolEvent) {
        match event {
            TurnToolEvent::Detached { tool_call_id, text } => {
                let mut context = self.context.lock();
                if context.outcomes.contains_key(&tool_call_id) {
                    return;
                }
                if let Some(call) = context
                    .pending_tool_calls
                    .iter()
                    .find(|call| call.id == tool_call_id)
                    .cloned()
                {
                    context
                        .outcomes
                        .insert(tool_call_id, async_ack_outcome(&call, &text));
                }
            }
            TurnToolEvent::Done { tool_call_id, result } => {
                self.context
                    .lock()
                    .outcomes
                    .insert(tool_call_id, ToolOutput::Succeeded { result });
            }
            TurnToolEvent::Failed { tool_call_id, error } => {
                self.context.lock().outcomes.insert(
                    tool_call_id,
                    ToolOutput::Failed {
                        error: error.as_str().map(str::to_string).unwrap_or_else(|| error.to_string()),
                    },
                );
            }
            TurnToolEvent::Aborted { tool_call_id } => {
                self.context
                    .lock()
                    .outcomes
                    .insert(tool_call_id, ToolOutput::Aborted);
            }
        }
        self.check_tool_completion();
    }

    fn check_tool_completion(self: &Arc<Self>) {
        if !self.is_acting() {
            return;
        }
        let (complete, aborted) = {
            let context = self.context.lock();
            (
                !context.pending_tool_calls.is_empty()
                    && context
                        .pending_tool_calls
                        .iter()
                        .all(|call| context.outcomes.contains_key(&call.id)),
                context.outcome == Some(TurnOutcome::Aborted),
            )
        };
        if !complete {
            return;
        }
        self.collect_tool_outcomes();
        if aborted {
            *self.state.lock() = TurnState::Aborted;
        } else {
            self.enter_draining();
        }
    }

    fn collect_tool_outcomes(&self) {
        let mut context = self.context.lock();
        let produced = context
            .pending_tool_calls
            .iter()
            .filter_map(|call| {
                context
                    .outcomes
                    .get(&call.id)
                    .map(|output| tool_outcome_entry(call, output))
            })
            .collect::<Vec<_>>();
        context.produced.extend(produced);
        context.pending_tool_calls.clear();
        context.outcomes.clear();
    }

    fn collect_aborted(self: &Arc<Self>) {
        {
            let mut context = self.context.lock();
            for call in &context.pending_tool_calls {
                context
                    .outcomes
                    .entry(call.id.clone())
                    .or_insert(ToolOutput::Aborted);
            }
        }
        self.collect_tool_outcomes();
        self.context.lock().outcome = Some(TurnOutcome::Aborted);
        *self.state.lock() = TurnState::Aborted;
        self.notify_finished();
    }

    fn enter_draining(self: &Arc<Self>) {
        *self.state.lock() = TurnState::Draining;
        self.emit(TurnParentEvent::Signal(TurnSignal::Drain));
    }

    fn handle_notify(self: &Arc<Self>, messages: Vec<HistoryMessage>) {
        if self.context.lock().paused {
            self.context.lock().produced.extend(messages.clone());
            self.signal_reminders_consumed(&messages);
            *self.state.lock() = TurnState::Done;
            return;
        }
        if messages.is_empty() && self.max_steps_exceeded() {
            let max_steps = self.context.lock().input.max_steps.unwrap_or(0);
            let error = MaxStepsExceededError::new(max_steps, None);
            self.fail(json!({
                "name": error.name(),
                "code": error.code,
                "message": error.to_string(),
                "details": error.details,
            }));
            return;
        }
        {
            let mut context = self.context.lock();
            context.produced.extend(messages.clone());
            context.steps = if messages.is_empty() {
                context.steps + 1
            } else {
                1
            };
            context.attempt = 1;
            context.applied_recoveries.clear();
            context.attempt_message_override = None;
        }
        self.signal_reminders_consumed(&messages);
        self.enter_gating();
    }

    fn signal_reminders_consumed(&self, messages: &[HistoryMessage]) {
        let reminders = messages
            .iter()
            .filter(|entry| entry.meta_source() == Some("reminder"))
            .cloned()
            .collect::<Vec<_>>();
        if !reminders.is_empty() {
            self.emit(TurnParentEvent::Signal(TurnSignal::RemindersConsumed {
                reminders,
            }));
        }
    }

    fn max_steps_exceeded(&self) -> bool {
        let context = self.context.lock();
        context
            .input
            .max_steps
            .is_some_and(|max_steps| max_steps > 0 && context.steps >= max_steps)
    }

    fn handle_abort(self: &Arc<Self>, _reason: Option<Value>) {
        match self.state() {
            TurnState::Thinking => {
                let scope = self.context.lock().llm_scope.clone();
                scope.abort();
                self.salvage(true);
            }
            TurnState::Gating | TurnState::Retrying | TurnState::Draining => {
                self.context.lock().outcome = Some(TurnOutcome::Aborted);
                *self.state.lock() = TurnState::Aborted;
            }
            TurnState::ActingRunning => {
                self.context.lock().outcome = Some(TurnOutcome::Aborted);
                *self.state.lock() = TurnState::ActingAborting;
                self.check_tool_completion();
                if self.state() != TurnState::ActingAborting {
                    return;
                }
                let delay = self.options.abort_grace_ms.unwrap_or(2_500);
                let this = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if this.state() == TurnState::ActingAborting {
                        this.collect_aborted();
                    }
                });
            }
            TurnState::ActingAborting => self.collect_aborted(),
            _ => {}
        }
    }

    fn handle_steer(self: &Arc<Self>) {
        match self.state() {
            TurnState::Thinking => {
                if self.context.lock().accumulator.lock().has_tool_call() {
                    return;
                }
                let scope = self.context.lock().llm_scope.clone();
                scope.abort();
                self.salvage(false);
                self.enter_draining();
            }
            TurnState::Retrying => self.enter_draining(),
            _ => {}
        }
    }

    fn salvage(&self, aborted: bool) {
        let partial = self.context.lock().accumulator.lock().finish(Some(AssistantMetaInput {
            source: Some("salvaged".to_string()),
            ..Default::default()
        }));
        if let Some(salvaged) = salvage_interrupted_message(&partial.message) {
            self.context.lock().produced.push(HistoryMessage::Assistant(
                create_assistant_entry(salvaged, partial.meta.unwrap_or_default()),
            ));
        }
        if aborted {
            self.context.lock().outcome = Some(TurnOutcome::Aborted);
            *self.state.lock() = TurnState::Aborted;
        }
    }

    fn fail(&self, error: Value) {
        let mut context = self.context.lock();
        context.outcome = Some(TurnOutcome::Failed);
        context.error = Some(error);
        *self.state.lock() = TurnState::Failed;
    }
}

pub fn create_turn_machine(
    requester: Arc<dyn LlmRequester>,
    options: Option<CreateTurnMachineOptions>,
) -> TurnMachineLogic {
    TurnMachineLogic {
        requester,
        options: options.unwrap_or_default(),
    }
}
