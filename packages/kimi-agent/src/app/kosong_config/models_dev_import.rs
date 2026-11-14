use std::collections::BTreeMap;
use std::sync::LazyLock;

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevModelItem {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub max_context_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    pub reasoning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevProviderItem {
    pub id: String,
    pub name: String,
    pub wire_type: Option<String>,
    pub base_url: Option<String>,
    pub guessed: bool,
    pub needs_base_url: bool,
    pub rejected: bool,
    pub reject_reason: Option<String>,
    pub env_key: Option<String>,
    pub models: Vec<ModelsDevModelItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderCatalogStatus {
    #[serde(rename = "connected")]
    Connected,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "unconfigured")]
    Unconfigured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCatalogItem {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    pub has_api_key: bool,
    pub status: ProviderCatalogStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
}

pub static PROVIDER_ID_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\p{L}\p{N}][\p{L}\p{N}\-_ ]*$").expect("valid provider id pattern")
});

pub fn is_valid_provider_id(value: &str) -> bool {
    PROVIDER_ID_PATTERN.is_match(value)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportModelsDevProviderOptions {
    pub catalog_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportModelsDevProviderResult {
    pub provider: ProviderCatalogItem,
    pub models_imported: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCustomRegistryOptions {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_default_when_unset: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportCustomRegistryResult {
    pub providers: Vec<ProviderCatalogItem>,
    pub models_imported: usize,
    pub credential_env: BTreeMap<String, String>,
}

#[async_trait]
pub trait IModelsDevImportService: Send + Sync {
    async fn list_models_dev_providers(&self) -> Result<Vec<ModelsDevProviderItem>, Error2>;
    async fn get_models_dev_provider(&self, catalog_id: &str) -> Result<ModelsDevProviderItem, Error2>;
    async fn import_models_dev_provider(
        &self,
        options: ImportModelsDevProviderOptions,
    ) -> Result<ImportModelsDevProviderResult, Error2>;
    async fn import_custom_registry(
        &self,
        options: ImportCustomRegistryOptions,
    ) -> Result<ImportCustomRegistryResult, Error2>;
}
