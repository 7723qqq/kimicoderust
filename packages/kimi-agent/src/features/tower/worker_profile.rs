use crate::prompt::profiles::{AgentProfile, CODER_ROLE_SUFFIX, TASK_AGENT_ROLE_PREFIX};

use super::tower::TOWER_WORKER_PROFILE;

pub const TOWER_WORKER_TOOLS: &[&str] = &[
    "Bash",
    "TowerComplete",
    "TowerFinding",
    "TowerInbox",
    "TowerMission",
    "TowerReview",
    "TowerSend",
    "TowerStatus",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "ExitSpecMode",
    "Glob",
    "Grep",
    "Read",
    "RepoMap",
    "run_code",
    "lsp",
    "Skill",
    "SnapshotList",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TodoList",
    "NotifyUser",
    "WaitFor",
    "WebSearch",
    "FetchURL",
    "Write",
    "mcp__*",
];
pub const TOWER_WORKER_ROLE_OVERLAY: &str = include_str!("tower-worker-overlay.md");

pub fn tower_worker_profile() -> AgentProfile {
    AgentProfile {
        name: TOWER_WORKER_PROFILE.to_string(),
        description: "Tower worker/reviewer agent — executes one tower mission in its own git worktree (or reviews one branch), coordinating only through Tower* tools. Spawned via the TowerSpawn tool.".to_string(),
        when_to_use: Some("Use this agent for non-trivial software engineering work that may require reading files, editing code, running commands, and returning a compact but technically complete summary to the parent agent.".to_string()),
        role_additional: format!(
            "{TASK_AGENT_ROLE_PREFIX}\n\n{CODER_ROLE_SUFFIX}\n\n{}",
            TOWER_WORKER_ROLE_OVERLAY.trim()
        ),
        tools: TOWER_WORKER_TOOLS.iter().map(|name| (*name).to_string()).collect(),
        subagents: None,
    }
}
