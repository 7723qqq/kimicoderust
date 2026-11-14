use std::collections::HashMap;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::IDisposable;
use crate::wire::record::WireAsyncError;

pub use crate::app::kosong_config::config_section::{ModelSource, OAuthRef, OAuthStorage};

pub type ProviderType = String;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_source: Option<ModelSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Map<String, Value>>,
}

pub type ProvidersSection = IndexMap<String, ProviderConfig>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvidersChangedEvent {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultProviderChangedEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

pub trait IProviderService: Send + Sync {
    fn ready(&self) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn on_did_change_providers(
        &self,
        listener: Arc<dyn Fn(ProvidersChangedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn on_did_change_default_provider(
        &self,
        listener: Arc<dyn Fn(DefaultProviderChangedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn get(&self, name: &str) -> Option<ProviderConfig>;
    fn list(&self) -> ProvidersSection;
    fn get_default_provider(&self) -> Option<String>;
    fn set(
        &self,
        name: &str,
        config: ProviderConfig,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn delete(&self, name: &str) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn load_all(&self, providers: ProvidersSection, default_provider: Option<String>);
    fn replace_all(
        &self,
        providers: ProvidersSection,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>>;
    fn set_default_provider(
        &self,
        id: Option<String>,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>>;
}
