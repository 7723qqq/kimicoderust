use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use kimi_core::primitives::errors::Error2;
use serde_json::{Map, Value, json};

use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchLike, FetchResponse};
use crate::app::auth::auth::BearerTokenProvider;
use crate::app::auth::errors::AUTH_TOKEN_MISSING;
use crate::app::telemetry::telemetry::{ITelemetryService, noop_telemetry_service};

use super::super::errors::register_web_errors;
use super::super::tools::fetch_url_types::{
    HttpFetchError, UrlFetchKind, UrlFetchOptions, UrlFetchResult, UrlFetcher,
};
use super::local_fetch_url::system_fetch;

pub struct MoonshotFetchURLProviderOptions {
    pub token_provider: Option<Arc<dyn BearerTokenProvider>>,
    pub api_key: Option<String>,
    pub base_url: String,
    pub default_headers: Option<HashMap<String, String>>,
    pub custom_headers: Option<HashMap<String, String>>,
    pub local_fallback: Arc<dyn UrlFetcher>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
    pub telemetry: Option<Arc<dyn ITelemetryService>>,
}

pub struct MoonshotFetchURLProvider {
    token_provider: Option<Arc<dyn BearerTokenProvider>>,
    api_key: Option<String>,
    base_url: String,
    default_headers: HashMap<String, String>,
    custom_headers: HashMap<String, String>,
    local_fallback: Arc<dyn UrlFetcher>,
    fetch_impl: Arc<dyn FetchLike>,
    telemetry: Arc<dyn ITelemetryService>,
}

impl MoonshotFetchURLProvider {
    pub fn new(options: MoonshotFetchURLProviderOptions) -> Self {
        register_web_errors();
        Self {
            token_provider: options.token_provider,
            api_key: options.api_key,
            base_url: options.base_url,
            default_headers: options.default_headers.unwrap_or_default(),
            custom_headers: options.custom_headers.unwrap_or_default(),
            local_fallback: options.local_fallback,
            fetch_impl: options.fetch_impl.unwrap_or_else(system_fetch),
            telemetry: options.telemetry.unwrap_or_else(noop_telemetry_service),
        }
    }

    async fn fetch_via_moonshot(
        &self,
        url: &str,
        tool_call_id: Option<&str>,
        signal: Option<&tokio_util::sync::CancellationToken>,
        attempt: &mut bool,
    ) -> Result<String, FetchError> {
        let body_json = json!({ "url": url }).to_string();
        let response = self
            .post(body_json, tool_call_id, signal, attempt)
            .await?;

        if response.status != 200 {
            let status = response.status;
            let detail = response.text().await.unwrap_or_default();
            let message = format!("Moonshot fetch request failed: HTTP {status}. {detail}")
                .trim()
                .to_string();
            return Err(Box::new(HttpFetchError::new(status, message)));
        }
        response.text().await
    }

    async fn post(
        &self,
        body_json: String,
        tool_call_id: Option<&str>,
        signal: Option<&tokio_util::sync::CancellationToken>,
        attempt: &mut bool,
    ) -> Result<FetchResponse, FetchError> {
        let access_token = self.resolve_api_key().await?;
        *attempt = true;
        let mut headers = self
            .default_headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<IndexMap<String, String>>();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        headers.insert("Accept".to_string(), "text/markdown".to_string());
        headers.insert(
            "Content-Type".to_string(),
            "application/json".to_string(),
        );
        if let Some(tool_call_id) = tool_call_id.filter(|id| !id.is_empty()) {
            headers.insert(
                "X-Msh-Tool-Call-Id".to_string(),
                tool_call_id.to_string(),
            );
        }
        for (name, value) in &self.custom_headers {
            headers.insert(name.clone(), value.clone());
        }
        self.fetch_impl
            .fetch(
                &self.base_url,
                FetchInit {
                    headers: headers.into_iter().collect(),
                    method: Some("POST".to_string()),
                    body: Some(body_json),
                    signal: signal.cloned(),
                    ..Default::default()
                },
            )
            .await
    }

    async fn resolve_api_key(&self) -> Result<String, FetchError> {
        if let Some(token_provider) = &self.token_provider {
            match token_provider.get_access_token(false).await {
                Ok(token) if !token.trim().is_empty() => return Ok(token),
                Ok(_) => {
                    if let Some(api_key) = self.api_key.as_ref().filter(|key| !key.is_empty()) {
                        return Ok(api_key.clone());
                    }
                }
                Err(error) => {
                    if let Some(api_key) = self.api_key.as_ref().filter(|key| !key.is_empty()) {
                        return Ok(api_key.clone());
                    }
                    return Err(error);
                }
            }
        }
        if let Some(api_key) = self.api_key.as_ref().filter(|key| !key.is_empty()) {
            return Ok(api_key.clone());
        }
        Err(Box::new(Error2::new(
            AUTH_TOKEN_MISSING,
            "Moonshot fetch service is not configured: missing API key or token provider.",
        )))
    }
}

#[async_trait]
impl UrlFetcher for MoonshotFetchURLProvider {
    async fn fetch(
        &self,
        url: &str,
        options: UrlFetchOptions,
    ) -> Result<UrlFetchResult, FetchError> {
        let mut credential_resolved = false;
        match self
            .fetch_via_moonshot(
                url,
                options.tool_call_id.as_deref(),
                options.signal.as_ref(),
                &mut credential_resolved,
            )
            .await
        {
            Ok(content) => Ok(UrlFetchResult {
                content,
                kind: UrlFetchKind::Extracted,
            }),
            Err(error) => {
                if options.signal.as_ref().is_some_and(|signal| signal.is_cancelled()) {
                    return Err(error);
                }
                let properties = Map::from_iter([
                    (
                        "error_type".to_string(),
                        Value::String(classify_fetch_error(error.as_ref())),
                    ),
                    (
                        "used_api_key".to_string(),
                        Value::Bool(credential_resolved),
                    ),
                ]);
                self.telemetry
                    .track2("web_fetch_fallback", Some(properties));
                self.local_fallback.fetch(url, options).await
            }
        }
    }
}

fn classify_fetch_error(error: &(dyn Error + Send + Sync + 'static)) -> String {
    if let Some(error) = error.downcast_ref::<HttpFetchError>() {
        return format!("http_{}", error.status);
    }
    if let Some(error) = error.downcast_ref::<Error2>() {
        return error.code().as_str().to_string();
    }
    std::any::type_name_of_val(error)
        .rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("Unknown")
        .to_string()
}
