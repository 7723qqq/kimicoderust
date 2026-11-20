use std::sync::{Arc, LazyLock};

use futures_util::future::BoxFuture;

use crate::agent::tools::github::github_tools::{
    GITHUB_MUTATING_TOOL_NAMES, GITHUB_READONLY_TOOL_NAMES,
};
use crate::app::agent_profile_catalog::agent_profile_catalog::{
    AgentProfileInput, AgentProfilePromptPrefixContext,
};
use crate::app::agent_profile_catalog::contribution::register_agent_profile;
use crate::app::agent_profile_catalog::profile_shared::{
    TASK_AGENT_ROLE_PREFIX, render_system_prompt_result, skill_active_for,
};

use super::git_context::collect_git_context;

fn owned_tools(tools: &[&str]) -> Vec<String> {
    tools.iter().map(|tool| (*tool).to_string()).collect()
}

pub static AGENT_TOOLS: LazyLock<Vec<String>> = LazyLock::new(|| {
    owned_tools(&[
        "Read",
        "Write",
        "Edit",
        "Grep",
        "Glob",
        "Bash",
        "TaskList",
        "TaskOutput",
        "TaskStop",
        "WaitFor",
        "CronCreate",
        "CronList",
        "CronDelete",
        "TodoList",
        "Skill",
        "SnapshotList",
        "WebSearch",
        "Agent",
        "AgentSwarm",
        "FetchURL",
        "AskUserQuestion",
        "NotifyUser",
        "ExitPlanMode",
        "EnterSpecMode",
        "ExitSpecMode",
        "CreateGoal",
        "GetGoal",
        "SetGoalBudget",
        "UpdateGoal",
        "TowerInit",
        "TowerStatus",
        "TowerTeardown",
        "RepoMap",
        "run_code",
        "lsp",
    ])
    .into_iter()
    .chain(GITHUB_READONLY_TOOL_NAMES.iter().map(ToString::to_string))
    .chain(GITHUB_MUTATING_TOOL_NAMES.iter().map(ToString::to_string))
    .chain(std::iter::once("mcp__*".to_string()))
    .collect()
});

pub static CODER_TOOLS: LazyLock<Vec<String>> = LazyLock::new(|| {
    owned_tools(&[
        "NotifyUser",
        "Bash",
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
        "WaitFor",
        "WebSearch",
        "FetchURL",
        "Write",
        "mcp__*",
    ])
});

pub static EXPLORE_TOOLS: LazyLock<Vec<String>> = LazyLock::new(|| {
    owned_tools(&[
        "NotifyUser",
        "Bash",
        "Read",
        "Glob",
        "Grep",
        "WebSearch",
        "FetchURL",
    ])
});

pub const CODER_ROLE: &str = "You are a subagent; the main agent is your caller and all `user` messages come from it. The main agent sees only your final message. Do not ask the end user questions; explain ambiguities in your final handoff.\n\nYour final message is the entire handoff. Make it technically complete: what you changed and why, the path of every file you touched, how you verified the change (tests or commands run, with results), and anything left undone or worth follow-up. If you are stopped before finishing, the parent receives only what you have written so far, so keep the handoff current.";

pub const EXPLORE_ROLE: &str = "You are a codebase exploration specialist. Your role is EXCLUSIVELY to search, read, and analyze existing code and resources. You do NOT have access to file editing tools.\n\nGuidelines:\n- Use Glob for broad file pattern matching. Prefer patterns with a literal anchor (extension or subdirectory); pure wildcards like `*` or `**/*` are allowed but usually truncate at the match cap.\n- Use Bash only for read-only operations (ls, git status, git log, git diff, find); never use it for file creation or modification.\n- When WebSearch or FetchURL is available, use it when a question needs external context (library documentation, error messages, upstream APIs); the local codebase remains your primary domain.\n- Adapt your search depth based on the thoroughness level specified by the caller.\n\nIf the prompt includes a <git-context> block, use it to orient yourself about the repository state before starting your investigation.\n\nYou are meant to be a fast agent. Complete the search request efficiently and report your findings clearly in a structured format.";

pub fn register_profiles() {
    register_agent_profile(AgentProfileInput {
        name: "agent".to_string(),
        description: Some("Default agent".to_string()),
        when_to_use: None,
        override_: None,
        tools: Some(AGENT_TOOLS.clone()),
        disallowed_tools: None,
        subagents: Some(owned_tools(&["coder", "explore", "plan"])),
        summary_policy: None,
        system_prompt: None,
        render_system_prompt: Some(Arc::new(|context| {
            render_system_prompt_result("", context, skill_active_for(&AGENT_TOOLS), None)
        })),
        prompt_prefix: None,
    });
    register_agent_profile(AgentProfileInput {
        name: "coder".to_string(),
        description: Some("General software engineering agent — the only subagent type with file-editing tools; use it for any delegated task that must modify code.".to_string()),
        when_to_use: Some("Use this agent for non-trivial software engineering work that may require reading files, editing code, running commands, and returning a compact but technically complete summary to the parent agent.".to_string()),
        override_: None,
        tools: Some(CODER_TOOLS.clone()),
        disallowed_tools: None,
        subagents: None,
        summary_policy: None,
        system_prompt: None,
        render_system_prompt: Some(Arc::new(|context| {
            render_system_prompt_result(CODER_ROLE, context, skill_active_for(&CODER_TOOLS), None)
        })),
        prompt_prefix: None,
    });
    register_agent_profile(AgentProfileInput {
        name: "explore".to_string(),
        description: Some("Fast codebase exploration with prompt-enforced read-only behavior.".to_string()),
        when_to_use: Some("Fast agent specialized for exploring codebases: it finds files by patterns (e.g. \"src/**/*.yaml\"), searches code for keywords (e.g. \"database connection\"), and answers questions about the codebase (e.g. \"how does the auth module work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"thorough\" for comprehensive analysis across multiple locations and naming conventions.".to_string()),
        override_: None,
        tools: Some(EXPLORE_TOOLS.clone()),
        disallowed_tools: None,
        subagents: None,
        summary_policy: None,
        system_prompt: None,
        render_system_prompt: Some(Arc::new(|context| {
            render_system_prompt_result(
                &format!("{}\n\n{}", TASK_AGENT_ROLE_PREFIX, EXPLORE_ROLE.trim()),
                context,
                skill_active_for(&EXPLORE_TOOLS),
                Some(true),
            )
        })),
        prompt_prefix: Some(Arc::new(|context: AgentProfilePromptPrefixContext| {
            Box::pin(async move {
                collect_git_context(context.process, &context.cwd, context.log, None).await
            }) as BoxFuture<'static, String>
        })),
    });
}
