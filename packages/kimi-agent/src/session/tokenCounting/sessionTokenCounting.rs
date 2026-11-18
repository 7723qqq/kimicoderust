use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
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
    pub tokens: u64,
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
    fn latest_measured(&self, agent: &AgentContext) -> u64;
    fn status_size(&self, agent: &AgentContext) -> u64;
    fn record_truncation(&self, agent: &AgentContext, cut_index: usize);
    fn rebase(&self, agent: &AgentContext, input: TokenCountingRebaseInput);
    fn request_size(
        &self,
        request: &TokenCountingRequest,
        pricing: Option<&ImageTokenPricing>,
    ) -> u64;
    fn estimate_text(&self, text: &str) -> u64;
    fn estimate_message(&self, message: &Message, pricing: Option<&ImageTokenPricing>) -> u64;
    fn estimate_messages(&self, messages: &[Message], pricing: Option<&ImageTokenPricing>) -> u64;
    fn estimate_tools(&self, tools: &[Tool]) -> u64;
}


pub static I_SESSION_TOKEN_COUNTING_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn ISessionTokenCountingService>>> =
    LazyLock::new(|| create_decorator("sessionTokenCountingService"));
