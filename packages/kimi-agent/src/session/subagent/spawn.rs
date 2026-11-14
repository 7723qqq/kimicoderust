use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::config_section::SubagentModelSource;

pub const DEFAULT_PROFILE_NAME: &str = "coder";
pub const FORK_WITH_RESUME_UNAVAILABLE: &str = "Cannot set resume when forking the current context. Fork creates a new agent; resume continues an existing one.";
pub const FORK_WITH_TYPE_UNAVAILABLE: &str = "Cannot set a different subagent_type when forking the current context. A fork inherits this agent's own agent type.";
pub const FORK_WITH_MODEL_UNAVAILABLE: &str = "Cannot override the model when forking the current context. A fork inherits this agent's model.";
pub const FORK_EXPERIMENTAL_UNAVAILABLE: &str = "fork is disabled: the subagent_fork experimental flag is off.";
pub const FORK_CONTEXT_NOTICE: &str = "The conversation above is not your own history: it is a one-time snapshot inherited from the agent that forked you. Treat it as reference material only — you are an independent subagent, not a continuation of that agent. Do the task below directly yourself, then report the result.";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkCompatibilityArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpawnCallerProfile {
    pub profile_name: Option<String>,
    pub model_alias: Option<String>,
}

pub fn fork_incompatibility(
    args: &ForkCompatibilityArgs,
    own: &SpawnCallerProfile,
) -> Option<&'static str> {
    if args
        .resume
        .as_deref()
        .map(str::trim)
        .is_some_and(|resume| !resume.is_empty())
    {
        return Some(FORK_WITH_RESUME_UNAVAILABLE);
    }
    let requested_profile_name = args
        .subagent_type
        .as_deref()
        .filter(|profile_name| !profile_name.is_empty());
    if requested_profile_name.is_some_and(|profile_name| Some(profile_name) != own.profile_name.as_deref()) {
        return Some(FORK_WITH_TYPE_UNAVAILABLE);
    }
    if args.model.as_deref() != own.model_alias.as_deref()
        && args.model.is_some()
    {
        return Some(FORK_WITH_MODEL_UNAVAILABLE);
    }
    None
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSpawnPlanInput {
    pub caller_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSpawnPlan {
    pub profile_name: String,
    pub model: String,
    pub model_source: Option<SubagentModelSource>,
    pub thinking: Option<String>,
    pub fork: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSubagentOptions {
    pub caller_agent_id: String,
    pub plan: SubagentSpawnPlan,
    pub labels: Option<HashMap<String, String>>,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnedSubagent {
    pub agent_id: String,
    pub profile_name: String,
    pub model: String,
    pub model_source: Option<SubagentModelSource>,
    pub prompt_text: String,
}
