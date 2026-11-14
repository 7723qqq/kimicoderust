use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::Error2;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::config_section::ConfigSchema;
use crate::llm_adapter::contract::errors::CONFIG_INVALID_ERROR_CODE;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRefreshChange {
    pub provider_id: String,
    pub provider_name: String,
    pub added: u64,
    pub removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRefreshFailure {
    pub provider: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredDefault {
    pub from: String,
    pub to: String,
    pub from_display_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshProviderModelsResponse {
    pub changed: Vec<ProviderRefreshChange>,
    pub unchanged: Vec<String>,
    pub failed: Vec<ProviderRefreshFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_default: Option<RetiredDefault>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefreshProviderModelsScope {
    All,
    Oauth,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshProviderModelsOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<RefreshProviderModelsScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredModel {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogChanged {
    pub payload: RefreshProviderModelsResponse,
}

impl ModelCatalogChanged {
    pub const TYPE: &'static str = "event.model_catalog.changed";

    pub fn new(payload: RefreshProviderModelsResponse) -> Self {
        Self { payload }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub changed: Vec<ProviderRefreshChange>,
    pub unchanged: Vec<String>,
    pub failed: Vec<ProviderRefreshFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_default: Option<RetiredDefault>,
}

impl From<ModelCatalogChanged> for ModelCatalogChangedEvent {
    fn from(event: ModelCatalogChanged) -> Self {
        Self {
            type_: ModelCatalogChanged::TYPE.to_string(),
            changed: event.payload.changed,
            unchanged: event.payload.unchanged,
            failed: event.payload.failed,
            retired_default: event.payload.retired_default,
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, Error2> {
    serde_json::from_value(value)
        .map_err(|error| Error2::new(CONFIG_INVALID_ERROR_CODE, error.to_string()))
}

fn required(value: &str, field: &str) -> Result<(), Error2> {
    if value.is_empty() {
        return Err(Error2::new(
            CONFIG_INVALID_ERROR_CODE,
            format!("{field} must not be empty"),
        ));
    }
    Ok(())
}

impl ProviderRefreshChange {
    fn validate(&self) -> Result<(), Error2> {
        required(&self.provider_id, "provider_id")?;
        required(&self.provider_name, "provider_name")
    }
}

impl ProviderRefreshFailure {
    fn validate(&self) -> Result<(), Error2> {
        required(&self.provider, "provider")?;
        required(&self.reason, "reason")
    }
}

impl RetiredDefault {
    fn validate(&self) -> Result<(), Error2> {
        required(&self.from, "from")?;
        required(&self.to, "to")?;
        required(&self.from_display_name, "from_display_name")
    }
}

impl RefreshProviderModelsResponse {
    fn validate(&self) -> Result<(), Error2> {
        for change in &self.changed {
            change.validate()?;
        }
        for provider in &self.unchanged {
            required(provider, "unchanged provider")?;
        }
        for failure in &self.failed {
            failure.validate()?;
        }
        if let Some(retired) = &self.retired_default {
            retired.validate()?;
        }
        Ok(())
    }
}

pub struct ProviderRefreshChangeSchema;
pub struct ProviderRefreshFailureSchema;
pub struct RetiredDefaultSchema;
pub struct RefreshProviderModelsResponseSchema;

pub static PROVIDER_REFRESH_CHANGE_SCHEMA: ProviderRefreshChangeSchema = ProviderRefreshChangeSchema;
pub static PROVIDER_REFRESH_FAILURE_SCHEMA: ProviderRefreshFailureSchema = ProviderRefreshFailureSchema;
pub static RETIRED_DEFAULT_SCHEMA: RetiredDefaultSchema = RetiredDefaultSchema;
pub static REFRESH_PROVIDER_MODELS_RESPONSE_SCHEMA: RefreshProviderModelsResponseSchema =
    RefreshProviderModelsResponseSchema;

impl ConfigSchema for ProviderRefreshChangeSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ProviderRefreshChange = decode(value)?;
        decoded.validate()?;
        serde_json::to_value(decoded)
            .map_err(|error| Error2::new(CONFIG_INVALID_ERROR_CODE, error.to_string()))
    }
}

impl ConfigSchema for ProviderRefreshFailureSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: ProviderRefreshFailure = decode(value)?;
        decoded.validate()?;
        serde_json::to_value(decoded)
            .map_err(|error| Error2::new(CONFIG_INVALID_ERROR_CODE, error.to_string()))
    }
}

impl ConfigSchema for RetiredDefaultSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: RetiredDefault = decode(value)?;
        decoded.validate()?;
        serde_json::to_value(decoded)
            .map_err(|error| Error2::new(CONFIG_INVALID_ERROR_CODE, error.to_string()))
    }
}

impl ConfigSchema for RefreshProviderModelsResponseSchema {
    fn parse(&self, value: Value) -> Result<Value, Error2> {
        let decoded: RefreshProviderModelsResponse = decode(value)?;
        decoded.validate()?;
        serde_json::to_value(decoded)
            .map_err(|error| Error2::new(CONFIG_INVALID_ERROR_CODE, error.to_string()))
    }
}

pub type RetiredModelListener = Arc<dyn Fn(RetiredModel) + Send + Sync>;
pub type Dispose = Box<dyn FnOnce() + Send>;

#[async_trait]
pub trait IProviderDiscoveryService: Send + Sync {
    fn on_did_retire_model(&self, listener: RetiredModelListener) -> Dispose;

    async fn refresh_provider_models(
        &self,
        options: RefreshProviderModelsOptions,
    ) -> Result<RefreshProviderModelsResponse, Error2>;

    fn retired_model_replacement(&self, alias: &str) -> Option<String>;
}
