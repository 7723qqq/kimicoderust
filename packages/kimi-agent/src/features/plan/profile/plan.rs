use std::sync::Arc;

use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfileInput;
use crate::app::agent_profile_catalog::contribution::register_agent_profile;
use crate::app::agent_profile_catalog::profile_shared::{
    render_system_prompt_result, skill_active_for,
};
pub use crate::prompt::profiles::{PLAN_ROLE_SUFFIX, TASK_AGENT_ROLE_PREFIX};

pub const PLAN_TOOLS: &[&str] = &["NotifyUser", "Read", "Glob", "Grep", "WebSearch", "FetchURL"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanProfile {
    pub name: &'static str,
    pub description: &'static str,
    pub when_to_use: &'static str,
    pub tools: &'static [&'static str],
    pub read_only: bool,
}

pub const PLAN_PROFILE: PlanProfile = PlanProfile {
    name: "plan",
    description: "Read-only implementation planning and architecture design.",
    when_to_use: "Use this agent when the parent agent needs a step-by-step implementation plan, key file identification, and architectural trade-off analysis before code changes are made.",
    tools: PLAN_TOOLS,
    read_only: true,
};

pub fn plan_role() -> String {
    format!(
        "{TASK_AGENT_ROLE_PREFIX}\n\nIf you lack sufficient codebase context, recommend an explore subagent for the remaining questions. Return:\n1. What you know\n2. Unanswered questions for explore\n3. A preliminary plan if questions remain, otherwise a final implementation plan\n\nYou are read-only: do not run commands or modify files, even when general instructions say to make changes. Return the plan as your final message."
    )
}

pub fn register_plan_profile() {
    let tools = owned_plan_tools();
    let render_tools = tools.clone();
    register_agent_profile(AgentProfileInput {
        name: PLAN_PROFILE.name.to_string(),
        description: Some(PLAN_PROFILE.description.to_string()),
        when_to_use: Some(PLAN_PROFILE.when_to_use.to_string()),
        override_: None,
        tools: Some(tools),
        disallowed_tools: None,
        subagents: None,
        summary_policy: None,
        system_prompt: None,
        render_system_prompt: Some(Arc::new(move |context| {
            render_system_prompt_result(
                &plan_role(),
                context,
                skill_active_for(&render_tools),
                Some(true),
            )
        })),
        prompt_prefix: None,
    });
}

fn owned_plan_tools() -> Vec<String> {
    PLAN_TOOLS.iter().map(|tool| (*tool).to_string()).collect()
}
