use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::token_counting::token_counting::{
    ContextSize, TokenCountingRequest, TokenCountingStrategy,
};
use crate::human::llm::message::ToolDescription as Tool;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::contract::model_family::ImageTokenPricing;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCountingRebaseInput {
    pub length: usize,
    pub tokens: usize,
    pub measured: bool,
}

pub trait ISessionTokenCountingService: Send + Sync {
    fn strategy(&self) -> TokenCountingStrategy;
    fn get(&self, agent: &AgentContext, start: Option<isize>, end: Option<isize>) -> ContextSize;
    fn measured(
        &self,
        agent: &AgentContext,
        input: &[Message],
        output: &[Message],
        usage: &TokenUsage,
    );
    fn latest_measured(&self, agent: &AgentContext) -> usize;
    fn status_size(&self, agent: &AgentContext) -> u64;
    fn record_truncation(&self, agent: &AgentContext, cut_index: usize);
    fn rebase(&self, agent: &AgentContext, input: TokenCountingRebaseInput);
    fn request_size(
        &self,
        request: &TokenCountingRequest,
        pricing: Option<&ImageTokenPricing>,
    ) -> usize;
    fn estimate_text(&self, text: &str) -> usize;
    fn estimate_message(&self, message: &Message, pricing: Option<&ImageTokenPricing>) -> usize;
    fn estimate_messages(&self, messages: &[Message], pricing: Option<&ImageTokenPricing>) -> usize;
    fn estimate_tools(&self, tools: &[Tool]) -> usize;
}
