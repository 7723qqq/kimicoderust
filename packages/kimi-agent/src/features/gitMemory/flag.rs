use std::sync::LazyLock;

use crate::app::flag::flag_registry::{
    FlagDefinitionInput, FlagSurface, register_flag_definition,
};

pub const GIT_MEMORY_FLAG_ID: &str = "git_memory";
pub const GIT_MEMORY_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_GIT_MEMORY";

pub static GIT_MEMORY_FLAG: LazyLock<FlagDefinitionInput> = LazyLock::new(|| FlagDefinitionInput {
    id: GIT_MEMORY_FLAG_ID.to_string(),
    title: "Git memory".to_string(),
    description:
        "Distil local git history into structured facts and inject them into the agent context."
            .to_string(),
    env: GIT_MEMORY_FLAG_ENV.to_string(),
    default: false,
    surface: FlagSurface::Both,
    is_exposed: None,
});

static REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    register_flag_definition(GIT_MEMORY_FLAG.clone());
});

pub fn register_git_memory_flag() {
    LazyLock::force(&REGISTRATION);
}
