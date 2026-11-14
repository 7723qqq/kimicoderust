pub const GOAL_FEATURE_NAME: &str = "goal";
pub const GOAL_DEADLINE_SERVICE_SCOPE: &str = "app";
pub const GOAL_DEADLINE_SERVICE_ACTIVATION: &str = "onDemand";
pub const GOAL_TOOL_DOMAIN: &str = "goal";
pub const GOAL_TOOL_NAMES: &[&str] = &["CreateGoal", "GetGoal", "SetGoalBudget", "UpdateGoal"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalFeature;

impl GoalFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        GOAL_FEATURE_NAME
    }

    pub fn deadline_service_scope(&self) -> &'static str {
        GOAL_DEADLINE_SERVICE_SCOPE
    }

    pub fn deadline_service_activation(&self) -> &'static str {
        GOAL_DEADLINE_SERVICE_ACTIVATION
    }
}
