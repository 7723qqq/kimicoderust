pub const SKILL_FEATURE_NAME: &str = "skill";
pub const SKILL_SERVICE_SCOPE: &str = "agent";
pub const SKILL_SERVICE_ACTIVATION: &str = "onDemand";
pub const SKILL_TOOL_NAME: &str = "Skill";
pub const SKILL_TOOL_DOMAIN: &str = "skill";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SkillFeature;

impl SkillFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        SKILL_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        SKILL_SERVICE_SCOPE
    }

    pub fn service_activation(&self) -> &'static str {
        SKILL_SERVICE_ACTIVATION
    }
}
