use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use super::subagent_backend::{
    BACKENDS_EXPERIMENTAL_UNAVAILABLE, build_subagent_backend_descriptions,
    strip_subagent_backend_parameter,
};

pub const SUBAGENT_BACKEND_SECTION: &str = "subagentBackend";

pub const CLAUDE_CODE_COMMAND_DESCRIPTION: &str =
    "Path to the claude CLI (defaults to \"claude\" resolved on PATH).";
pub const CLAUDE_CODE_MODEL_DESCRIPTION: &str =
    "Model to run Claude Code with (defaults to the CLI default).";
pub const CODEX_COMMAND_DESCRIPTION: &str =
    "Path to the codex CLI (defaults to \"codex\" resolved on PATH).";
pub const CODEX_MODEL_DESCRIPTION: &str = "Model to run Codex with (defaults to the CLI default).";
pub const ACP_COMMAND_DESCRIPTION: &str = "Command that starts the ACP server.";
pub const ACP_ARGS_DESCRIPTION: &str = "Extra arguments passed to the ACP server command.";
pub const ACP_ENV_DESCRIPTION: &str =
    "Extra environment variables merged into the ACP server process.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ClaudeCodeBackendConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CodexBackendConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AcpBackendConfig {
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentBackendConfig {
    #[serde(default)]
    pub claude_code: Option<ClaudeCodeBackendConfig>,
    #[serde(default)]
    pub codex: Option<CodexBackendConfig>,
    #[serde(default)]
    pub acp: Option<AcpBackendConfig>,
}

pub fn exposes_subagent_backends(flag_enabled: bool) -> bool {
    flag_enabled
}
