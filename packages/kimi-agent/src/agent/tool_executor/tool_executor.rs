use std::sync::Arc;

use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::event::Event;
use crate::_base::utils::abort::AbortSignal;
use crate::hooks::OrderedHookSlot;
use crate::human::llm::message::{ContentPart, ToolCall};
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::tool::tool_input_display::ToolInputDisplay;
use crate::wire::record::WireAsyncError;

use super::before_tool_execute_event::BeforeToolExecuteEventImpl;
use super::tool_executor_events::ToolUpdate;
use super::tool_hooks::{
    ResolvedToolExecutionHookContext, ToolDidExecuteContext, WillExecuteToolEvent,
};

pub use crate::tool::tool_contract::{ToolDelivery, ToolDisclosure, ToolResultSpill, ToolSource};
pub use crate::turn_loop::types::{ToolAccesses, tool_accesses_conflict};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExecutableToolOutput {
    Text(String),
    Parts(Vec<ContentPart>),
}

impl Default for ExecutableToolOutput {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl From<String> for ExecutableToolOutput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for ExecutableToolOutput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecutableToolResult {
    pub output: ExecutableToolOutput,
    pub is_error: Option<bool>,
    pub stop_turn: Option<bool>,
    pub stop_turn_reason: Option<String>,
    pub truncated: Option<bool>,
    pub note: Option<String>,
    pub delivery: Option<ToolDelivery>,
    pub spill: Option<ToolResultSpill>,
    pub spill_exempt: Option<bool>,
}

impl ExecutableToolResult {
    pub fn error(output: impl Into<ExecutableToolOutput>) -> Self {
        Self {
            output: output.into(),
            is_error: Some(true),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub output: ExecutableToolOutput,
    pub is_error: Option<bool>,
    pub stop_turn: Option<bool>,
    pub stop_turn_reason: Option<String>,
    pub truncated: Option<bool>,
    pub note: Option<String>,
    pub delivery: Option<ToolDelivery>,
    pub spill: Option<ToolResultSpill>,
    pub spill_exempt: Option<bool>,
    pub description: Option<String>,
    pub display: Option<ToolInputDisplay>,
    pub approval_rule: Option<String>,
    pub stop_batch_after_this: Option<bool>,
}

impl From<ExecutableToolResult> for ToolResult {
    fn from(result: ExecutableToolResult) -> Self {
        Self {
            output: result.output,
            is_error: result.is_error,
            stop_turn: result.stop_turn,
            stop_turn_reason: result.stop_turn_reason,
            truncated: result.truncated,
            note: result.note,
            delivery: result.delivery,
            spill: result.spill,
            spill_exempt: result.spill_exempt,
            ..Default::default()
        }
    }
}

impl ToolResult {
    pub fn into_executable(self) -> ExecutableToolResult {
        ExecutableToolResult {
            output: self.output,
            is_error: self.is_error,
            stop_turn: self.stop_turn,
            stop_turn_reason: self.stop_turn_reason,
            truncated: self.truncated,
            note: self.note,
            delivery: self.delivery,
            spill: self.spill,
            spill_exempt: self.spill_exempt,
        }
    }

    pub fn error(output: impl Into<ExecutableToolOutput>) -> Self {
        Self::from(ExecutableToolResult::error(output))
    }
}

#[derive(Clone)]
pub struct ExecutableToolContext {
    pub turn_id: u64,
    pub tool_call_id: String,
    pub trace: Option<LLMRequestTrace>,
    pub metadata: Option<Value>,
    pub signal: AbortSignal,
    pub steer_signal: Option<AbortSignal>,
    pub on_update: Option<Arc<dyn Fn(ToolUpdate) + Send + Sync>>,
    pub on_foreground_task_start: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

#[derive(Clone)]
pub struct RunnableToolExecution {
    pub accesses: Option<ToolAccesses>,
    pub display: Option<ToolInputDisplay>,
    pub description: Option<String>,
    pub stop_batch_after_this: Option<bool>,
    pub approval_rule: String,
    pub matches_rule: Option<Arc<dyn Fn(&str) -> bool + Send + Sync>>,
    pub execute: Arc<
        dyn Fn(ExecutableToolContext) -> BoxFuture<'static, Result<ExecutableToolResult, WireAsyncError>>
            + Send
            + Sync,
    >,
}

#[derive(Clone)]
pub enum ToolExecution {
    Runnable(RunnableToolExecution),
    Error(ExecutableToolResult),
}

pub trait ExecutableTool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Map<String, Value>;

    fn resolve_execution(
        &self,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolExecution, WireAsyncError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolInfoOrigin {
    Initial,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
    pub source: ToolSource,
    pub disclosure: Option<ToolDisclosure>,
    pub origin: ToolInfoOrigin,
}

pub trait IAgentToolRegistryService: Send + Sync {
    fn resolve(&self, name: &str) -> Option<Arc<dyn ExecutableTool>>;
    fn list(&self) -> Vec<ToolInfo>;
    fn list_live(&self) -> Option<Vec<ToolInfo>> {
        None
    }
    fn revision(&self) -> u64;
}

pub struct ToolTruncationInput {
    pub tool_name: String,
    pub tool_call_id: String,
    pub result: ToolResult,
}

pub trait IAgentToolResultTruncationService: Send + Sync {
    fn truncate_for_model(
        &self,
        input: ToolTruncationInput,
    ) -> BoxFuture<'static, Result<ToolResult, WireAsyncError>>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallStartedPayload {
    pub tool_call_id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ToolInputDisplay>,
}

#[derive(Clone)]
pub struct ToolExecutorExecuteOptions {
    pub signal: AbortSignal,
    pub steer_signal: Option<AbortSignal>,
    pub turn_id: u64,
    pub trace: Option<LLMRequestTrace>,
    pub on_tool_call: Option<Arc<dyn Fn(ToolCallStartedPayload) + Send + Sync>>,
}

#[derive(Debug, Clone)]
pub struct ToolExecutionResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
    pub duration_ms: u64,
}

pub type MissingToolDescriber = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type UnavailableToolDescriber = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallGuardInput {
    pub name: String,
    pub source: ToolSource,
}

pub type ToolCallGuard = Arc<dyn Fn(ToolCallGuardInput) -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallDupType {
    SameStep,
    CrossStep,
}

impl ToolCallDupType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameStep => "same_step",
            Self::CrossStep => "cross_step",
        }
    }
}

#[derive(Clone)]
pub struct ToolExecutorHooks {
    pub on_execute_tool: OrderedHookSlot<ResolvedToolExecutionHookContext>,
    pub on_did_execute_tool: OrderedHookSlot<ToolDidExecuteContext>,
}

impl Default for ToolExecutorHooks {
    fn default() -> Self {
        Self {
            on_execute_tool: OrderedHookSlot::new(),
            on_did_execute_tool: OrderedHookSlot::new(),
        }
    }
}

pub trait IAgentToolExecutorService: Send + Sync {
    fn execute(
        self: &Arc<Self>,
        calls: Vec<ToolCall>,
        options: ToolExecutorExecuteOptions,
    ) -> BoxStream<'static, Result<ToolExecutionResult, WireAsyncError>>;

    fn on_before_execute_tool(&self) -> Event<Arc<BeforeToolExecuteEventImpl>>;

    fn on_will_execute_tool(&self) -> Event<WillExecuteToolEvent>;

    fn hooks(&self) -> ToolExecutorHooks;

    fn record_dup_type(&self, tool_call_id: &str, dup_type: ToolCallDupType);

    fn register_tool_call_guard(&self, guard: ToolCallGuard) -> DisposableRef;

    fn register_unavailable_tool_describer(&self, describer: UnavailableToolDescriber)
    -> DisposableRef;

    fn register_missing_tool_describer(&self, describer: MissingToolDescriber) -> DisposableRef;
}
