pub const CRON_FEATURE_NAME: &str = "cron";
pub const CRON_SERVICE_SCOPE: &str = "agent";
pub const CRON_TOOL_DOMAIN: &str = "cron";
pub const CRON_TOOL_NAMES: &[&str] = &["CronCreate", "CronList", "CronDelete"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CronFeature;

impl CronFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        CRON_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        CRON_SERVICE_SCOPE
    }

    pub fn tool_names(&self) -> &'static [&'static str] {
        CRON_TOOL_NAMES
    }
}
