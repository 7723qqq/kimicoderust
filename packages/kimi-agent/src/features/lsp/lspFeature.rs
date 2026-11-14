pub const LSP_FEATURE_NAME: &str = "lsp";
pub const LSP_SERVICE_SCOPE: &str = "session";
pub const LSP_TOOL_NAME: &str = "lsp";
pub const LSP_TOOL_DOMAIN: &str = "lsp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspFeature {
    enabled: bool,
}

impl LspFeature {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn name(&self) -> &'static str {
        LSP_FEATURE_NAME
    }

    pub fn contributes_services(&self) -> bool {
        self.enabled
    }

    pub fn service_scope(&self) -> &'static str {
        LSP_SERVICE_SCOPE
    }
}
