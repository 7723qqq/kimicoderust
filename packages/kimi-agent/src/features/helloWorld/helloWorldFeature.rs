pub const HELLO_WORLD_FEATURE_NAME: &str = "helloWorld";
pub const HELLO_WORLD_SERVICE_SCOPE: &str = "agent";
pub const HELLO_WORLD_SERVICE_ACTIVATION: &str = "onScopeCreated";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HelloWorldFeature;

impl HelloWorldFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        HELLO_WORLD_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        HELLO_WORLD_SERVICE_SCOPE
    }

    pub fn service_activation(&self) -> &'static str {
        HELLO_WORLD_SERVICE_ACTIVATION
    }
}
