use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::human::llm::message::StreamIndex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIContentPart {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<OpenAIImageUrl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_url: Option<OpenAIAudioUrl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_url: Option<OpenAIVideoUrl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<OpenAIFile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIAudioUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

pub type OpenAIVideoUrl = OpenAIAudioUrl;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIFile {
    pub file_data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIWireToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub function: OpenAIWireFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenAIWireFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAIContent {
    Text(String),
    Parts(Vec<OpenAIContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum OpenAIWireMessage {
    System {
        content: OpenAIContent,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    User {
        content: OpenAIContent,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Assistant {
        content: Option<OpenAIContent>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<OpenAIWireToolCall>>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Tool {
        tool_call_id: String,
        content: OpenAIContent,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

impl OpenAIWireMessage {
    pub fn extra_mut(&mut self) -> &mut Map<String, Value> {
        match self {
            Self::System { extra, .. }
            | Self::User { extra, .. }
            | Self::Assistant { extra, .. }
            | Self::Tool { extra, .. } => extra,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAIRawUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_hit_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_miss_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<OpenAICachedTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<OpenAICachedTokensDetails>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAICachedTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAIRawStreamToolCallDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<StreamIndex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function: Option<OpenAIRawStreamFunctionDelta>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAIRawStreamFunctionDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAIRawChunk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub choices: Vec<OpenAIRawChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<OpenAIRawUsage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAIRawChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<OpenAIRawDelta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpenAIRawDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIRawStreamToolCallDelta>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}
