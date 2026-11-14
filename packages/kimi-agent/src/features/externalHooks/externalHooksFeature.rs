pub const EXTERNAL_HOOKS_FEATURE_NAME: &str = "externalHooks";
pub const EXTERNAL_HOOKS_SERVICE_SCOPES: &[&str] = &["app", "session", "agent"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExternalHooksFeature;

impl ExternalHooksFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        EXTERNAL_HOOKS_FEATURE_NAME
    }

    pub fn service_scopes(&self) -> &'static [&'static str] {
        EXTERNAL_HOOKS_SERVICE_SCOPES
    }
}
