use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::human::llm::message::{Message, ToolDescription};
use crate::human::llm::protocol::base::TraitContext;
use crate::human::llm::protocol::thinking::ThinkingStrategy;
use crate::human::llm::requester::bases::anthropic::contract::AnthropicWireMessage;
use crate::human::llm::requester::requester::ToolCallIdPolicy;

#[derive(Clone, Default)]
pub struct AnthropicTrait {
    pub tool_call_id_policy: Option<ToolCallIdPolicy>,
    pub thinking: Option<ThinkingStrategy>,
    pub encode_max_completion_tokens: Option<Arc<dyn Fn(u64, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub convert_tool: Option<Arc<dyn Fn(&ToolDescription, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub accepted_image_mimes: Option<Arc<dyn Fn(&TraitContext) -> Option<HashSet<String>> + Send + Sync>>,
    pub convert_message: Option<Arc<dyn Fn(&Message, &AnthropicWireMessage, &TraitContext) -> Option<Option<AnthropicWireMessage>> + Send + Sync>>,
    pub merge_history: Option<Arc<dyn Fn(&[AnthropicWireMessage], &TraitContext) -> Option<Vec<AnthropicWireMessage>> + Send + Sync>>,
    pub build_params: Option<Arc<dyn Fn(Map<String, Value>, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
}
