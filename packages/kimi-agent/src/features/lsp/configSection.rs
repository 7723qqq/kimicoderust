use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const LSP_SECTION: &str = "lsp";

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<IndexMap<String, String>>,
    pub extension_to_language: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialization_options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_grace_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initialize_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_grace_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct LspConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub servers: Option<IndexMap<String, LspServerConfig>>,
}
