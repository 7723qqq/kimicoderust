use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::app::config::config_section_contributions::register_config_section;
use crate::app::flag::flag::IFlagService;

use super::flag::SUBAGENT_BACKENDS_FLAG_ID;
use super::subagent_backend::ISubagentBackend;

pub const SUBAGENT_BACKEND_SECTION: &str = "subagentBackend";
pub const BACKENDS_EXPERIMENTAL_UNAVAILABLE: &str =
    "backend is disabled: the subagent_backends experimental flag is off.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCodeBackendConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexBackendConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpBackendConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentBackendConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeBackendConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexBackendConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpBackendConfig>,
}

pub static CLAUDE_CODE_BACKEND_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Path to the claude CLI (defaults to \"claude\" resolved on PATH)."},
            "model": {"type": "string", "description": "Model to run Claude Code with (defaults to the CLI default)."}
        }
    })
});
pub static CODEX_BACKEND_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Path to the codex CLI (defaults to \"codex\" resolved on PATH)."},
            "model": {"type": "string", "description": "Model to run Codex with (defaults to the CLI default)."}
        }
    })
});
pub static ACP_BACKEND_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "Command that starts the ACP server."},
            "args": {"type": "array", "items": {"type": "string"}, "description": "Extra arguments passed to the ACP server command."},
            "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Extra environment variables merged into the ACP server process."}
        },
        "required": ["command"]
    })
});
pub static SUBAGENT_BACKEND_CONFIG_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "claudeCode": CLAUDE_CODE_BACKEND_CONFIG_SCHEMA.clone(),
            "codex": CODEX_BACKEND_CONFIG_SCHEMA.clone(),
            "acp": ACP_BACKEND_CONFIG_SCHEMA.clone()
        }
    })
});

pub fn exposes_subagent_backends(flags: &dyn IFlagService) -> bool {
    flags.enabled(SUBAGENT_BACKENDS_FLAG_ID)
}

pub fn build_subagent_backend_descriptions(backends: &[Arc<dyn ISubagentBackend>]) -> String {
    format!(
        "Available backends (pass via backend): {}",
        backends
            .iter()
            .map(|backend| backend.name().as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub fn strip_subagent_backend_parameter(parameters: &Map<String, Value>) -> Map<String, Value> {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return parameters.clone();
    };
    if !properties.contains_key("backend") {
        return parameters.clone();
    }
    let mut next_properties = properties.clone();
    next_properties.remove("backend");
    let mut next = parameters.clone();
    next.insert("properties".to_string(), Value::Object(next_properties));
    if let Some(required) = parameters.get("required").and_then(Value::as_array)
        && required.iter().any(|entry| entry.as_str() == Some("backend"))
    {
        next.insert(
            "required".to_string(),
            Value::Array(
                required
                    .iter()
                    .filter(|entry| entry.as_str() != Some("backend"))
                    .cloned()
                    .collect(),
            ),
        );
    }
    next
}

pub fn register_subagent_backend_config() {
    register_config_section(
        SUBAGENT_BACKEND_SECTION,
        SUBAGENT_BACKEND_CONFIG_SCHEMA.clone(),
        json!({}),
    );
}
