use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::utils::abort::AbortSignal;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::r#loop::r#loop::Turn;
use crate::hooks::Hooks;
use crate::human::llm::usage::TokenUsage;
use crate::wire::record::WireAsyncError;

use super::spawn::{SpawnSubagentOptions, SpawnedSubagent, SubagentSpawnPlan, SubagentSpawnPlanInput};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AgentRunRequest {
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        helper: Option<String>,
    },
    Retry {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<String>,
    },
}

#[derive(Clone)]
pub struct RunAgentOptions {
    pub signal: AbortSignal,
    pub on_ready: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunCompletion {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SharedAgentRunError(pub Arc<String>);

impl SharedAgentRunError {
    pub fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    pub fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedAgentRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedAgentRunError {}

#[derive(Clone)]
pub struct AgentRunHandle {
    pub agent_id: String,
    pub turn: Arc<Turn>,
    pub completion: Shared<BoxFuture<'static, Result<AgentRunCompletion, SharedAgentRunError>>>,
    pub partial_output: Arc<dyn Fn() -> String + Send + Sync>,
}

#[derive(Clone)]
pub struct AgentTaskStartHookContext {
    pub agent_name: String,
    pub prompt: String,
    pub signal: AbortSignal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTaskStopHookContext {
    pub agent_name: String,
    pub response: String,
}

#[derive(Clone)]
pub struct AgentTaskHooks {
    pub on_will_start_agent_task: AgentTaskStartHookContext,
}

impl crate::hooks::HookEvents for AgentTaskHooks {
    type OnWillStartAgentTask = AgentTaskStartHookContext;
}

#[async_trait]
pub trait ISessionSubagentService: Send + Sync {
    fn hooks(&self) -> Arc<Hooks<AgentTaskHooks>>;
    fn on_did_stop_agent_task(
        &self,
        listener: Arc<dyn Fn(AgentTaskStopHookContext) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    async fn run(
        &self,
        agent: Arc<AgentContext>,
        request: AgentRunRequest,
        opts: RunAgentOptions,
    ) -> Result<AgentRunHandle, WireAsyncError>;
    async fn plan_spawn(
        &self,
        input: SubagentSpawnPlanInput,
    ) -> Result<SubagentSpawnPlan, WireAsyncError>;
    async fn spawn(
        &self,
        opts: SpawnSubagentOptions,
    ) -> Result<SpawnedSubagent, WireAsyncError>;
    fn notify_agent_task_stopped(&self, context: AgentTaskStopHookContext);
}
