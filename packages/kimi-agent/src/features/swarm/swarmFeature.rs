pub const SWARM_FEATURE_NAME: &str = "swarm";
pub const SWARM_SERVICE_ACTIVATION: &str = "onScopeCreated";
pub const SWARM_TOOL_NAME: &str = "AgentSwarm";
pub const SWARM_TOOL_DOMAIN: &str = "swarm";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SwarmFeature;

impl SwarmFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        SWARM_FEATURE_NAME
    }

    pub fn service_activation(&self) -> &'static str {
        SWARM_SERVICE_ACTIVATION
    }

    pub fn service_scopes(&self) -> &'static [&'static str] {
        &["agent", "session"]
    }
}
