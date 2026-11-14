use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::lspService::LspProviderRegistration;
use super::protocol::{LspHover, LspLocation, LspPosition};
use kimi_core::primitives::errors::Error2;

pub const LSP_SERVICE_ID: &str = "lspService";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum LspOperation {
    #[serde(rename = "goToDefinition")]
    GoToDefinition,
    #[serde(rename = "findReferences")]
    FindReferences,
    #[serde(rename = "goToImplementation")]
    GoToImplementation,
    #[serde(rename = "hover")]
    Hover,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspQueryRequest {
    pub operation: LspOperation,
    pub file_path: String,
    pub position: LspPosition,
    pub workspace_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LspProviderQuery {
    pub operation: LspOperation,
    pub file_path: String,
    pub position: LspPosition,
    pub workspace_root: String,
    pub language_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LspQueryResult {
    Locations { locations: Vec<LspLocation> },
    Hover { hover: Option<LspHover> },
}

#[async_trait]
pub trait LspProvider: Send + Sync {
    fn id(&self) -> &str;
    fn extension_to_language(&self) -> &IndexMap<String, String>;
    async fn query(
        &self,
        request: LspProviderQuery,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2>;
}

#[async_trait]
pub trait ILspService: Send + Sync {
    fn register_provider(
        &self,
        provider: std::sync::Arc<dyn LspProvider>,
    ) -> Result<LspProviderRegistration, Error2>;

    async fn query(
        &self,
        request: LspQueryRequest,
        signal: Option<CancellationToken>,
    ) -> Result<LspQueryResult, Error2>;
}
