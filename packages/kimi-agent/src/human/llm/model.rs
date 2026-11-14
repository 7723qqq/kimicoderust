use serde::{Deserialize, Serialize};

use crate::human::llm::capability::ModelCapability;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConnection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vertexai: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmModel {
    #[serde(flatten)]
    pub connection: LlmConnection,
    pub provider: String,
    pub model: String,
    pub capability: ModelCapability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
}

impl LlmModel {
    pub fn base_url(&self) -> Option<&str> {
        self.connection.base_url.as_deref()
    }
}

pub fn model_key(model: &LlmModel) -> String {
    match &model.connection.base_url {
        Some(base_url) => format!("{base_url}#{}", model.model),
        None => model.model.clone(),
    }
}
