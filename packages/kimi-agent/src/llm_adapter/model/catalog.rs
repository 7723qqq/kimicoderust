use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::human::llm::requester::requester::LlmCredentialProvider;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::capability::ModelCapability;
use crate::llm_adapter::model::model_auth::effective_model_config;
use crate::llm_adapter::model::model_requester::{
    ModelRequestEvent, ModelRequestInput, ModelRequestParams, ModelRequestStream, ModelRequester,
};
use crate::llm_adapter::protocol::protocol::{Protocol, ProtocolProviderOptions};
use crate::llm_adapter::provider::provider::ProviderConfig;
use crate::wire::record::WireAsyncError;

use super::model::{ModelRecord, ModelUpgrade};

#[derive(Clone)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub protocol: Protocol,
    pub base_url: Option<String>,
    pub headers: HashMap<String, String>,
    pub capabilities: ModelCapability,
    pub max_context_size: u32,
    pub max_input_size: Option<u32>,
    pub max_output_size: Option<u32>,
    pub display_name: Option<String>,
    pub reasoning_key: Option<String>,
    pub support_efforts: Option<Vec<String>>,
    pub default_effort: Option<String>,
    pub always_thinking: Option<bool>,
    pub adaptive_thinking: Option<bool>,
    pub provider_type: Option<String>,
    pub provider_name: String,
    pub credential_provider: Option<Arc<dyn LlmCredentialProvider>>,
    pub provider_options: Option<ProtocolProviderOptions>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPingResult {
    pub ok: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCatalogItem {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub max_context_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ProtocolModelUpgrade>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolModelUpgrade {
    pub model: String,
    pub migration_markdown: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCatalogStatus {
    Connected,
    Error,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetDefaultModelResponse {
    pub default_model: String,
    pub model: ModelCatalogItem,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCredentialState {
    pub has_api_key: bool,
    pub has_oauth_token: bool,
    pub has_credential_conflict: bool,
}

fn to_protocol_model_upgrade(upgrade: Option<&ModelUpgrade>) -> Option<ProtocolModelUpgrade> {
    upgrade.map(|upgrade| ProtocolModelUpgrade {
        model: upgrade.model.clone(),
        migration_markdown: upgrade.migration_markdown.clone(),
        retirement_at: upgrade.retirement_at.clone(),
    })
}

pub fn to_protocol_model(
    model: &Model,
    record: &ModelRecord,
    provider_type: Option<&str>,
) -> ModelCatalogItem {
    ModelCatalogItem {
        provider: model.provider_name.clone(),
        model: model.id.clone(),
        display_name: Some(
            model
                .display_name
                .clone()
                .unwrap_or_else(|| model.name.clone()),
        ),
        max_context_size: model.max_context_size,
        capabilities: effective_model_config(
            record,
            provider_type.or(model.provider_type.as_deref()),
        )
        .capabilities,
        support_efforts: model.support_efforts.clone(),
        default_effort: model.default_effort.clone(),
        upgrade: to_protocol_model_upgrade(record.upgrade.as_ref()),
    }
}

pub fn to_protocol_model_fallback(
    model_id: &str,
    record: &ModelRecord,
    provider_type: Option<&str>,
) -> ModelCatalogItem {
    let effective = effective_model_config(record, provider_type);
    ModelCatalogItem {
        provider: effective.provider.clone().unwrap_or_default(),
        model: model_id.to_string(),
        display_name: Some(
            effective
                .display_name
                .clone()
                .or_else(|| effective.model.clone())
                .unwrap_or_else(|| model_id.to_string()),
        ),
        max_context_size: effective.max_context_size.unwrap_or(0),
        capabilities: effective.capabilities.clone(),
        support_efforts: effective.support_efforts.clone(),
        default_effort: effective.default_effort.clone(),
        upgrade: to_protocol_model_upgrade(record.upgrade.as_ref()),
    }
}

pub fn to_protocol_provider(
    provider_id: &str,
    provider: &ProviderConfig,
    models: &IndexMap<String, ModelRecord>,
    global_default_model: Option<&str>,
    credential: &ProviderCredentialState,
) -> ProviderCatalogItem {
    let provider_models = model_ids_for_provider(models, provider_id);
    let default_model = provider.default_model.clone().or_else(|| {
        global_default_for_provider(models, global_default_model, provider_id)
    });
    ProviderCatalogItem {
        id: provider_id.to_string(),
        type_: provider.type_.clone().unwrap_or_else(|| "openai".to_string()),
        base_url: provider.base_url.clone(),
        default_model,
        api_key_env: provider.api_key_env.clone(),
        has_api_key: credential.has_api_key,
        status: if credential.has_credential_conflict {
            ProviderCatalogStatus::Error
        } else if credential.has_api_key || credential.has_oauth_token {
            ProviderCatalogStatus::Connected
        } else {
            ProviderCatalogStatus::Unconfigured
        },
        models: Some(provider_models),
    }
}

pub fn model_ids_for_provider(
    models: &IndexMap<String, ModelRecord>,
    provider_id: &str,
) -> Vec<String> {
    models
        .iter()
        .filter(|(_, record)| record.provider.as_deref() == Some(provider_id))
        .map(|(model_id, _)| model_id.clone())
        .collect()
}

pub fn global_default_for_provider(
    models: &IndexMap<String, ModelRecord>,
    global_default_model: Option<&str>,
    provider_id: &str,
) -> Option<String> {
    let model_id = global_default_model?;
    (models.get(model_id)?.provider.as_deref() == Some(provider_id))
        .then(|| model_id.to_string())
}

#[async_trait]
pub trait IModelCatalog: Send + Sync {
    fn get(&self, id: &str) -> Result<Model, WireAsyncError>;
    fn get_requester(&self, id: &str) -> Result<Arc<dyn ModelRequester>, WireAsyncError>;
    fn generate(
        &self,
        id: &str,
        input: ModelRequestInput,
        signal: Option<CancellationToken>,
        params: Option<ModelRequestParams>,
    ) -> Result<ModelRequestStream, WireAsyncError>;
    async fn ping(&self, id: &str) -> ModelPingResult;
    fn find_by_name(&self, name: &str) -> Vec<String>;
    async fn list_models(&self) -> Result<Vec<ModelCatalogItem>, WireAsyncError>;
    async fn list_providers(&self) -> Result<Vec<ProviderCatalogItem>, WireAsyncError>;
    async fn get_provider(&self, provider_id: &str) -> Result<ProviderCatalogItem, WireAsyncError>;
    async fn set_default_model(
        &self,
        model_id: &str,
    ) -> Result<SetDefaultModelResponse, WireAsyncError>;
}
