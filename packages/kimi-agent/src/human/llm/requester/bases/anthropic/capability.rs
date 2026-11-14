use crate::human::llm::capability::ModelCapability;

const CLAUDE_VISION_TOOL_PREFIXES: &[&str] = &["claude-3-", "claude-3.5-", "claude-3.7-"];
const CLAUDE_THINKING_VISION_TOOL_PREFIXES: &[&str] = &[
    "claude-opus-4",
    "claude-sonnet-4",
    "claude-haiku-4",
    "claude-fable",
];

pub const ANTHROPIC_VISION_TOOL_CAPABILITY: ModelCapability = ModelCapability {
    image_in: true,
    video_in: false,
    audio_in: false,
    thinking: false,
    tool_use: true,
    pdf_in: None,
    dynamically_loaded_tools: None,
    unknown_capability: false,
};

pub const ANTHROPIC_THINKING_VISION_TOOL_CAPABILITY: ModelCapability = ModelCapability {
    image_in: true,
    video_in: false,
    audio_in: false,
    thinking: true,
    tool_use: true,
    pdf_in: None,
    dynamically_loaded_tools: None,
    unknown_capability: false,
};

pub fn get_anthropic_model_capability(model_name: &str) -> Option<ModelCapability> {
    let normalized = model_name.to_lowercase();
    if CLAUDE_VISION_TOOL_PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    {
        return Some(ANTHROPIC_VISION_TOOL_CAPABILITY);
    }
    if CLAUDE_THINKING_VISION_TOOL_PREFIXES
        .iter()
        .any(|prefix| normalized.starts_with(prefix))
    {
        return Some(ANTHROPIC_THINKING_VISION_TOOL_CAPABILITY);
    }
    None
}
