use super::super::feature::Feature;

pub const USAGE_FEATURE_NAME: &str = "usage";
pub const USAGE_SERVICE_SCOPE: &str = "session";
pub const USAGE_SERVICE_ID: &str = "sessionUsageService";
pub const USAGE_AGENT_MODEL: &str = "usage";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageFeature;

impl UsageFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        USAGE_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        USAGE_SERVICE_SCOPE
    }

    pub fn agent_model(&self) -> &'static str {
        USAGE_AGENT_MODEL
    }

    pub fn assemble(&self) -> Feature {
        let mut feature = Feature::new(USAGE_FEATURE_NAME);
        feature.contribute_agent_model(USAGE_AGENT_MODEL);
        feature.contribute_service(USAGE_SERVICE_SCOPE, USAGE_SERVICE_ID, None);
        feature
    }
}
