use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapability {
    pub image_in: bool,
    pub video_in: bool,
    pub audio_in: bool,
    pub thinking: bool,
    pub tool_use: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf_in: Option<bool>,
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
}

pub fn mark_unknown_capability(capability: &ModelCapability) -> ModelCapability {
    if is_unknown_capability(capability) {
        return capability.clone();
    }
    ModelCapability {
        unknown_capability: true,
        ..capability.clone()
    }
}
