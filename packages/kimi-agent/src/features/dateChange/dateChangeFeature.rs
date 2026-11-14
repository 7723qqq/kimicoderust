pub const DATE_CHANGE_FEATURE_NAME: &str = "dateChange";
pub const DATE_CHANGE_SERVICE_SCOPE: &str = "agent";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DateChangeFeature;

impl DateChangeFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        DATE_CHANGE_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        DATE_CHANGE_SERVICE_SCOPE
    }
}
