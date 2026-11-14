pub const NOTIFY_FEATURE_NAME: &str = "notify";
pub const NOTIFY_SERVICE_SCOPE: &str = "agent";
pub const NOTIFY_TOOL_NAME: &str = "NotifyUser";
pub const NOTIFY_TOOL_DOMAIN: &str = "notify";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NotifyFeature;

impl NotifyFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        NOTIFY_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        NOTIFY_SERVICE_SCOPE
    }

    pub fn tool_enabled(&self, session_notify_enabled: bool) -> bool {
        session_notify_enabled
    }
}
