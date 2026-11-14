use super::subagent_backend::{SUBAGENT_BACKENDS_FLAG_ENV, SUBAGENT_BACKENDS_FLAG_ID};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubagentBackendsFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const SUBAGENT_BACKENDS_FLAG: SubagentBackendsFlag = SubagentBackendsFlag {
    id: SUBAGENT_BACKENDS_FLAG_ID,
    title: "External subagent backends",
    description: "Enables the claude-code / codex / acp backends for the Agent tool, which spawn external agent CLIs instead of in-process subagents.",
    env: SUBAGENT_BACKENDS_FLAG_ENV,
    default: false,
    surface: "core",
};
