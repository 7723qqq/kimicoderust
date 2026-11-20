use std::sync::Arc;

use crate::_base::di::collection::{CollectionDescriptor, CollectionOptions, collection};
use crate::agent::agent_context::agent_context::AgentContext;

use super::agent_model::DomainResourceRuntime;

pub struct AgentEffectContext {
    pub agent: Arc<AgentContext>,
}

#[derive(Clone)]
pub struct AgentEffectDefinition {
    pub id: String,
    pub create: Arc<dyn Fn(AgentEffectContext) -> Arc<dyn DomainResourceRuntime> + Send + Sync>,
}

pub fn define_agent_effect(definition: AgentEffectDefinition) -> AgentEffectDefinition {
    definition
}

pub fn agent_effect_contribution() -> CollectionDescriptor<AgentEffectDefinition> {
    collection::<AgentEffectDefinition>(
        "agent-effect",
        CollectionOptions::validating(|value: &AgentEffectDefinition, existing: &[AgentEffectDefinition]| {
            if existing.iter().any(|definition| definition.id == value.id) {
                return Err(format!(
                    "Agent effect '{}' already has an active provider",
                    value.id
                ));
            }
            Ok(())
        }),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEffectContext {
    pub session_id: String,
}

#[derive(Clone)]
pub struct SessionEffectDefinition {
    pub id: String,
    pub create: Arc<dyn Fn(SessionEffectContext) -> Arc<dyn DomainResourceRuntime> + Send + Sync>,
}

pub fn session_effect_contribution() -> CollectionDescriptor<SessionEffectDefinition> {
    collection::<SessionEffectDefinition>(
        "session-effect",
        CollectionOptions::validating(|value: &SessionEffectDefinition, existing: &[SessionEffectDefinition]| {
            if existing.iter().any(|definition| definition.id == value.id) {
                return Err(format!(
                    "Session effect '{}' already has an active provider",
                    value.id
                ));
            }
            Ok(())
        }),
    )
}
