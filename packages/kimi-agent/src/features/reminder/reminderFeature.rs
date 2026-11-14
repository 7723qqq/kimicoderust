pub const REMINDER_FEATURE_NAME: &str = "reminder";
pub const REMINDER_SERVICE_SCOPE: &str = "agent";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReminderFeature;

impl ReminderFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        REMINDER_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        REMINDER_SERVICE_SCOPE
    }
}
