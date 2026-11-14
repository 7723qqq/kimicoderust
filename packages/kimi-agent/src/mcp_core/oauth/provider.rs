use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::utils::fetch_like::{FetchInit, FetchLike};
use crate::mcp_core::client_http::OAuthClientProvider;
use crate::mcp_core::client_shared::KIMI_MCP_CLIENT_NAME;
use crate::wire::record::WireAsyncError;

use super::store::{
    McpOAuthStore, McpOAuthStoreMeta, canonical_mcp_oauth_resource, mcp_oauth_store_key,
};

const TOKENS_SUFFIX: &str = "-tokens.json";
const CLIENT_SUFFIX: &str = "-client.json";
const DISCOVERY_SUFFIX: &str = "-discovery.json";
const PASSIVE_REDIRECT_URI: &str = "http://127.0.0.1:3118/callback";

pub use super::store::META_SUFFIX;
pub type StoredMcpOAuthTokens = OAuthTokens;
pub type OAuthClientInformationMixed = Value;
pub type OAuthDiscoveryState = Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obtained_at: Option<i64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClientMetadata {
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub client_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthInvalidationScope {
    All,
    Client,
    Tokens,
    Verifier,
    Discovery,
}

impl OAuthInvalidationScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Client => "client",
            Self::Tokens => "tokens",
            Self::Verifier => "verifier",
            Self::Discovery => "discovery",
        }
    }
}

#[derive(Debug, Clone)]
struct SharedProviderError(Arc<String>);

impl SharedProviderError {
    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedProviderError {}

pub struct McpOAuthProviderOptions {
    pub server_name: String,
    pub server_url: String,
    pub store: Arc<dyn McpOAuthStore>,
    pub client_label: Option<String>,
    pub client_name: Option<String>,
    pub now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    pub on_tokens_saved: Option<Arc<dyn Fn(StoredMcpOAuthTokens) + Send + Sync>>,
    pub on_credentials_invalidated: Option<Arc<dyn Fn(OAuthInvalidationScope) + Send + Sync>>,
    pub track: Option<Arc<dyn Fn(BoxFuture<'static, Result<(), WireAsyncError>>) + Send + Sync>>,
}

type TokenOperation = Shared<BoxFuture<'static, Result<(), SharedProviderError>>>;

static TOKEN_OPERATIONS: std::sync::LazyLock<Mutex<HashMap<String, (u64, TokenOperation)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static NEXT_TOKEN_OPERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

struct TokenTransaction {
    key: String,
    store: Arc<dyn McpOAuthStore>,
    tokens_file: String,
    meta_file: String,
    server_name: String,
    server_url: String,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    on_tokens_saved: Option<Arc<dyn Fn(StoredMcpOAuthTokens) + Send + Sync>>,
    track: Option<Arc<dyn Fn(BoxFuture<'static, Result<(), WireAsyncError>>) + Send + Sync>>,
}

impl TokenTransaction {
    fn save(&self, mut tokens: OAuthTokens) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone_handle();
        let work = this.clone_handle();
        let operation = this.run_exclusive(async move {
            if tokens.obtained_at.is_none() {
                tokens.obtained_at = Some((work.now)());
            }
            work.store
                .write(&work.tokens_file, serde_json::to_value(&tokens)?)
                .await?;
            let meta = McpOAuthStoreMeta {
                server_name: work.server_name.clone(),
                server_url: work.server_url.clone(),
            };
            work.store
                .write(&work.meta_file, serde_json::to_value(meta)?)
                .await?;
            if let Some(listener) = &work.on_tokens_saved {
                listener(tokens);
            }
            Ok(())
        }.boxed());
        this.track_operation(operation.clone());
        async move { operation.await.map_err(SharedProviderError::boxed) }.boxed()
    }

    fn clear(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone_handle();
        let work = this.clone_handle();
        let operation = this.run_exclusive(async move {
            work.store.remove(&work.tokens_file).await?;
            work.store.remove(&work.meta_file).await?;
            Ok(())
        }.boxed());
        async move { operation.await.map_err(SharedProviderError::boxed) }.boxed()
    }

    fn clear_if_current(&self, expected: &OAuthTokens) -> BoxFuture<'static, Result<bool, WireAsyncError>> {
        let this = self.clone_handle();
        let work = this.clone_handle();
        let expected = expected.clone();
        let cleared = Arc::new(Mutex::new(None));
        let outcome = cleared.clone();
        let operation = this.run_exclusive(async move {
            let current = work.tokens().await?;
            if current.as_ref() != Some(&expected) {
                *outcome.lock() = Some(false);
                return Ok(());
            }
            work.store.remove(&work.tokens_file).await?;
            work.store.remove(&work.meta_file).await?;
            *outcome.lock() = Some(true);
            Ok(())
        }.boxed());
        this.track_operation(operation.clone());
        async move {
            operation.await.map_err(SharedProviderError::boxed)?;
            Ok(cleared.lock().unwrap())
        }
        .boxed()
    }

    fn run_exclusive(&self, work: BoxFuture<'static, Result<(), WireAsyncError>>) -> TokenOperation {
        let key = self.key.clone();
        let previous = TOKEN_OPERATIONS
            .lock()
            .get(&key)
            .map(|(_, operation)| operation.clone());
        let id = NEXT_TOKEN_OPERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let operation = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            work.await.map_err(|error| SharedProviderError(Arc::new(error.to_string())))
        }
        .boxed()
        .shared();
        TOKEN_OPERATIONS.lock().insert(key.clone(), (id, operation.clone()));
        let cleanup = operation.clone();
        tokio::spawn(async move {
            let _ = cleanup.await;
            let mut operations = TOKEN_OPERATIONS.lock();
            if operations.get(&key).is_some_and(|(current, _)| *current == id) {
                operations.remove(&key);
            }
        });
        operation
    }

    fn track_operation(&self, operation: TokenOperation) {
        if let Some(track) = &self.track {
            let tracked = operation
                .map(|result| result.map_err(|error| Box::new(error) as WireAsyncError))
                .boxed();
            track(tracked);
        }
    }

    fn tokens(&self) -> BoxFuture<'static, Result<Option<OAuthTokens>, WireAsyncError>> {
        let store = self.store.clone();
        let tokens_file = self.tokens_file.clone();
        async move {
            let value = store.read(&tokens_file).await?;
            value.map(serde_json::from_value).transpose().map_err(|error| error.into())
        }
        .boxed()
    }

    fn create_fetch(&self, inner: Arc<dyn FetchLike>) -> Arc<dyn FetchLike> {
        Arc::new(OAuthFetch {
            transaction: self.clone_handle(),
            inner,
        })
    }

    fn clone_handle(&self) -> Self {
        Self {
            key: self.key.clone(),
            store: self.store.clone(),
            tokens_file: self.tokens_file.clone(),
            meta_file: self.meta_file.clone(),
            server_name: self.server_name.clone(),
            server_url: self.server_url.clone(),
            now: self.now.clone(),
            on_tokens_saved: self.on_tokens_saved.clone(),
            track: self.track.clone(),
        }
    }
}

struct OAuthFetch {
    transaction: TokenTransaction,
    inner: Arc<dyn FetchLike>,
}

#[async_trait]
impl FetchLike for OAuthFetch {
    async fn fetch(&self, url: &str, mut init: FetchInit) -> Result<crate::_base::utils::fetch_like::FetchResponse, crate::_base::utils::fetch_like::FetchError> {
        let token_grant = init.body.as_deref().is_some_and(|body| {
            url::form_urlencoded::parse(body.as_bytes())
                .find(|(key, _)| key == "grant_type")
                .is_some_and(|(_, value)| value == "refresh_token" || value == "authorization_code")
        });
        if token_grant {
            init.timeout_ms = Some(30_000);
        }
        self.inner.fetch(url, init).await
    }
}

pub struct McpOAuthClientProvider {
    pub store_key: String,
    pub server_url: String,
    server_name: String,
    store: Arc<dyn McpOAuthStore>,
    client_label: String,
    on_credentials_invalidated: Option<Arc<dyn Fn(OAuthInvalidationScope) + Send + Sync>>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    redirect_url: Mutex<Option<String>>,
    code_verifier: Mutex<Option<String>>,
    state: Mutex<Option<String>>,
    last_authorization_url: Mutex<Option<String>>,
    token_transaction: TokenTransaction,
    client_cache: Arc<Mutex<Option<OAuthClientInformationMixed>>>,
    discovery_cache: Arc<Mutex<Option<OAuthDiscoveryState>>>,
    ready: Shared<BoxFuture<'static, Result<(), SharedProviderError>>>,
}

impl McpOAuthClientProvider {
    pub fn new(options: McpOAuthProviderOptions) -> Self {
        let server_url = canonical_mcp_oauth_resource(&options.server_url).unwrap();
        let store_key = mcp_oauth_store_key(&options.server_name, &server_url).unwrap();
        let client_label = options.client_label.unwrap_or_else(|| {
            format!(
                "{} ({})",
                options.client_name.unwrap_or_else(|| KIMI_MCP_CLIENT_NAME.to_string()),
                options.server_name
            )
        });
        let now = options.now.unwrap_or_else(|| Arc::new(|| chrono::Utc::now().timestamp_millis()));
        let token_transaction = TokenTransaction {
            key: store_key.clone(),
            store: options.store.clone(),
            tokens_file: format!("{store_key}{TOKENS_SUFFIX}"),
            meta_file: format!("{store_key}{}", super::store::META_SUFFIX),
            server_name: options.server_name.clone(),
            server_url: server_url.clone(),
            now: now.clone(),
            on_tokens_saved: options.on_tokens_saved,
            track: options.track,
        };
        let client_cache = Arc::new(Mutex::new(None));
        let discovery_cache = Arc::new(Mutex::new(None));
        let store = options.store.clone();
        let store_key_for_load = store_key.clone();
        let client_cache_for_load = client_cache.clone();
        let discovery_cache_for_load = discovery_cache.clone();
        let ready = async move {
            let (client, discovery) = tokio::join!(
                store.read(&format!("{store_key_for_load}{CLIENT_SUFFIX}")),
                store.read(&format!("{store_key_for_load}{DISCOVERY_SUFFIX}"))
            );
            let client = client?;
            let discovery = discovery?;
            *client_cache_for_load.lock() = client;
            *discovery_cache_for_load.lock() = discovery;
            Ok(())
        }
        .boxed()
        .shared();
        tokio::spawn(ready.clone());
        Self {
            store_key,
            server_url,
            server_name: options.server_name,
            store: options.store,
            client_label,
            on_credentials_invalidated: options.on_credentials_invalidated,
            now,
            redirect_url: Mutex::new(None),
            code_verifier: Mutex::new(None),
            state: Mutex::new(None),
            last_authorization_url: Mutex::new(None),
            token_transaction,
            client_cache,
            discovery_cache,
            ready,
        }
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let ready = self.ready.clone();
        async move { ready.await.map_err(SharedProviderError::boxed) }.boxed()
    }

    pub fn set_redirect_url(&self, url: &str) {
        *self.redirect_url.lock() = Some(url.to_string());
    }

    pub fn take_authorization_url(&self) -> Option<String> {
        self.last_authorization_url.lock().take()
    }

    pub fn expected_state(&self) -> Option<String> {
        self.state.lock().clone()
    }

    pub fn reset_flow(&self) {
        *self.redirect_url.lock() = None;
        *self.code_verifier.lock() = None;
        *self.state.lock() = None;
        *self.last_authorization_url.lock() = None;
    }

    pub fn redirect_url(&self) -> String {
        self.effective_redirect_uri()
    }

    pub fn client_metadata(&self) -> OAuthClientMetadata {
        OAuthClientMetadata {
            redirect_uris: vec![self.effective_redirect_uri()],
            token_endpoint_auth_method: "none".to_string(),
            grant_types: vec!["authorization_code".to_string(), "refresh_token".to_string()],
            response_types: vec!["code".to_string()],
            client_name: self.client_label.clone(),
            scope: None,
        }
    }

    pub fn state(&self) -> String {
        let mut state = self.state.lock();
        if state.is_none() {
            let mut bytes = [0u8; 16];
            getrandom::fill(&mut bytes).unwrap();
            *state = Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect());
        }
        state.clone().unwrap()
    }

    pub async fn client_information(&self) -> Result<Option<OAuthClientInformationMixed>, WireAsyncError> {
        self.ready().await?;
        Ok(self.client_cache.lock().clone())
    }

    pub async fn save_client_information(&self, info: OAuthClientInformationMixed) -> Result<(), WireAsyncError> {
        self.store
            .write(&format!("{}{CLIENT_SUFFIX}", self.store_key), info.clone())
            .await?;
        *self.client_cache.lock() = Some(info);
        Ok(())
    }

    pub async fn tokens(&self) -> Result<Option<OAuthTokens>, WireAsyncError> {
        self.token_transaction.tokens().await
    }

    pub async fn save_tokens(&self, tokens: OAuthTokens) -> Result<(), WireAsyncError> {
        self.token_transaction.save(tokens).await
    }

    pub fn create_oauth_fetch(&self, fetch: Option<Arc<dyn FetchLike>>) -> Arc<dyn FetchLike> {
        self.token_transaction.create_fetch(
            fetch.unwrap_or_else(|| Arc::new(crate::mcp_core::client_shared::ReqwestFetch::new())),
        )
    }

    pub fn redirect_to_authorization(&self, url: &str) {
        *self.last_authorization_url.lock() = Some(url.to_string());
    }

    pub fn save_code_verifier(&self, verifier: &str) {
        *self.code_verifier.lock() = Some(verifier.to_string());
    }

    pub fn code_verifier(&self) -> Result<String, WireAsyncError> {
        self.code_verifier.lock().clone().ok_or_else(|| {
            Box::new(BugIndicatingError::new(
                "McpOAuthClientProvider: PKCE code verifier not initialized",
            )) as WireAsyncError
        })
    }

    pub async fn save_discovery_state(&self, state: OAuthDiscoveryState) -> Result<(), WireAsyncError> {
        self.store
            .write(&format!("{}{DISCOVERY_SUFFIX}", self.store_key), state.clone())
            .await?;
        *self.discovery_cache.lock() = Some(state);
        Ok(())
    }

    pub async fn discovery_state(&self) -> Result<Option<OAuthDiscoveryState>, WireAsyncError> {
        self.ready().await?;
        Ok(self.discovery_cache.lock().clone())
    }

    pub async fn invalidate_stale_registration(&self, redirect_uri: &str) -> Result<bool, WireAsyncError> {
        self.ready().await?;
        let info = self.client_cache.lock().clone();
        let Some(info) = info else {
            return Ok(false);
        };
        let Some(uris) = info.get("redirect_uris").and_then(Value::as_array) else {
            return Ok(false);
        };
        if uris.is_empty() || uris.iter().any(|uri| uri.as_str() == Some(redirect_uri)) {
            return Ok(false);
        }
        self.clear_credentials(OAuthInvalidationScope::Client).await?;
        Ok(true)
    }

    pub async fn invalidate_credentials(&self, scope: OAuthInvalidationScope) -> Result<(), WireAsyncError> {
        if !matches!(scope, OAuthInvalidationScope::Tokens | OAuthInvalidationScope::All) {
            return self.clear_credentials(scope).await;
        }
        if self.token_transaction.tokens().await?.is_none() {
            return Ok(());
        }
        self.token_transaction.clear().await?;
        if scope == OAuthInvalidationScope::All {
            self.clear_credentials(OAuthInvalidationScope::Client).await?;
            self.clear_credentials(OAuthInvalidationScope::Discovery).await?;
            *self.code_verifier.lock() = None;
        }
        if let Some(listener) = &self.on_credentials_invalidated {
            listener(scope);
        }
        Ok(())
    }

    pub async fn clear_credentials(&self, scope: OAuthInvalidationScope) -> Result<(), WireAsyncError> {
        if scope == OAuthInvalidationScope::Verifier {
            *self.code_verifier.lock() = None;
            if let Some(listener) = &self.on_credentials_invalidated {
                listener(scope);
            }
            return Ok(());
        }
        if matches!(scope, OAuthInvalidationScope::Tokens | OAuthInvalidationScope::All) {
            self.token_transaction.clear().await?;
        }
        if matches!(scope, OAuthInvalidationScope::Client | OAuthInvalidationScope::All) {
            self.store
                .remove(&format!("{}{CLIENT_SUFFIX}", self.store_key))
                .await?;
            *self.client_cache.lock() = None;
        }
        if matches!(scope, OAuthInvalidationScope::Discovery | OAuthInvalidationScope::All) {
            self.store
                .remove(&format!("{}{DISCOVERY_SUFFIX}", self.store_key))
                .await?;
            *self.discovery_cache.lock() = None;
        }
        if scope == OAuthInvalidationScope::All {
            *self.code_verifier.lock() = None;
        }
        if let Some(listener) = &self.on_credentials_invalidated {
            listener(scope);
        }
        Ok(())
    }

    pub async fn clear_tokens_if_current(&self, expected: &OAuthTokens) -> Result<bool, WireAsyncError> {
        let cleared = self.token_transaction.clear_if_current(expected).await?;
        if cleared {
            if let Some(listener) = &self.on_credentials_invalidated {
                listener(OAuthInvalidationScope::Tokens);
            }
        }
        Ok(cleared)
    }

    fn effective_redirect_uri(&self) -> String {
        if let Some(url) = self.redirect_url.lock().clone() {
            return url;
        }
        self.client_cache
            .lock()
            .as_ref()
            .and_then(|info| info.get("redirect_uris"))
            .and_then(Value::as_array)
            .and_then(|uris| uris.first())
            .and_then(Value::as_str)
            .unwrap_or(PASSIVE_REDIRECT_URI)
            .to_string()
    }

    pub fn now(&self) -> i64 {
        (self.now)()
    }
}

#[async_trait]
impl OAuthClientProvider for McpOAuthClientProvider {
    async fn authorize(&self) -> Result<Option<String>, WireAsyncError> {
        Ok(self.tokens().await?.map(|tokens| tokens.access_token))
    }
}

pub fn create_mcp_oauth_fetch(
    provider: Option<&McpOAuthClientProvider>,
    fetch: Option<Arc<dyn FetchLike>>,
) -> Option<Arc<dyn FetchLike>> {
    match provider {
        Some(provider) => Some(provider.create_oauth_fetch(fetch)),
        None => fetch,
    }
}
