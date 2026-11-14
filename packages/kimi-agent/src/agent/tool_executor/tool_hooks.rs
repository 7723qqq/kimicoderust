use std::sync::Arc;

use serde_json::Value;

use crate::_base::event::{IWaitUntil, WaitUntil};
use crate::_base::utils::abort::AbortSignal;
use crate::human::llm::message::ToolCall;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use super::tool_executor::{
    ExecutableTool, ExecutableToolResult, RunnableToolExecution, ToolAccesses,
};

#[derive(Clone)]
pub struct ToolExecutionHookContext {
    pub turn_id: u64,
    pub signal: AbortSignal,
    pub trace: Option<LLMRequestTrace>,
    pub tool_call: ToolCall,
    pub tool_calls: Vec<ToolCall>,
    pub tool: Option<Arc<dyn ExecutableTool>>,
    pub args: Value,
}

#[derive(Clone)]
pub struct ResolvedToolExecutionHookContext {
    pub base: ToolExecutionHookContext,
    pub execution: RunnableToolExecution,
}

#[derive(Clone, Default)]
pub struct BeforeExecuteDecision {
    pub veto: Option<ExecutableToolResult>,
    pub execution_metadata: Option<Value>,
}

pub type BeforeToolExecuteEvent = Arc<super::before_tool_execute_event::BeforeToolExecuteEventImpl>;

#[derive(Clone)]
pub struct WillExecuteToolEventData {
    pub turn_id: u64,
    pub tool_call: ToolCall,
    pub execution: RunnableToolExecution,
    pub args: Value,
}

#[derive(Clone)]
pub struct WillExecuteToolEvent {
    pub turn_id: u64,
    pub tool_call: ToolCall,
    pub execution: RunnableToolExecution,
    pub args: Value,
    pub wait_until: WaitUntil,
}

impl IWaitUntil for WillExecuteToolEvent {
    type Data = WillExecuteToolEventData;

    fn with_wait_until(data: Self::Data, _signal: AbortSignal, wait_until: WaitUntil) -> Self {
        Self {
            turn_id: data.turn_id,
            tool_call: data.tool_call,
            execution: data.execution,
            args: data.args,
            wait_until,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionOutcome {
    Executed,
    PreflightRejected,
    ResolutionFailed,
    Vetoed,
    Aborted,
    HookFailed,
    Synthetic,
    Skipped,
}

impl ToolExecutionOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Executed => "executed",
            Self::PreflightRejected => "preflight-rejected",
            Self::ResolutionFailed => "resolution-failed",
            Self::Vetoed => "vetoed",
            Self::Aborted => "aborted",
            Self::HookFailed => "hook-failed",
            Self::Synthetic => "synthetic",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone)]
pub struct ToolDidExecuteContext {
    pub base: ToolExecutionHookContext,
    pub outcome: ToolExecutionOutcome,
    pub accesses: Option<ToolAccesses>,
    pub result: ExecutableToolResult,
    pub stop_turn: Option<bool>,
}
