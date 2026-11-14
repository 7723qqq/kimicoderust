use super::super::feature::Feature;
use super::super::featureRegistry::register_feature;
use super::plan::AGENT_PLAN_SERVICE_ID;
use super::tools::exitPlanMode::exitPlanMode::EXIT_PLAN_MODE_TOOL_SERVICE_ID;

pub const PLAN_FEATURE_NAME: &str = "plan";
pub const PLAN_SERVICE_SCOPE: &str = "agent";
pub const PLAN_TOOL_DOMAIN: &str = "plan";
pub const EXIT_PLAN_MODE_TOOL_NAME: &str = "ExitPlanMode";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanFeature;

impl PlanFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        PLAN_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        PLAN_SERVICE_SCOPE
    }

    pub fn assemble(&self) -> Feature {
        let mut feature = Feature::new(PLAN_FEATURE_NAME);
        feature.contribute_agent_service(AGENT_PLAN_SERVICE_ID, None);
        feature.contribute_tool(
            EXIT_PLAN_MODE_TOOL_SERVICE_ID,
            EXIT_PLAN_MODE_TOOL_NAME,
            PLAN_TOOL_DOMAIN,
        );
        feature
    }
}

pub fn register_plan_feature() {
    register_feature(|| PlanFeature::new().assemble());
}
