use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::errors::errors::SharedError;
use crate::_base::utils::abort::AbortSignal;
use crate::agent::context_memory::types::ContextMessage;
use crate::agent::r#loop::errors::LoopErrors;
use crate::agent::r#loop::machine::UserEntry;
use crate::agent::r#loop::machine::engine::{
    MachineEngine, MachineEngineAttachBundle, MachineEngineAttachRef, MachineEngineRetrySnapshot,
    MachineEngineToolCallSnapshot,
};
use crate::hooks::OrderedHookSlot;
use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::message::ContentPart;
use crate::human::llm::usage::TokenUsage;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, Error2Options};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivityTurnSnapshot {
    pub turn_id: u64,
    pub phase: AgentActivityPhase,
    pub step: u32,
    pub ending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ending_reason: Option<AgentActivityEndingReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<MachineEngineRetrySnapshot>,
    pub active_tool_calls: Vec<MachineEngineToolCallSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityPhase {
    Running,
    ToolCall,
    Retrying,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentActivityEndingReason {
    Aborted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentActivitySnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<AgentActivityTurnSnapshot>,
}

#[derive(Clone)]
pub struct LoopSnapshot {
    pub state: LoopState,
    pub active_turn_id: Option<u64>,
    pub active_prompt_id: Option<String>,
    pub queue: Vec<UserEntry>,
    pub notification_count: usize,
    pub paused: bool,
    pub has_pending_requests: bool,
    pub turn: Option<AgentActivityTurnSnapshot>,
    pub active_trace_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopState {
    Idle,
    Running,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextPart {
    #[serde(rename = "type")]
    pub type_: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

pub type LoopErrorCode = &'static str;
pub type LoopError = Error2;

pub fn create_max_steps_exceeded_error(max_steps: u32, message: Option<String>) -> LoopError {
    let message = message.unwrap_or_else(|| {
        format!(
            "Turn exceeded maxSteps={max_steps}. If max_steps_per_turn is too small, raise it in config.toml (loop_control.max_steps_per_turn), or run \"/update-config\" to update it, then \"/reload\"."
        )
    });
    Error2::with_options(
        LoopErrors::LOOP_MAX_STEPS_EXCEEDED,
        message,
        Error2Options {
            details: Some(Map::from_iter([(
                "maxSteps".to_string(),
                Value::from(max_steps),
            )])),
            name: Some("LoopError".to_string()),
            ..Default::default()
        },
    )
}

pub fn is_max_steps_exceeded_error(error: &(dyn std::error::Error + 'static)) -> bool {
    error
        .downcast_ref::<Error2>()
        .is_some_and(|error| error.code().as_str() == LoopErrors::LOOP_MAX_STEPS_EXCEEDED)
}

#[derive(Clone)]
pub struct BeforeStepContext {
    pub turn_id: u64,
    pub step: u32,
    pub first_step_of_turn: bool,
    pub signal: AbortSignal,
}

#[derive(Clone)]
pub struct AfterStepContext {
    pub turn_id: u64,
    pub step: u32,
    pub first_step_of_turn: bool,
    pub signal: AbortSignal,
    pub usage: TokenUsage,
    pub finish_reason: FinishReason,
    pub stop_turn: bool,
}

#[derive(Clone)]
pub struct LoopErrorContext {
    pub turn_id: u64,
    pub step: Option<u32>,
    pub step_id: Option<String>,
    pub signal: AbortSignal,
    pub error: SharedError,
    pub retry: Arc<dyn Fn() + Send + Sync>,
}

#[async_trait]
pub trait LoopErrorHandler: Send + Sync {
    fn id(&self) -> &str;
    fn matches(&self, context: &LoopErrorContext) -> bool;
    async fn handle(&self, context: LoopErrorContext) -> Result<Option<bool>, WireAsyncError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopErrorHandlerRegistrationOptions {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompletedTurnResult {
    pub steps: u32,
    pub truncated: bool,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub enum LoopRunResult {
    Completed(CompletedTurnResult),
    Failed { steps: u32, error: SharedError },
    Cancelled {
        steps: u32,
        reason: Option<SharedError>,
    },
}

pub type TurnResult = LoopRunResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone)]
pub struct Turn {
    pub id: Option<u64>,
    pub state: Option<TurnState>,
    pub signal: AbortSignal,
    pub ready: Shared<BoxFuture<'static, ()>>,
    pub result: Shared<BoxFuture<'static, TurnResult>>,
    cancel_fn: Arc<dyn Fn(Option<SharedError>) -> bool + Send + Sync>,
}

impl Turn {
    pub fn new(
        id: Option<u64>,
        state: Option<TurnState>,
        signal: AbortSignal,
        ready: Shared<BoxFuture<'static, ()>>,
        result: Shared<BoxFuture<'static, TurnResult>>,
        cancel: Arc<dyn Fn(Option<SharedError>) -> bool + Send + Sync>,
    ) -> Self {
        Self {
            id,
            state,
            signal,
            ready,
            result,
            cancel_fn: cancel,
        }
    }

    pub fn cancel(&self, reason: Option<SharedError>) -> bool {
        (self.cancel_fn)(reason)
    }
}

#[derive(Clone, Default)]
pub struct LoopSubmitOptions {
    pub steer_if_active: Option<bool>,
    pub on_materialize: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopSubmitResult {
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopCancelTarget {
    pub turn_id: Option<u64>,
    pub prompt_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptState {
    Pending,
    Running,
    Steered,
    Completed,
    Failed,
    Cancelled,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptTerminalState {
    Completed,
    Failed,
    Cancelled,
    Blocked,
}

#[derive(Debug, Clone)]
pub struct PromptCompletion {
    pub prompt_id: String,
    pub result: Option<TurnResult>,
    pub state: PromptTerminalState,
}

#[derive(Clone)]
pub struct PromptSnapshot {
    pub id: String,
    pub user_message_id: String,
    pub created_at: String,
    pub state: PromptState,
    pub message: ContextMessage,
}

#[derive(Clone)]
pub struct PromptHandle {
    pub id: String,
    pub user_message_id: String,
    pub created_at: String,
    pub state: PromptState,
    pub message: ContextMessage,
    pub launched: Shared<BoxFuture<'static, Option<Arc<Turn>>>>,
    pub completion: Shared<BoxFuture<'static, PromptCompletion>>,
}

impl PromptHandle {
    pub fn snapshot(&self) -> PromptSnapshot {
        PromptSnapshot {
            id: self.id.clone(),
            user_message_id: self.user_message_id.clone(),
            created_at: self.created_at.clone(),
            state: self.state,
            message: self.message.clone(),
        }
    }
}

#[derive(Clone)]
pub struct PromptPayload {
    pub input: Vec<ContentPart>,
    pub prompt_id: Option<String>,
}

#[derive(Clone)]
pub struct SteerPayload {
    pub input: Vec<ContentPart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptLaunchResult {
    pub turn_id: u64,
}

#[derive(Clone)]
pub struct SteerResult {
    pub turn_id: Option<u64>,
    pub prompt_id: String,
    pub created_at: String,
    pub state: SteerState,
    pub steered: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SteerState {
    Running,
    Queued,
    Blocked,
}

#[derive(Clone)]
pub struct PromptSubmitContext {
    pub prompt_message: ContextMessage,
    pub is_steer: bool,
    pub hook_parts: Vec<TextPart>,
    pub block: bool,
}

#[derive(Clone, Default)]
pub struct LoopNotify {
    pub message: Option<ContextMessage>,
    pub turn_scoped: Option<bool>,
    pub bypass_max_steps: Option<bool>,
    pub on_consume: Option<Arc<dyn Fn() + Send + Sync>>,
    pub on_drop: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
pub struct LoopNotifyHandle {
    pub dropped: bool,
    on_drop: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl LoopNotifyHandle {
    pub fn new(on_drop: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            dropped: false,
            on_drop,
        }
    }

    pub fn drop(&mut self) {
        if self.dropped {
            return;
        }
        self.dropped = true;
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
        }
    }
}

#[derive(Clone, Default)]
pub struct LoopHooks {
    pub on_will_begin_step: OrderedHookSlot<BeforeStepContext>,
    pub on_did_finish_step: OrderedHookSlot<AfterStepContext>,
    pub on_before_submit_prompt: OrderedHookSlot<PromptSubmitContext>,
}

#[async_trait]
pub trait IAgentLoopService: Send + Sync {
    fn submit(&self, input: UserEntry, options: Option<LoopSubmitOptions>) -> LoopSubmitResult;
    async fn steer(&self, prompt_ids: &[String]) -> Result<(), WireAsyncError>;
    fn cancel(&self, target: Option<LoopCancelTarget>, reason: Option<SharedError>) -> bool;
    fn snapshot(&self) -> LoopSnapshot;
    async fn settled(&self);
    fn try_acquire_quiescence(&self) -> Option<DisposableRef>;
    fn try_acquire_history_snapshot(&self) -> Option<DisposableRef>;
    fn notify(&self, note: Option<LoopNotify>) -> LoopNotifyHandle;
    fn build_attach_bundle(&self) -> MachineEngineAttachBundle;
    fn attach_engine(
        &self,
        reference: MachineEngineAttachRef,
        bundle: MachineEngineAttachBundle,
    ) -> Arc<MachineEngine>;
    async fn reset_machine_engine(&self) -> Result<(), WireAsyncError>;
    fn prompt_handle(&self, id: &str) -> Option<PromptHandle>;
    fn register_loop_error_handler(
        &self,
        handler: Arc<dyn LoopErrorHandler>,
        options: Option<LoopErrorHandlerRegistrationOptions>,
    ) -> DisposableRef;
    fn hooks(&self) -> LoopHooks;
}


pub static I_AGENT_LOOP_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentLoopService>>> =
    LazyLock::new(|| create_decorator("agentLoopService"));
