pub const SESSION_QUERY_FEATURE_NAME: &str = "sessionQuery";
pub const SESSION_QUERY_SERVICE_SCOPE: &str = "app";
pub const SESSION_QUERY_TOOL_NAME: &str = "session_query";
pub const SESSION_QUERY_TOOL_DOMAIN: &str = "sessionQuery";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionQueryFeature;

impl SessionQueryFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        SESSION_QUERY_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        SESSION_QUERY_SERVICE_SCOPE
    }

    pub fn main_agent_only(&self) -> bool {
        true
    }
}
