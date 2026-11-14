use std::sync::LazyLock;

use crate::app::flag::flag_registry::{
    FlagDefinitionInput, FlagSurface, register_flag_definition,
};

pub const SUBAGENT_BACKENDS_FLAG_ID: &str = "subagent_backends";
pub const SUBAGENT_BACKENDS_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_SUBAGENT_BACKENDS";

pub static SUBAGENT_BACKENDS_FLAG: LazyLock<FlagDefinitionInput> = LazyLock::new(|| {
    FlagDefinitionInput {
        id: SUBAGENT_BACKENDS_FLAG_ID.to_string(),
        title: "External subagent backends".to_string(),
        description: "Enables the claude-code / codex / acp backends for the Agent tool, which spawn external agent CLIs instead of in-process subagents.".to_string(),
        env: SUBAGENT_BACKENDS_FLAG_ENV.to_string(),
        default: false,
        surface: FlagSurface::Core,
        is_exposed: None,
    }
});

pub fn register_subagent_backends_flag() {
    register_flag_definition(SUBAGENT_BACKENDS_FLAG.clone());
}
