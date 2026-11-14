pub const SESSION_INIT_FEATURE_NAME: &str = "sessionInit";
pub const SESSION_INIT_SERVICE_SCOPE: &str = "session";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionInitFeature;

impl SessionInitFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        SESSION_INIT_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        SESSION_INIT_SERVICE_SCOPE
    }
}
