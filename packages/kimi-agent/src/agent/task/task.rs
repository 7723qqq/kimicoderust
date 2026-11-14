use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::_base::event::Event;
use crate::_base::utils::abort::AbortSignal;
use crate::app::task::task::ITaskHandle;
use crate::wire::record::WireAsyncError;

pub use super::persist::{AgentTaskOutputTail, AgentTaskPersistence};
pub use super::task_lifecycle::ForegroundTaskReleaseReason;
pub use super::types::{
    AgentTask, AgentTaskInfo, AgentTaskInfoBase, AgentTaskKind, AgentTaskStatus,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentTaskLoadOptions {
    pub replace: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskOutputSnapshot {
    pub output_path: Option<String>,
    pub output_size_bytes: u64,
    pub preview_bytes: u64,
    pub truncated: bool,
    pub full_output_available: bool,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskOutputChunk {
    pub task_id: String,
    pub offset: u64,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct RegisterAgentTaskOptions {
    pub detached: Option<bool>,
    pub detachable: Option<bool>,
    pub timeout_ms: Option<u64>,
    pub detach_timeout_ms: Option<u64>,
    pub auto_background_on_timeout: Option<bool>,
    pub signal: Option<CancellationToken>,
}

#[derive(Clone)]
pub struct AgentTaskTrackOptions {
    pub id_prefix: Option<String>,
    pub description: String,
    pub detached: Option<bool>,
    pub timeout_ms: Option<u64>,
    pub detach_timeout_ms: Option<u64>,
    pub signal: Option<CancellationToken>,
    pub force_stop: Option<Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>>,
    pub on_detach: Option<Arc<dyn Fn() + Send + Sync>>,
    pub to_info: Arc<dyn Fn(AgentTaskInfoBase) -> AgentTaskInfo + Send + Sync>,
}

#[derive(Clone)]
pub struct IAgentTaskEntry {
    pub task_id: String,
    pub on_did_detach: Shared<BoxFuture<'static, ForegroundTaskReleaseReason>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentTaskNotificationSeverity {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskNotificationContext {
    pub agent_id: String,
    pub notification_type: String,
    pub title: String,
    pub body: String,
    pub severity: AgentTaskNotificationSeverity,
    pub source_kind: String,
    pub source_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskWaitDelivery {
    pub task_id: String,
    pub status: AgentTaskStatus,
}

#[async_trait]
pub trait IAgentTaskService: Send + Sync {
    fn on_did_append_output(&self) -> Event<AgentTaskOutputChunk>;
    fn on_did_change_task(&self) -> Event<AgentTaskInfo>;
    fn refresh(&self, task_id: &str);
    fn track(
        &self,
        handle: Arc<dyn ITaskHandle<serde_json::Value>>,
        options: AgentTaskTrackOptions,
    ) -> IAgentTaskEntry;
    fn register_task(&self, task: Arc<dyn AgentTask>, options: Option<RegisterAgentTaskOptions>) -> String;
    fn get_task(&self, task_id: &str) -> Option<AgentTaskInfo>;
    fn list(&self, active_only: Option<bool>, limit: Option<usize>) -> Vec<AgentTaskInfo>;
    fn persist_output(&self, task_id: &str);
    async fn get_output_snapshot(
        &self,
        task_id: &str,
        max_preview_bytes: i64,
    ) -> Result<AgentTaskOutputSnapshot, WireAsyncError>;
    async fn read_output(&self, task_id: &str, tail: Option<i64>) -> Result<String, WireAsyncError>;
    async fn read_output_tail(
        &self,
        task_id: &str,
        max_bytes: i64,
    ) -> Result<AgentTaskOutputTail, WireAsyncError>;
    async fn suppress_terminal_notification(&self, task_id: &str) -> Result<(), WireAsyncError>;
    async fn suppress_all_terminal_notifications(&self) -> Result<(), WireAsyncError>;
    fn mark_tasks_delivered_via_wait(&self, tasks: &[AgentTaskWaitDelivery]);
    fn detach(&self, task_id: &str) -> Option<AgentTaskInfo>;
    async fn stop(&self, task_id: &str, reason: Option<String>) -> Result<Option<AgentTaskInfo>, WireAsyncError>;
    async fn stop_by_user(&self, task_id: &str) -> Result<Option<AgentTaskInfo>, WireAsyncError>;
    async fn stop_all(&self, reason: Option<String>) -> Result<Vec<AgentTaskInfo>, WireAsyncError>;
    async fn stop_all_on_exit(&self, reason: String) -> Result<Vec<AgentTaskInfo>, WireAsyncError>;
    async fn drain_writes(&self) -> Result<(), WireAsyncError>;
    async fn wait(
        &self,
        task_id: &str,
        timeout_ms: Option<u64>,
        signal: Option<AbortSignal>,
    ) -> Result<Option<AgentTaskInfo>, WireAsyncError>;
    async fn wait_for_foreground_release(
        &self,
        task_id: &str,
    ) -> Result<Option<ForegroundTaskReleaseReason>, WireAsyncError>;
}
