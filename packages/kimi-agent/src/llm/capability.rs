use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub image_in: bool,
    pub video_in: bool,
    pub audio_in: bool,
    pub thinking: bool,
    pub tool_use: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_in: Option<bool>,
    pub max_context_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamically_loaded_tools: Option<bool>,
}

pub const UNKNOWN_CAPABILITY: ModelCapability = ModelCapability {
    image_in: false,
    video_in: false,
    audio_in: false,
    thinking: false,
    tool_use: false,
    pdf_in: Some(false),
    max_context_tokens: 0,
    max_input_tokens: None,
    dynamically_loaded_tools: Some(false),
};

pub fn is_unknown_capability(capability: &ModelCapability) -> bool {
    !capability.image_in
        && !capability.video_in
        && !capability.audio_in
        && !capability.thinking
        && !capability.tool_use
        && capability.pdf_in != Some(true)
        && capability.dynamically_loaded_tools != Some(true)
        && capability.max_context_tokens == 0
}
