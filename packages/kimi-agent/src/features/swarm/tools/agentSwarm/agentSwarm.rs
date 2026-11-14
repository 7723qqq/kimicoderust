use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const AGENT_SWARM_TOOL_SERVICE_ID: &str = "agentSwarmTool";
pub const PROMPT_TEMPLATE_PLACEHOLDER: &str = "{{item}}";
pub const MAX_AGENT_SWARM_SUBAGENTS: usize = 128;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSwarmToolInput {
    pub description: String,
    #[serde(default)]
    pub subagent_type: Option<String>,
    #[serde(default)]
    pub prompt_template: Option<String>,
    #[serde(default)]
    pub items: Option<Vec<String>>,
    #[serde(default)]
    pub fork: Option<bool>,
    #[serde(default)]
    pub resume_agent_ids: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub model: Option<String>,
}

pub fn agent_swarm_input_schema() -> Value {
    crate::tools::swarm_tool::agent_swarm_tool_def(None).input_schema
}
