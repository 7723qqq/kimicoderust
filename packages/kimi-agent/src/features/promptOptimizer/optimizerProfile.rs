use crate::prompt::profiles::AgentProfile;

use super::promptOptimizer::{
    PROMPT_OPTIMIZER_PROFILE_NAME, PROMPT_OPTIMIZER_SYSTEM_PROMPT, PROMPT_OPTIMIZER_TOOLS,
};

pub const PROMPT_OPTIMIZER_PROFILE_DESCRIPTION: &str =
    "Rewrites a drafted prompt without acting on it.";

pub fn prompt_optimizer_profile() -> AgentProfile {
    AgentProfile {
        name: PROMPT_OPTIMIZER_PROFILE_NAME.to_string(),
        description: PROMPT_OPTIMIZER_PROFILE_DESCRIPTION.to_string(),
        when_to_use: None,
        role_additional: PROMPT_OPTIMIZER_SYSTEM_PROMPT.to_string(),
        tools: PROMPT_OPTIMIZER_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        subagents: None,
    }
}
