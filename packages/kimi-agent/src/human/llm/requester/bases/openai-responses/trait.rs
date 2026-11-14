use std::sync::Arc;

use serde_json::{Map, Value};

use crate::human::llm::message::ToolDescription;
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::base::TraitContext;
use crate::human::llm::protocol::thinking::ThinkingStrategy;
use crate::human::llm::requester::bases::openai_responses::contract::{
    OpenAIResponsesRawChunk, OpenAIResponsesRawUsage, ResponsesInputItem,
};
use crate::human::llm::requester::requester::{ToolCallIdPolicy, ToolMessageConversion};

#[derive(Debug, Clone)]
pub struct ResponsesCompletionTokensWhenUnsetInput {
    pub model: LlmModel,
    pub used_context_tokens: Option<u64>,
}

#[derive(Clone, Default)]
pub struct OpenAIResponsesTrait {
    pub tool_call_id_policy: Option<ToolCallIdPolicy>,
    pub tool_message_conversion: Option<ToolMessageConversion>,
    pub strict_thinking_validation: Option<bool>,
    pub thinking: Option<ThinkingStrategy>,
    pub encode_cache_key: Option<Arc<dyn Fn(&str, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub encode_max_completion_tokens: Option<Arc<dyn Fn(u64, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub completion_tokens_when_unset: Option<Arc<dyn Fn(&ResponsesCompletionTokensWhenUnsetInput) -> Option<u64> + Send + Sync>>,
    pub convert_tool: Option<Arc<dyn Fn(&ToolDescription, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub merge_history: Option<Arc<dyn Fn(&[ResponsesInputItem], &TraitContext) -> Option<Vec<ResponsesInputItem>> + Send + Sync>>,
    pub build_params: Option<Arc<dyn Fn(Map<String, Value>, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub extract_usage: Option<Arc<dyn Fn(&OpenAIResponsesRawChunk) -> Option<OpenAIResponsesRawUsage> + Send + Sync>>,
}
