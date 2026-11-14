use crate::human::llm::capability::ModelCapability;

pub const OPENAI_REASONING_CAPABILITY: ModelCapability = ModelCapability {
    image_in: false,
    video_in: false,
    audio_in: false,
    thinking: true,
    tool_use: true,
    pdf_in: None,
    dynamically_loaded_tools: None,
    unknown_capability: false,
};

pub const OPENAI_VISION_TOOL_CAPABILITY: ModelCapability = ModelCapability {
    image_in: true,
    video_in: false,
    audio_in: false,
    thinking: false,
    tool_use: true,
    pdf_in: None,
    dynamically_loaded_tools: None,
    unknown_capability: false,
};

pub const OPENAI_TEXT_TOOL_CAPABILITY: ModelCapability = ModelCapability {
    image_in: false,
    video_in: false,
    audio_in: false,
    thinking: false,
    tool_use: true,
    pdf_in: None,
    dynamically_loaded_tools: None,
    unknown_capability: false,
};

pub const OPENAI_VISION_TOOL_PREFIXES: &[&str] = &["gpt-4o", "gpt-4-turbo", "gpt-4.1", "gpt-4.5"];

pub fn is_open_ai_reasoning_model(normalized_model_name: &str) -> bool {
    normalized_model_name
        .chars()
        .next()
        .is_some_and(|first| first == 'o')
        && normalized_model_name
            .chars()
            .nth(1)
            .is_some_and(|second| second.is_ascii_digit())
}

pub fn has_model_prefix(model_name: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| model_name.starts_with(prefix))
}

pub fn get_open_ai_legacy_model_capability(model_name: &str) -> Option<ModelCapability> {
    let normalized = model_name.to_lowercase();
    if is_open_ai_reasoning_model(&normalized) {
        return Some(OPENAI_REASONING_CAPABILITY);
    }
    if has_model_prefix(&normalized, OPENAI_VISION_TOOL_PREFIXES) {
        return Some(OPENAI_VISION_TOOL_CAPABILITY);
    }
    if normalized.starts_with("gpt-3.5-turbo") {
        return Some(OPENAI_TEXT_TOOL_CAPABILITY);
    }
    None
}
