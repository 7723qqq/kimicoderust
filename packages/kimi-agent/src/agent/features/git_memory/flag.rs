pub use crate::tools::memory_repo::git_memory_enabled;

pub const GIT_MEMORY_FLAG_ID: &str = "git_memory";
pub const GIT_MEMORY_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_GIT_MEMORY";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitMemoryFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const GIT_MEMORY_FLAG: GitMemoryFlag = GitMemoryFlag {
    id: GIT_MEMORY_FLAG_ID,
    title: "Git memory",
    description: "Distil local git history into structured facts and inject them into the agent context.",
    env: GIT_MEMORY_FLAG_ENV,
    default: false,
    surface: "both",
};
