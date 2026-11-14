use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::domains::web::WEB_FETCH_FAILED;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::utils::fetch_like::{FetchError, FetchInit, FetchLike, FetchResponse};
use crate::app::auth::auth::BearerTokenProvider;
use crate::app::auth::errors::AUTH_TOKEN_MISSING;
use crate::app::auth::web_search::web_search::{WebSearchOptions, WebSearchProvider, WebSearchResult};
use crate::mcp_core::client_shared::ReqwestFetch;

pub struct MoonshotWebSearchProviderOptions {
    pub token_provider: Option<Arc<dyn BearerTokenProvider>>,
    pub api_key: Option<String>,
    pub base_url: String,
    pub default_headers: Option<HashMap<String, String>>,
    pub custom_headers: Option<HashMap<String, String>>,
    pub fetch_impl: Option<Arc<dyn FetchLike>>,
}

#[derive(Serialize)]
struct MoonshotSearchRequest<'a> {
    text_query: &'a str,
    search_mode: &'static str,
}

#[derive(Deserialize)]
struct MoonshotSearchResult {
    site_name: Option<String>,
    title: Option<String>,
    url: Option<String>,
    snippet: Option<String>,
    content: Option<String>,
    date: Option<String>,
    icon: Option<String>,
    mime: Option<String>,
}

#[derive(Deserialize)]
struct MoonshotSearchResponse {
    search_results: Option<Vec<MoonshotSearchResult>>,
}

pub struct MoonshotWebSearchProvider {
    token_provider: Option<Arc<dyn BearerTokenProvider>>,
    api_key: Option<String>,
    base_url: String,
    default_headers: HashMap<String, String>,
    custom_headers: HashMap<String, String>,
    fetch_impl: Arc<dyn FetchLike>,
}

impl MoonshotWebSearchProvider {
    pub fn new(options: MoonshotWebSearchProviderOptions) -> Self {
        Self {
            token_provider: options.token_provider,
            api_key: options.api_key,
            base_url: options.base_url,
            default_headers: options.default_headers.unwrap_or_default(),
            custom_headers: options.custom_headers.unwrap_or_default(),
            fetch_impl: options
                .fetch_impl
                .unwrap_or_else(|| Arc::new(ReqwestFetch::new())),
        }
    }

    async fn post(
        &self,
        body_json: String,
        tool_call_id: Option<&str>,
        signal: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<FetchResponse, FetchError> {
        let access_token = self.resolve_api_key().await?;
        let mut headers = self.default_headers.clone();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        );
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        if let Some(tool_call_id) = tool_call_id.filter(|id| !id.is_empty()) {
            headers.insert("X-Msh-Tool-Call-Id".to_string(), tool_call_id.to_string());
        }
        for (name, value) in &self.custom_headers {
            headers.insert(name.clone(), value.clone());
        }
        self.fetch_impl
            .fetch(
                &self.base_url,
                FetchInit {
                    method: Some("POST".to_string()),
                    headers: headers.into_iter().collect(),
                    body: Some(body_json),
                    signal,
                    ..Default::default()
                },
            )
            .await
    }

    async fn resolve_api_key(&self) -> Result<String, FetchError> {
        if let Some(token_provider) = &self.token_provider {
            match token_provider.get_access_token(false).await {
                Ok(token) => return Ok(token),
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
            "Moonshot search service is not configured: missing API key or token provider.",
        )))
    }
}

#[async_trait]
impl WebSearchProvider for MoonshotWebSearchProvider {
    async fn search(
        &self,
        query: &str,
        options: WebSearchOptions,
    ) -> Result<Vec<WebSearchResult>, FetchError> {
        let body_json = serde_json::to_string(&MoonshotSearchRequest {
            text_query: query,
            search_mode: "links",
        })?;
        let response = self
            .post(
                body_json,
                options.tool_call_id.as_deref(),
                options.signal,
            )
            .await?;
        let status = response.status;
        if status == 401 {
            let detail = safe_read_text(response).await;
            return Err(Box::new(Error2::with_options(
                WEB_FETCH_FAILED,
                format!("Moonshot search request failed: HTTP 401 (auth/unauthorized). {detail}")
                    .trim()
                    .to_string(),
                Error2Options {
                    details: Some(Map::from_iter([(
                        "status".to_string(),
                        Value::from(status),
                    )])),
                    ..Default::default()
                },
            )));
        }
        if status != 200 {
            let detail = safe_read_text(response).await;
            return Err(Box::new(Error2::with_options(
                WEB_FETCH_FAILED,
                format!("Moonshot search request failed: HTTP {status}. {detail}")
                    .trim()
                    .to_string(),
                Error2Options {
                    details: Some(Map::from_iter([(
                        "status".to_string(),
                        Value::from(status),
                    )])),
                    ..Default::default()
                },
            )));
        }
        let body = response.text().await?;
        let json: MoonshotSearchResponse = serde_json::from_str(&body)?;
        let raw = json.search_results.unwrap_or_default();
        Ok(raw
            .into_iter()
            .map(|result| {
                let mut out = WebSearchResult {
                    title: result.title.unwrap_or_default(),
                    url: result.url.unwrap_or_default(),
                    snippet: result.snippet.unwrap_or_default(),
                    site_name: None,
                    date: None,
                };
                if let Some(date) = result.date.filter(|date| !date.is_empty()) {
                    out.date = Some(date);
                }
                if let Some(site_name) = result.site_name.filter(|name| !name.is_empty()) {
                    out.site_name = Some(site_name);
                }
                out
            })
            .collect())
    }
}

async fn safe_read_text(response: FetchResponse) -> String {
    response.text().await.unwrap_or_default()
}
