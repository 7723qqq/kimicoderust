use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::llm_adapter::protocol::protocol::Protocol;
use crate::llm_adapter::provider::provider::OAuthRef;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUpgrade {
    pub model: String,
    pub migration_markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<Protocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub beta_api: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ModelUpgrade>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<ModelOverride>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub type ModelsSection = IndexMap<String, ModelRecord>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsChangedEvent {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultModelChangedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

pub trait IModelService: Send + Sync {
    fn ready(&self) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn on_did_change_models(
        &self,
        listener: Arc<dyn Fn(ModelsChangedEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn on_did_change_default_model(
        &self,
        listener: Arc<dyn Fn(DefaultModelChangedEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn get(&self, id: &str) -> Option<ModelRecord>;
    fn list(&self) -> ModelsSection;
    fn get_default_model(&self) -> Option<String>;
    fn set(&self, id: &str, model: ModelRecord) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn delete(&self, id: &str) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn load_all(&self, models: ModelsSection, default_model: Option<String>);
    fn replace_all(&self, models: ModelsSection) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn set_default_model(
        &self,
        id: Option<String>,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>>;
}
