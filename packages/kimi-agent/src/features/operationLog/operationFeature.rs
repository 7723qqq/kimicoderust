pub const OPERATION_LOG_FEATURE_NAME: &str = "operationLog";
pub const OPERATION_LOG_SERVICE_SCOPE: &str = "agent";
pub const OPERATION_LOG_SERVICE_ACTIVATION: &str = "onScopeCreated";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OperationFeature;

impl OperationFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        OPERATION_LOG_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        OPERATION_LOG_SERVICE_SCOPE
    }

    pub fn service_activation(&self) -> &'static str {
        OPERATION_LOG_SERVICE_ACTIVATION
    }
}
