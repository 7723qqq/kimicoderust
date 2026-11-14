use std::sync::Arc;

use serde_json::{Map, Value};

use crate::human::llm::message::ToolDescription;
use crate::human::llm::protocol::base::TraitContext;
use crate::human::llm::protocol::thinking::ThinkingStrategy;
use crate::human::llm::requester::bases::google_genai::contract::GoogleContent;

#[derive(Clone, Default)]
pub struct GoogleGenAITrait {
    pub thinking: Option<ThinkingStrategy>,
    pub encode_max_completion_tokens: Option<Arc<dyn Fn(u64, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub convert_tool: Option<Arc<dyn Fn(&ToolDescription, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
    pub merge_history: Option<Arc<dyn Fn(&[GoogleContent], &TraitContext) -> Option<Vec<GoogleContent>> + Send + Sync>>,
    pub build_params: Option<Arc<dyn Fn(Map<String, Value>, &TraitContext) -> Option<Map<String, Value>> + Send + Sync>>,
}
