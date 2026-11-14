pub const SPEC_FEATURE_NAME: &str = "spec";
pub const SPEC_SERVICE_SCOPE: &str = "agent";
pub const SPEC_TOOL_DOMAIN: &str = "spec";
pub const SPEC_TOOL_NAMES: &[&str] = &["EnterSpecMode", "ExitSpecMode"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpecFeature;

impl SpecFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        SPEC_FEATURE_NAME
    }

    pub fn service_scope(&self) -> &'static str {
        SPEC_SERVICE_SCOPE
    }

    pub fn tool_names(&self) -> &'static [&'static str] {
        SPEC_TOOL_NAMES
    }
}
