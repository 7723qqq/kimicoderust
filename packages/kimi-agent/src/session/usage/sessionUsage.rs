use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::IDisposable;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::llm_requester::llm_requester::AgentLLMRequestSource;
use crate::agent::usage::usage::{UsageRecordedContext, UsageStatus};
use crate::human::llm::usage::TokenUsage;
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait ISessionUsageService: Send + Sync {
    async fn record(
        &self,
        agent: Arc<AgentContext>,
        model: String,
        usage: TokenUsage,
        source: Option<AgentLLMRequestSource>,
    ) -> Result<(), WireAsyncError>;
    fn status(&self, agent: &AgentContext) -> UsageStatus;
    fn on_did_record(
        &self,
        listener: Arc<dyn Fn(UsageRecordedContext) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
}
