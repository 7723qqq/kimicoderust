use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::app::kosong_config::config_section::OAuthRef;
use crate::llm_adapter::contract::errors::CONFIG_INVALID_ERROR_CODE;
use crate::llm_adapter::model::model_auth::api_key_env_missing_message;
use crate::region::KimiRegion;
use crate::wire::record::WireAsyncError;

pub type AuthManagedUsageResult = ManagedUsageResult;
pub type AuthManagedUserInfoResult = ManagedUserInfoResult;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KimiOAuthTokenRef {
    pub key: Option<String>,
    pub oauth_host: Option<String>,
}

impl From<&OAuthRef> for KimiOAuthTokenRef {
    fn from(reference: &OAuthRef) -> Self {
        Self {
            key: Some(reference.key.clone()),
            oauth_host: reference.oauth_host.clone(),
        }
    }
}

use super::errors::{
    AUTH_MODEL_NOT_RESOLVED, AUTH_PROVISIONING_REQUIRED, AUTH_TOKEN_MISSING, register_auth_errors,
};
use super::oauth_protocol::{
    ManagedUsageResult, ManagedUserInfoResult, OAuthFlowSnapshot, OAuthFlowStart,
    OAuthLoginCancelResponse, OAuthLogoutResponse, RefreshOAuthProviderModelsResponse,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub logged_in: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OAuthLoginOptions {
    pub region: Option<KimiRegion>,
}

#[derive(Debug, Clone, Default)]
pub struct OAuthManagedOptions {
    pub oauth_ref: Option<KimiOAuthTokenRef>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: Option<u64>,
    pub interval: u64,
}

#[derive(Debug, Clone)]
pub struct KimiOAuthLoginOptions {
    pub signal: CancellationToken,
    pub oauth_ref: Option<KimiOAuthTokenRef>,
    pub base_url: Option<String>,
    pub oauth_host: Option<String>,
    pub on_device_code: Arc<dyn Fn(DeviceAuthorization) + Send + Sync>,
    pub provision_config: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct KimiOAuthLoginResult {
    pub provider_name: String,
    pub ok: bool,
    pub provision: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct KimiOAuthLogoutResult {
    pub provider_name: String,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub message: String,
}

impl OAuthError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for OAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for OAuthError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodeTimeoutError {
    pub message: String,
}

impl DeviceCodeTimeoutError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Default for DeviceCodeTimeoutError {
    fn default() -> Self {
        Self::new("Device authorization timed out locally.")
    }
}

impl fmt::Display for DeviceCodeTimeoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DeviceCodeTimeoutError {}

#[derive(Debug, Clone, Default)]
pub struct ResolveKimiCodeRuntimeAuthOptions {
    pub configured_base_url: Option<String>,
    pub configured_oauth_ref: Option<OAuthRef>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolveKimiCodeLoginAuthOptions {
    pub configured_base_url: Option<String>,
    pub configured_oauth_ref: Option<OAuthRef>,
    pub requested_base_url: Option<String>,
    pub requested_oauth_host: Option<String>,
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolveKimiCodeOAuthRefOptions {
    pub oauth_host: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiRegionLoginHosts {
    pub oauth_host: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct KimiCodeLoginAuth {
    pub oauth_ref: Option<OAuthRef>,
    pub base_url: Option<String>,
    pub oauth_host: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct KimiCodeRuntimeAuth {
    pub oauth_ref: Option<OAuthRef>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ApplyManagedKimiCodeConfigOptions {
    pub models: Vec<Value>,
    pub base_url: Option<String>,
    pub oauth_key: String,
    pub oauth_host: Option<String>,
    pub preserve_default_model: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ClearManagedKimiCodeConfigResult {
    pub removed_provider: bool,
    pub removed_models: Vec<String>,
    pub default_model_cleared: bool,
    pub removed_services: Vec<String>,
}

#[async_trait]
pub trait OAuthSupport: Send + Sync {
    fn kimi_code_base_url(&self) -> String;
    fn kimi_region_login_hosts(
        &self,
        region: KimiRegion,
        env: &HashMap<String, String>,
    ) -> Option<KimiRegionLoginHosts>;
    fn resolve_kimi_code_login_auth(
        &self,
        options: ResolveKimiCodeLoginAuthOptions,
    ) -> KimiCodeLoginAuth;
    fn resolve_kimi_code_oauth_ref(&self, options: ResolveKimiCodeOAuthRefOptions) -> OAuthRef;
    fn resolve_kimi_code_runtime_auth(
        &self,
        options: ResolveKimiCodeRuntimeAuthOptions,
    ) -> KimiCodeRuntimeAuth;
    async fn fetch_managed_kimi_code_models(
        &self,
        access_token: String,
        base_url: Option<String>,
    ) -> Result<Vec<Value>, WireAsyncError>;
    fn apply_managed_kimi_code_config(
        &self,
        config: &mut Value,
        options: ApplyManagedKimiCodeConfigOptions,
    );
    fn clear_managed_kimi_code_config(
        &self,
        config: &mut Value,
    ) -> ClearManagedKimiCodeConfigResult;
}

#[async_trait]
pub trait BearerTokenProvider: Send + Sync {
    async fn get_access_token(&self, force: bool) -> Result<String, WireAsyncError>;
}

#[async_trait]
pub trait IOAuthService: Send + Sync {
    async fn start_login(
        &self,
        provider: Option<&str>,
        options: Option<OAuthLoginOptions>,
    ) -> Result<OAuthFlowStart, WireAsyncError>;
    fn get_flow(&self, provider: Option<&str>) -> Option<OAuthFlowSnapshot>;
    async fn cancel_login(
        &self,
        provider: Option<&str>,
    ) -> Result<OAuthLoginCancelResponse, WireAsyncError>;
    async fn logout(&self, provider: Option<&str>) -> Result<OAuthLogoutResponse, WireAsyncError>;
    async fn status(&self, provider: Option<&str>) -> Result<AuthStatus, WireAsyncError>;
    async fn refresh_oauth_provider_models(
        &self,
    ) -> Result<RefreshOAuthProviderModelsResponse, WireAsyncError>;
    async fn get_managed_usage(
        &self,
        provider: Option<&str>,
    ) -> Result<AuthManagedUsageResult, WireAsyncError>;
    async fn get_managed_user_info(
        &self,
        provider: Option<&str>,
    ) -> Result<AuthManagedUserInfoResult, WireAsyncError>;
    fn resolve_token_provider(
        &self,
        provider: &str,
        oauth_ref: &OAuthRef,
    ) -> Option<Arc<dyn BearerTokenProvider>>;
    fn resolve_token_provider_optional(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Option<Arc<dyn BearerTokenProvider>>;
    async fn get_cached_access_token(
        &self,
        provider: &str,
        oauth_ref: Option<&OAuthRef>,
    ) -> Result<Option<String>, WireAsyncError>;
    fn get_region(&self) -> KimiRegion;
    async fn get_region_async(&self) -> KimiRegion;
}

#[async_trait]
pub trait IOAuthToolkit: Send + Sync {
    async fn login(
        &self,
        provider_name: Option<String>,
        options: KimiOAuthLoginOptions,
    ) -> Result<KimiOAuthLoginResult, WireAsyncError>;
    async fn logout(
        &self,
        provider_name: Option<&str>,
        oauth_ref: Option<&KimiOAuthTokenRef>,
    ) -> Result<KimiOAuthLogoutResult, WireAsyncError>;
    async fn get_cached_access_token(
        &self,
        provider_name: Option<&str>,
        oauth_ref: Option<&KimiOAuthTokenRef>,
    ) -> Result<Option<String>, WireAsyncError>;
    fn token_provider(
        &self,
        provider_name: Option<&str>,
        oauth_ref: Option<&KimiOAuthTokenRef>,
    ) -> Option<Arc<dyn BearerTokenProvider>>;
    async fn get_managed_usage(
        &self,
        provider_name: Option<&str>,
        options: OAuthManagedOptions,
    ) -> Result<AuthManagedUsageResult, WireAsyncError>;
    async fn get_managed_user_info(
        &self,
        provider_name: Option<&str>,
        options: OAuthManagedOptions,
    ) -> Result<AuthManagedUserInfoResult, WireAsyncError>;
}

#[async_trait]
pub trait IAuthSummaryService: Send + Sync {
    async fn summarize(&self) -> Result<Vec<AuthStatus>, WireAsyncError>;
    async fn ensure_ready(&self, model_override: Option<&str>) -> Result<(), WireAsyncError>;
}

#[derive(Debug)]
pub struct AuthProvisioningRequiredError {
    inner: Error2,
}

impl AuthProvisioningRequiredError {
    pub fn new() -> Self {
        register_auth_errors();
        Self {
            inner: Error2::with_options(
                AUTH_PROVISIONING_REQUIRED,
                "no provider configured; complete onboarding via /login or the providers endpoint",
                Error2Options {
                    name: Some("AuthProvisioningRequiredError".to_string()),
                    ..Default::default()
                },
            ),
        }
    }
}

impl Default for AuthProvisioningRequiredError {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct AuthTokenMissingError {
    inner: Error2,
    pub provider_id: String,
}

impl AuthTokenMissingError {
    pub fn new(provider_id: impl Into<String>) -> Self {
        register_auth_errors();
        let provider_id = provider_id.into();
        let mut details = Map::new();
        details.insert(
            "provider_id".to_string(),
            Value::String(provider_id.clone()),
        );
        Self {
            inner: Error2::with_options(
                AUTH_TOKEN_MISSING,
                format!("provider {provider_id} has no credential configured"),
                Error2Options {
                    details: Some(details),
                    name: Some("AuthTokenMissingError".to_string()),
                    ..Default::default()
                },
            ),
            provider_id,
        }
    }
}

#[derive(Debug)]
pub struct AuthCredentialEnvMissingError {
    inner: Error2,
    pub provider_id: String,
}

impl AuthCredentialEnvMissingError {
    pub fn new(provider_id: impl Into<String>, env_name: &str) -> Self {
        register_auth_errors();
        let provider_id = provider_id.into();
        let mut details = Map::new();
        details.insert(
            "provider_id".to_string(),
            Value::String(provider_id.clone()),
        );
        Self {
            inner: Error2::with_options(
                CONFIG_INVALID_ERROR_CODE,
                api_key_env_missing_message(&provider_id, env_name),
                Error2Options {
                    details: Some(details),
                    name: Some("AuthCredentialEnvMissingError".to_string()),
                    ..Default::default()
                },
            ),
            provider_id,
        }
    }
}

#[derive(Debug)]
pub struct AuthModelNotResolvedError {
    inner: Error2,
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
}

impl AuthModelNotResolvedError {
    pub fn new(model_id: Option<String>, provider_id: Option<String>) -> Self {
        register_auth_errors();
        let mut details = Map::new();
        if let Some(model_id) = &model_id {
            details.insert("model_id".to_string(), Value::String(model_id.clone()));
        }
        if let Some(provider_id) = &provider_id {
            details.insert(
                "provider_id".to_string(),
                Value::String(provider_id.clone()),
            );
        }
        let message = match &model_id {
            Some(model_id) => {
                format!("model {model_id} does not resolve to a configured provider")
            }
            None => "no default model configured".to_string(),
        };
        Self {
            inner: Error2::with_options(
                AUTH_MODEL_NOT_RESOLVED,
                message,
                Error2Options {
                    details: (!details.is_empty()).then_some(details),
                    name: Some("AuthModelNotResolvedError".to_string()),
                    ..Default::default()
                },
            ),
            model_id,
            provider_id,
        }
    }
}

macro_rules! impl_auth_error {
    ($type:ty) => {
        impl Deref for $type {
            type Target = Error2;

            fn deref(&self) -> &Self::Target {
                &self.inner
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.inner.fmt(formatter)
            }
        }

        impl Error for $type {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                self.inner.source()
            }
        }
    };
}

impl_auth_error!(AuthProvisioningRequiredError);
impl_auth_error!(AuthTokenMissingError);
impl_auth_error!(AuthCredentialEnvMissingError);
impl_auth_error!(AuthModelNotResolvedError);
