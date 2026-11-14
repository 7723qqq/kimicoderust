use crate::human::llm::capability::ModelCapability;
use crate::human::llm::requester::bases::openai::capability::{
    OPENAI_REASONING_CAPABILITY, OPENAI_VISION_TOOL_CAPABILITY, OPENAI_VISION_TOOL_PREFIXES,
    has_model_prefix, is_open_ai_reasoning_model,
};

pub fn get_open_ai_responses_model_capability(model_name: &str) -> Option<ModelCapability> {
    let normalized = model_name.to_lowercase();
    if is_open_ai_reasoning_model(&normalized) {
        return Some(OPENAI_REASONING_CAPABILITY);
    }
    if has_model_prefix(&normalized, OPENAI_VISION_TOOL_PREFIXES) {
        return Some(OPENAI_VISION_TOOL_CAPABILITY);
    }
    None
}
