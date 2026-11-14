pub const TOKEN_COUNTING_FEATURE_NAME: &str = "tokenCounting";
pub const TOKEN_COUNTING_SERVICE_SCOPE: &str = "session";
pub const TOKEN_COUNTING_AGENT_MODEL: &str = "TokenCountingAgentModelDefinition";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCountingFeature;

impl TokenCountingFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        TOKEN_COUNTING_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        TOKEN_COUNTING_SERVICE_SCOPE
    }

    pub fn agent_model(&self) -> &'static str {
        TOKEN_COUNTING_AGENT_MODEL
    }
}
