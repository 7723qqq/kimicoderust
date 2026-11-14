pub const CODE_RUNTIME_FEATURE_NAME: &str = "codeRuntime";
pub const RUN_CODE_TOOL_NAME: &str = "run_code";
pub const RUN_CODE_TOOL_DOMAIN: &str = "codeRuntime";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeRuntimeFeature;

impl CodeRuntimeFeature {
    pub fn new() -> Self {
        Self
    }

    pub fn name(&self) -> &'static str {
        CODE_RUNTIME_FEATURE_NAME
    }

    pub fn tool_name(&self) -> &'static str {
        RUN_CODE_TOOL_NAME
    }

    pub fn tool_domain(&self) -> &'static str {
        RUN_CODE_TOOL_DOMAIN
    }
}
