use std::sync::Arc;

use crate::app::agent_profile_catalog::agent_profile_catalog::{
    AgentProfile, AgentProfileContext, AgentProfileInput, SystemPromptRenderResult,
    normalize_agent_profile,
};
use crate::app::agent_profile_catalog::agent_profile_contribution::AgentProfileContribution;
use crate::app::agent_profile_catalog::profile_shared::render_prompt_template_result;

use super::types::{AgentFileDefinition, AgentFileDiscoveryResult, AgentFileSource};

pub fn agent_profile_from_file(
    definition: AgentFileDefinition,
    base_prompt: Arc<dyn Fn(&AgentProfileContext) -> SystemPromptRenderResult + Send + Sync>,
) -> AgentProfile {
    let skill_active = definition
        .tools
        .as_ref()
        .is_none_or(|tools| tools.iter().any(|tool| tool == "Skill"))
        && !definition
            .disallowed_tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|tool| tool == "Skill"));
    let prompt = definition.prompt.clone();
    let input = AgentProfileInput {
        name: definition.name,
        description: Some(definition.description),
        when_to_use: definition.when_to_use,
        override_: Some(definition.override_ || definition.source == AgentFileSource::Explicit),
        tools: definition.tools,
        disallowed_tools: definition.disallowed_tools,
        subagents: definition.subagents,
        summary_policy: None,
        system_prompt: None,
        render_system_prompt: Some(Arc::new(move |context| {
            render_prompt_template_result(
                &prompt,
                context,
                skill_active,
                base_prompt.clone(),
            )
        })),
        prompt_prefix: None,
    };
    normalize_agent_profile(input)
}

pub fn profiles_from_discovery(
    result: AgentFileDiscoveryResult,
    base_prompt: Arc<dyn Fn(&AgentProfileContext) -> SystemPromptRenderResult + Send + Sync>,
) -> AgentProfileContribution {
    AgentProfileContribution {
        profiles: result
            .agents
            .into_iter()
            .map(|definition| agent_profile_from_file(definition, base_prompt.clone()))
            .collect(),
        skipped: Some(result.skipped),
        scanned_roots: Some(result.scanned_roots),
    }
}
