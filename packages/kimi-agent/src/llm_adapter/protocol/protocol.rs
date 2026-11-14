use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::llm_adapter::contract::capability::ModelCapability;
use crate::llm_adapter::model::catalog::Model;
use crate::llm_adapter::model::model_requester_impl::ResolvedLlmModel;
use crate::llm_adapter::protocol::protocol_base::{ProtocolBaseId, ResolvedAdapterIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai")]
    OpenAI,
    #[serde(rename = "openai_responses")]
    OpenAIResponses,
    #[serde(rename = "google-genai")]
    GoogleGenAI,
    #[serde(rename = "antigravity")]
    Antigravity,
}

impl Protocol {
    pub const ALL: [Self; 5] = [
        Self::Anthropic,
        Self::OpenAI,
        Self::OpenAIResponses,
        Self::GoogleGenAI,
        Self::Antigravity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAI => "openai",
            Self::OpenAIResponses => "openai_responses",
            Self::GoogleGenAI => "google-genai",
            Self::Antigravity => "antigravity",
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Protocol {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAI),
            "openai_responses" => Ok(Self::OpenAIResponses),
            "google-genai" => Ok(Self::GoogleGenAI),
            "antigravity" => Ok(Self::Antigravity),
            _ => Err(format!("unsupported protocol: {value}")),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolProviderOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertexai: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl ProtocolProviderOptions {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolAdapterConfig {
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    pub model_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<ProtocolProviderOptions>,
}

pub trait IProtocolAdapterRegistry: Send + Sync {
    fn supported_protocols(&self) -> Vec<Protocol>;
    fn resolve_adapter_identity(
        &self,
        protocol: Protocol,
        provider_type: Option<&str>,
    ) -> ResolvedAdapterIdentity;
    fn resolve_provider_base_id(
        &self,
        protocol: Protocol,
        provider_type: Option<&str>,
    ) -> ProtocolBaseId;
    fn resolve_capability(
        &self,
        protocol: Protocol,
        model_name: &str,
        provider_type: Option<&str>,
        provider_id: Option<&str>,
    ) -> ModelCapability;
    fn resolve(&self, model: &Model) -> Result<ResolvedLlmModel, crate::wire::record::WireAsyncError>;
}
