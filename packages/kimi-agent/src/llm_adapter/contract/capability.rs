use serde::{Deserialize, Serialize};

use crate::human::llm::capability::ModelCapability as LlmModelCapability;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(skip)]
    pub unknown_capability: bool,
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
    unknown_capability: true,
};

pub fn is_unknown_capability(capability: &ModelCapability) -> bool {
    if capability.unknown_capability {
        return true;
    }
    !capability.image_in
        && !capability.video_in
        && !capability.audio_in
        && !capability.thinking
        && !capability.tool_use
        && capability.pdf_in != Some(true)
        && capability.dynamically_loaded_tools != Some(true)
        && capability.max_context_tokens == 0
}

pub fn mark_unknown_capability(capability: &ModelCapability) -> ModelCapability {
    ModelCapability {
        unknown_capability: true,
        ..capability.clone()
    }
}

pub fn to_llm_capability(capability: &ModelCapability) -> LlmModelCapability {
    LlmModelCapability {
        image_in: capability.image_in,
        video_in: capability.video_in,
        audio_in: capability.audio_in,
        thinking: capability.thinking,
        tool_use: capability.tool_use,
        pdf_in: capability.pdf_in,
        dynamically_loaded_tools: capability.dynamically_loaded_tools,
        unknown_capability: false,
    }
}
