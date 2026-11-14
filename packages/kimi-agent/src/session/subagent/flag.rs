use std::sync::LazyLock;

use crate::app::flag::flag_registry::{
    FlagDefinitionInput, FlagSurface, register_flag_definition,
};

pub const SUBAGENT_FORK_FLAG_ID: &str = "subagent_fork";
pub const SUBAGENT_FORK_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_SUBAGENT_FORK";

pub static SUBAGENT_FORK_FLAG: LazyLock<FlagDefinitionInput> = LazyLock::new(|| {
    FlagDefinitionInput {
        id: SUBAGENT_FORK_FLAG_ID.to_string(),
        title: "Fork context for subagents".to_string(),
        description: "Let the Agent and AgentSwarm tools start a subagent with a snapshot of the calling agent's conversation history via the fork parameter.".to_string(),
        env: SUBAGENT_FORK_FLAG_ENV.to_string(),
        default: false,
        surface: FlagSurface::Core,
        is_exposed: None,
    }
});

pub fn register_subagent_fork_flag() {
    register_flag_definition(SUBAGENT_FORK_FLAG.clone());
}
