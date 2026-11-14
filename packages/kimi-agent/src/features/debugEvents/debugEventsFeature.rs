pub const DEBUG_EVENTS_FEATURE_NAME: &str = "debugEvents";
pub const DEBUG_EVENTS_SERVICE_SCOPE: &str = "app";
pub const DEBUG_EVENTS_SERVICE_ACTIVATION: &str = "onDemand";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugEventsFeature;

impl DebugEventsFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        DEBUG_EVENTS_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        DEBUG_EVENTS_SERVICE_SCOPE
    }

    pub fn service_activation(&self) -> &'static str {
        DEBUG_EVENTS_SERVICE_ACTIVATION
    }
}
