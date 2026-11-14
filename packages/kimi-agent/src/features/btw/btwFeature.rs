pub const BTW_FEATURE_NAME: &str = "btw";
pub const BTW_SERVICE_SCOPE: &str = "session";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BtwFeature;

impl BtwFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        BTW_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        BTW_SERVICE_SCOPE
    }
}
