use serde::{Deserialize, Serialize};

pub use crate::app::agent_profile_catalog::agent_profile_contribution::SkippedAgentFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentFileSource {
    Plugin,
    Project,
    User,
    Extra,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFileRoot {
    pub path: String,
    pub source: AgentFileSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentFileDefinition {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    pub override_: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<Vec<String>>,
    pub prompt: String,
    pub path: String,
    pub source: AgentFileSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentFileDiscoveryResult {
    pub agents: Vec<AgentFileDefinition>,
    pub skipped: Vec<SkippedAgentFile>,
    pub scanned_roots: Vec<String>,
}
