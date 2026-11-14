pub const LSP_FLAG_ID: &str = "lsp";
pub const LSP_FLAG_ENV: &str = "KIMI_CODE_EXPERIMENTAL_LSP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LspFlag {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub env: &'static str,
    pub default: bool,
    pub surface: &'static str,
}

pub const LSP_FLAG: LspFlag = LspFlag {
    id: LSP_FLAG_ID,
    title: "LSP tool",
    description: "Expose the lsp tool for go-to-definition, find-references, implementation, and hover queries through language servers configured in the [lsp] config section.",
    env: LSP_FLAG_ENV,
    default: false,
    surface: "core",
};

pub use crate::tools::lsp_tool::lsp_enabled;
