use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::human::llm::message::{ContentPart, ToolCall};
use crate::wire::record::WireAsyncError;

/// Wire shape of a tool progress update. Mirrors v2's `ToolUpdate`
/// (`tool/toolContract.ts:41`) and `packages/protocol`'s `toolUpdateSchema`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdate {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace: Option<bool>,
}

impl ToolUpdate {
    pub fn text(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: Some(text.into()),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub content: Vec<ContentPart>,
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDetachAck {
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskWaitInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskWaitOutcome {
    pub completed: Vec<String>,
    pub running: Vec<String>,
    pub unknown: Vec<String>,
    pub timed_out: bool,
}

pub type OnToolUpdate = Arc<dyn Fn(ToolUpdate) + Send + Sync>;
pub type OnToolDetach = Arc<dyn Fn(ToolDetachAck) + Send + Sync>;
pub type WaitForTasks = Arc<
    dyn Fn(TaskWaitInput) -> BoxFuture<'static, Result<TaskWaitOutcome, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ToolExecuteInput {
    pub tool_call: ToolCall,
    pub signal: CancellationToken,
    pub on_update: Option<OnToolUpdate>,
    pub detach: Option<OnToolDetach>,
    pub wait_for_tasks: Option<WaitForTasks>,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, input: ToolExecuteInput) -> Result<ToolResult, WireAsyncError>;
}
