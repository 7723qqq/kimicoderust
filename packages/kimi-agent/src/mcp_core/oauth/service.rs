use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use base64::Engine;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::{AbortController, AbortSignal};
use crate::_base::utils::fetch_like::{FetchInit, FetchLike};
use crate::mcp_core::client_shared::ReqwestFetch;
use crate::mcp_core::errors::MCP_OAUTH_FAILED;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, Error2Options};

use super::callback_server::{CallbackResult, CallbackServer, start_callback_server};
use super::coordinator::McpOAuthCredentialsCoordinator;
use super::provider::{
    McpOAuthClientProvider, McpOAuthProviderOptions, OAuthInvalidationScope, OAuthTokens,
    StoredMcpOAuthTokens,
};
use super::store::{
    McpOAuthStore, McpOAuthStoreMeta, canonical_mcp_oauth_resource, mcp_oauth_store_key,
};

const REFRESH_AHEAD_MS: i64 = 120_000;
const MAX_TIMER_DELAY_MS: i64 = 0x7fffffff;
const DEFAULT_AUTH_REQUEST_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_SHUTDOWN_DRAIN_TIMEOUT_MS: u64 = 30_000;
const OFFLINE_ACCESS_SCOPE: &str = "offline_access";
const CONCURRENT_GRANT_GRACE_MS: i64 = 10_000;

#[derive(Debug, Clone)]
struct SharedOAuthError {
    message: Arc<String>,
    code: Option<String>,
    name: Option<String>,
    details: Option<Map<String, Value>>,
    retryable: Option<bool>,
    status_code: Option<i64>,
    already_authorized: bool,
}

impl SharedOAuthError {
    fn new(error: WireAsyncError) -> Self {
        let already_authorized = error.downcast_ref::<AlreadyAuthorizedError>().is_some();
        let error2 = error
            .downcast_ref::<Error2>()
            .or_else(|| error.downcast_ref::<AlreadyAuthorizedError>().map(|error| &error.0));
        Self {
            message: Arc::new(error.to_string()),
            code: error2.map(|error| error.code().as_str().to_string()),
            name: error2.map(|error| error.name().to_string()),
            details: error2.and_then(|error| error.details().cloned()),
            retryable: error2.and_then(|error| error.retryable()),
            status_code: error2.and_then(|error| error.status_code()),
            already_authorized,
        }
    }

    fn boxed(self) -> WireAsyncError {
        let Some(code) = self.code.clone() else {
            return Box::new(self);
        };
        let mut error = Error2::new(code, self.message.as_str().to_string());
        if let Some(name) = &self.name {
            error = error.with_name(name);
        }
        if let Some(details) = self.details.clone() {
            error = error.with_details(details);
        }
        if let Some(retryable) = self.retryable {
            error = error.with_retryable(retryable);
        }
        if let Some(status_code) = self.status_code {
            error = error.with_status_code(status_code);
        }
        if self.already_authorized {
            Box::new(AlreadyAuthorizedError(error))
        } else {
            Box::new(error)
        }
    }
}

impl fmt::Display for SharedOAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message.as_str())
    }
}

impl Error for SharedOAuthError {}

pub trait McpOAuthScheduledTask: Send + Sync {
    fn cancel(&self);
}

struct TimerTask(tokio::task::JoinHandle<()>);

impl McpOAuthScheduledTask for TimerTask {
    fn cancel(&self) {
        self.0.abort();
    }
}

pub trait McpOAuthScheduler: Send + Sync {
    fn now(&self) -> i64;
    fn schedule(&self, delay_ms: i64, task: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>) -> Arc<dyn McpOAuthScheduledTask>;
}

struct DefaultScheduler;

impl McpOAuthScheduler for DefaultScheduler {
    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn schedule(&self, delay_ms: i64, task: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>) -> Arc<dyn McpOAuthScheduledTask> {
        Arc::new(TimerTask(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms.max(0) as u64)).await;
            task().await;
        })))
    }
}

pub struct McpOAuthServiceOptions {
    pub store: Arc<dyn McpOAuthStore>,
    pub client_label: Option<String>,
    pub resolve_client_name: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub coordinator: Option<Arc<dyn McpOAuthCredentialsCoordinator>>,
    pub log: Option<Arc<dyn ILogService>>,
    pub scheduler: Option<Arc<dyn McpOAuthScheduler>>,
    pub auth_request_timeout_ms: Option<u64>,
    pub shutdown_drain_timeout_ms: Option<u64>,
}

#[derive(Clone)]
pub struct BeginAuthorizationOptions {
    pub client_label: Option<String>,
}

#[derive(Clone, Default)]
pub struct CompleteAuthorizationOptions {
    pub signal: Option<AbortSignal>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone)]
pub struct BeginAuthorizationResult {
    pub wait_id: String,
    pub authorization_url: String,
    pub complete: Arc<dyn Fn(CompleteAuthorizationOptions) -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>,
    pub cancel: Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpOAuthEventType {
    TokensSaved,
    TokensInvalidated,
    RefreshFailed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpOAuthEvent {
    pub type_: McpOAuthEventType,
    pub server_name: String,
    pub server_url: String,
    pub scope: Option<OAuthInvalidationScope>,
    pub error: Option<String>,
}

pub type McpOAuthEventListener = Arc<dyn Fn(McpOAuthEvent) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthTokenState {
    pub has_tokens: bool,
    pub has_refresh_token: bool,
    pub expires_at: Option<i64>,
    pub expired: bool,
}

#[derive(Clone)]
struct ActiveAuthorization {
    started: Shared<BoxFuture<'static, Result<SharedAuthorizationFlow, SharedOAuthError>>>,
    controller: AbortController,
    server_ref: Arc<Mutex<Option<Arc<CallbackServer>>>>,
}

#[derive(Clone)]
struct SharedAuthorizationFlow {
    attach: Arc<dyn Fn() -> BeginAuthorizationResult + Send + Sync>,
    cancel_underlying: Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>,
}

struct OAuthServiceState {
    providers: HashMap<String, Arc<McpOAuthClientProvider>>,
    listeners: IndexMap<u64, McpOAuthEventListener>,
    next_listener_id: u64,
    refreshes: HashMap<String, Shared<BoxFuture<'static, Result<(), SharedOAuthError>>>>,
    refresh_timers: HashMap<String, Arc<dyn McpOAuthScheduledTask>>,
    active_authorizations: HashMap<String, ActiveAuthorization>,
    authorization_waits: HashMap<String, Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>>,
    background_tasks: HashMap<usize, Shared<BoxFuture<'static, ()>>>,
    next_background_task: usize,
    shutting_down: bool,
    shutdown: Option<Shared<BoxFuture<'static, ()>>>,
}

#[derive(Clone)]
pub struct McpOAuthService {
    store: Arc<dyn McpOAuthStore>,
    client_label: Option<String>,
    resolve_client_name: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    coordinator: Option<Arc<dyn McpOAuthCredentialsCoordinator>>,
    log: Arc<dyn ILogService>,
    scheduler: Arc<dyn McpOAuthScheduler>,
    auth_request_timeout_ms: u64,
    shutdown_drain_timeout_ms: u64,
    state: Arc<Mutex<OAuthServiceState>>,
}

impl McpOAuthService {
    pub fn new(options: McpOAuthServiceOptions) -> Self {
        Self {
            store: options.store,
            client_label: options.client_label,
            resolve_client_name: options.resolve_client_name,
            coordinator: options.coordinator,
            log: options.log.unwrap_or_else(|| Arc::new(service_log::NoopLog)),
            scheduler: options.scheduler.unwrap_or_else(|| Arc::new(DefaultScheduler)),
            auth_request_timeout_ms: options.auth_request_timeout_ms.unwrap_or(DEFAULT_AUTH_REQUEST_TIMEOUT_MS),
            shutdown_drain_timeout_ms: options.shutdown_drain_timeout_ms.unwrap_or(DEFAULT_SHUTDOWN_DRAIN_TIMEOUT_MS),
            state: Arc::new(Mutex::new(OAuthServiceState {
                providers: HashMap::new(),
                listeners: IndexMap::new(),
                next_listener_id: 0,
                refreshes: HashMap::new(),
                refresh_timers: HashMap::new(),
                active_authorizations: HashMap::new(),
                authorization_waits: HashMap::new(),
                background_tasks: HashMap::new(),
                next_background_task: 0,
                shutting_down: false,
                shutdown: None,
            })),
        }
    }

    pub fn dispose(&self) -> BoxFuture<'static, ()> {
        self.shutdown()
    }

    pub fn get_provider(&self, server_name: &str, server_url: &str) -> Arc<McpOAuthClientProvider> {
        let store_key = mcp_oauth_store_key(server_name, server_url).unwrap();
        let mut state = self.state.lock();
        if let Some(provider) = state.providers.get(&store_key).cloned() {
            return provider;
        }
        let provider = self.create_provider(server_name, server_url, None);
        state.providers.insert(store_key, provider.clone());
        provider
    }

    pub async fn has_tokens(&self, server_name: &str, server_url: &str) -> Result<bool, WireAsyncError> {
        Ok(self.get_provider(server_name, server_url).tokens().await?.is_some())
    }

    pub async fn token_state(&self, server_name: &str, server_url: &str) -> Result<McpOAuthTokenState, WireAsyncError> {
        let tokens = self.get_provider(server_name, server_url).tokens().await?;
        let Some(tokens) = tokens else {
            return Ok(McpOAuthTokenState {
                has_tokens: false,
                has_refresh_token: false,
                expires_at: None,
                expired: false,
            });
        };
        let expires_at = match (tokens.obtained_at, tokens.expires_in) {
            (Some(obtained), Some(expires)) => Some(obtained + expires * 1000),
            _ => None,
        };
        Ok(McpOAuthTokenState {
            has_tokens: true,
            has_refresh_token: tokens.refresh_token.as_ref().is_some_and(|token| !token.is_empty()),
            expires_at,
            expired: expires_at.is_some_and(|expires| self.scheduler.now() >= expires),
        })
    }

    pub fn on_event(&self, listener: McpOAuthEventListener) -> Box<dyn FnOnce() + Send> {
        let id = {
            let mut state = self.state.lock();
            let existing = state
                .listeners
                .iter()
                .find(|(_, current)| Arc::ptr_eq(current, &listener))
                .map(|(id, _)| *id);
            if let Some(id) = existing {
                id
            } else {
                let id = state.next_listener_id;
                state.next_listener_id += 1;
                state.listeners.insert(id, listener);
                id
            }
        };
        let state = self.state.clone();
        Box::new(move || {
            state.lock().listeners.shift_remove(&id);
        })
    }

    fn track_background_task(&self, future: Shared<BoxFuture<'static, ()>>) {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_background_task;
            state.next_background_task += 1;
            state.background_tasks.insert(id, future.clone());
            id
        };
        let state = self.state.clone();
        tokio::spawn(async move {
            future.await;
            state.lock().background_tasks.remove(&id);
        });
    }

    pub fn refresh(&self, server_name: &str, server_url: &str) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let store_key = mcp_oauth_store_key(server_name, server_url).unwrap();
        let mut state = self.state.lock();
        if let Some(existing) = state.refreshes.get(&store_key).cloned() {
            return async move { existing.await.map_err(SharedOAuthError::boxed) }.boxed();
        }
        if state.shutting_down {
            return async { Err(Box::new(Error2::new(MCP_OAUTH_FAILED, "MCP OAuth service is shutting down")) as WireAsyncError) }.boxed();
        }
        let this = self.clone_handle();
        let server_name = server_name.to_string();
        let server_url = server_url.to_string();
        let store_key_for_task = store_key.clone();
        let task = async move {
            let result = this.refresh_now(&server_name, &server_url).await;
            this.state.lock().refreshes.remove(&store_key_for_task);
            result.map_err(SharedOAuthError::new)
        }
        .boxed()
        .shared();
        state.refreshes.insert(store_key, task.clone());
        drop(state);
        tokio::spawn(task.clone());
        async move { task.await.map_err(SharedOAuthError::boxed) }.boxed()
    }

    pub async fn sweep_proactive_refresh(&self) -> Result<(), WireAsyncError> {
        if self.state.lock().shutting_down {
            return Ok(());
        }
        for key in self.store.list(None).await? {
            if self.state.lock().shutting_down {
                return Ok(());
            }
            if !key.ends_with(super::store::META_SUFFIX) {
                continue;
            }
            let Some(meta) = read_store_meta(self.store.as_ref(), &key, self.log.as_ref()).await? else {
                continue;
            };
            match self.token_state(&meta.server_name, &meta.server_url).await {
                Ok(state) => {
                    if state.has_tokens && state.has_refresh_token {
                        if let Some(expires_at) = state.expires_at {
                            self.schedule_refresh(&meta.server_name, &meta.server_url, expires_at);
                        }
                    }
                }
                Err(error) => {
                    self.log.warn(
                        "skipping MCP OAuth credential during proactive-refresh sweep",
                        Some(json!({ "file": key, "error": error.to_string() })),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn stop_proactive_refresh(&self) {
        let timers = self
            .state
            .lock()
            .refresh_timers
            .drain()
            .map(|(_, timer)| timer)
            .collect::<Vec<_>>();
        for timer in timers {
            timer.cancel();
        }
    }

    pub fn shutdown(&self) -> BoxFuture<'static, ()> {
        if let Some(shutdown) = self.state.lock().shutdown.clone() {
            return async move { shutdown.await }.boxed();
        }
        self.state.lock().shutting_down = true;
        self.stop_proactive_refresh();
        let authorizations = self.state.lock().active_authorizations.values().map(|active| active.clone()).collect::<Vec<_>>();
        let refreshes = self.state.lock().refreshes.values().cloned().collect::<Vec<_>>();
        self.state.lock().active_authorizations.clear();
        let this = self.clone_handle();
        let (deadline_sender, deadline_receiver) = tokio::sync::oneshot::channel();
        let deadline_sender = Arc::new(Mutex::new(Some(deadline_sender)));
        let deadline_sender_for_task = deadline_sender.clone();
        let deadline_log = this.log.clone();
        let deadline_task = this.scheduler.schedule(
            this.shutdown_drain_timeout_ms as i64,
            Arc::new(move || {
                deadline_log.warn("mcp oauth shutdown drain timed out; continuing teardown", None);
                if let Some(sender) = deadline_sender_for_task.lock().take() {
                    let _ = sender.send(());
                }
                async {}.boxed()
            }),
        );
        let drain_this = this.clone();
        let shutdown = async move {
            let drain = async move {
                let authorization_drain = futures_util::future::join_all(authorizations.into_iter().map(|active| async move {
                    active.controller.abort(None);
                    let server = active.server_ref.lock().clone();
                    if let Some(server) = server {
                        server.close().await;
                    }
                    if let Ok(flow) = active.started.await {
                        (flow.cancel_underlying)().await;
                    }
                }));
                let refresh_drain = futures_util::future::join_all(refreshes);
                let background_drain = drain_this.drain_background_tasks();
                let _ = tokio::join!(authorization_drain, refresh_drain, background_drain);
            };
            let drain_task = tokio::spawn(drain);
            tokio::select! {
                result = drain_task => {
                    if let Err(error) = result {
                        std::panic::resume_unwind(error.into_panic());
                    }
                }
                _ = deadline_receiver => {}
            }
            deadline_task.cancel();
            this.state.lock().listeners.clear();
            this.state.lock().providers.clear();
        }
        .boxed()
        .shared();
        self.state.lock().shutdown = Some(shutdown.clone());
        tokio::spawn(shutdown.clone());
        async move { shutdown.await }.boxed()
    }

    async fn drain_background_tasks(&self) {
        loop {
            let tasks = self.state.lock().background_tasks.values().cloned().collect::<Vec<_>>();
            let _ = futures_util::future::join_all(tasks).await;
            tokio::task::yield_now().await;
            if self.state.lock().background_tasks.is_empty() {
                return;
            }
        }
    }

    fn auth_fetch(&self, provider: Arc<McpOAuthClientProvider>, signals: Vec<AbortSignal>) -> Arc<dyn FetchLike> {
        let inner = provider.create_oauth_fetch(None);
        let timeout = self.auth_request_timeout_ms;
        Arc::new(TimeoutFetch { inner, timeout, signals })
    }

    async fn resolve_request_scope(&self, provider: Arc<McpOAuthClientProvider>, server_url: &str, signal: AbortSignal) -> Result<Option<String>, WireAsyncError> {
        let mut discovery = provider.discovery_state().await?;
        if discovery.as_ref().and_then(|value| value.get("authorizationServerMetadata")).is_none() {
            let info = discover_oauth_server_info(
                server_url,
                self.auth_fetch(provider.clone(), vec![signal]),
            )
            .await;
            let Ok(info) = info else {
                return Ok(None);
            };
            let mut next = discovery.clone().unwrap_or_else(|| json!({}));
            next["authorizationServerUrl"] = info["authorizationServerUrl"].clone();
            let resource_metadata = if !info["resourceMetadata"].is_null() {
                Some(info["resourceMetadata"].clone())
            } else {
                discovery.as_ref().and_then(|value| value.get("resourceMetadata")).cloned()
            };
            if let Some(resource_metadata) = resource_metadata {
                next["resourceMetadata"] = resource_metadata;
            } else if let Some(object) = next.as_object_mut() {
                object.remove("resourceMetadata");
            }
            next["authorizationServerMetadata"] = info["authorizationServerMetadata"].clone();
            if provider.save_discovery_state(next.clone()).await.is_err() {
                return Ok(None);
            }
            discovery = Some(next);
        }
        let advertised = discovery
            .as_ref()
            .and_then(|value| value.get("authorizationServerMetadata"))
            .and_then(|value| value.get("scopes_supported"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !advertised.iter().any(|scope| scope.as_str() == Some(OFFLINE_ACCESS_SCOPE)) {
            return Ok(None);
        }
        let base = discovery
            .as_ref()
            .and_then(|value| value.get("resourceMetadata"))
            .and_then(|value| value.get("scopes_supported"))
            .and_then(Value::as_array)
            .map(|scopes| scopes.iter().filter_map(|scope| scope.as_str().map(str::to_string)).collect::<Vec<_>>())
            .or_else(|| {
                provider.client_metadata().scope.map(|scope| {
                    scope.split_whitespace().filter(|part| !part.is_empty()).map(str::to_string).collect::<Vec<_>>()
                })
            })
            .unwrap_or_default();
        if base.iter().any(|scope| scope == OFFLINE_ACCESS_SCOPE) {
            return Ok(None);
        }
        let mut scopes = base;
        scopes.push(OFFLINE_ACCESS_SCOPE.to_string());
        Ok(Some(scopes.join(" ")))
    }

    pub async fn begin_authorization(&self, server_name: &str, server_url: &str, options: BeginAuthorizationOptions) -> Result<BeginAuthorizationResult, WireAsyncError> {
        if self.state.lock().shutting_down {
            return Err(Box::new(Error2::new(MCP_OAUTH_FAILED, "MCP OAuth service is shutting down")));
        }
        let owned_server_name = server_name.to_string();
        let owned_server_url = server_url.to_string();
        let store_key = mcp_oauth_store_key(server_name, server_url).unwrap();
        if let Some(refresh) = self.state.lock().refreshes.get(&store_key).cloned() {
            let _ = refresh.await;
        }
        if self.state.lock().shutting_down {
            return Err(Box::new(Error2::new(MCP_OAUTH_FAILED, "MCP OAuth service is shutting down")));
        }
        let (started, created) = {
            let mut state = self.state.lock();
            if let Some(active) = state.active_authorizations.get(&store_key) {
                (active.started.clone(), false)
            } else {
                let controller = AbortController::new();
                let server_ref = Arc::new(Mutex::new(None));
                let this = self.clone_handle();
                let flow_controller = controller.clone();
                let flow_server_ref = server_ref.clone();
                let started = async move {
                    this.start_authorization_flow(owned_server_name.clone(), owned_server_url.clone(), options, flow_controller, flow_server_ref)
                        .await
                        .map_err(SharedOAuthError::new)
                }
                .boxed()
                .shared();
                state.active_authorizations.insert(store_key.clone(), ActiveAuthorization {
                    started: started.clone(),
                    controller,
                    server_ref,
                });
                (started, true)
            }
        };
        let flow = match started.await {
            Ok(flow) => flow,
            Err(error) => {
                if created {
                    self.state.lock().active_authorizations.remove(&store_key);
                }
                return Err(error.boxed());
            }
        };
        Ok((flow.attach)())
    }

    pub async fn cancel_authorization_wait(&self, wait_id: &str) -> Result<(), WireAsyncError> {
        let cancel = self.state.lock().authorization_waits.get(wait_id).cloned();
        if let Some(cancel) = cancel {
            cancel().await?;
        }
        Ok(())
    }

    async fn start_authorization_flow(
        &self,
        server_name: String,
        server_url: String,
        options: BeginAuthorizationOptions,
        controller: AbortController,
        server_ref: Arc<Mutex<Option<Arc<CallbackServer>>>>,
    ) -> Result<SharedAuthorizationFlow, WireAsyncError> {
        let store_key = mcp_oauth_store_key(&server_name, &server_url).unwrap();
        let provider = if options.client_label.is_some() {
            let provider = self.create_provider(&server_name, &server_url, options.client_label.clone());
            self.state.lock().providers.insert(provider.store_key.clone(), provider.clone());
            provider
        } else {
            self.get_provider(&server_name, &server_url)
        };
        provider.reset_flow();
        let callback_server = start_callback_server().await.map_err(|error| {
            wrap_auth_error("failed to start OAuth callback listener", error)
        })?;
        let callback_server = Arc::new(callback_server);
        *server_ref.lock() = Some(callback_server.clone());
        let signal = controller.signal();
        let mut authorization_url = None;
        let flow_result = async {
            provider.set_redirect_url(&callback_server.redirect_uri);
            provider.ready().await?;
            provider.invalidate_stale_registration(&callback_server.redirect_uri).await?;
            let scope = self.resolve_request_scope(provider.clone(), &server_url, signal.clone()).await?;
            let tokens_saved = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let listener_server_name = server_name.clone();
            let listener_server_url = canonical_mcp_oauth_resource(&server_url)?;
            let tokens_saved_listener = tokens_saved.clone();
            let subscription = self.on_event(Arc::new(move |event| {
                if event.type_ == McpOAuthEventType::TokensSaved
                    && event.server_name == listener_server_name
                    && event.server_url == listener_server_url
                {
                    tokens_saved_listener.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }));
            let result = oauth_authorize(
                provider.clone(),
                &server_url,
                OAuthAuthorizeOptions {
                    scope,
                    authorization_code: None,
                    fetch: self.auth_fetch(provider.clone(), vec![signal.clone()]),
                },
            )
            .await;
            drop(subscription);
            match result? {
                OAuthAuthorizeOutcome::Authorized => {
                    callback_server.close().await;
                    if !tokens_saved.load(std::sync::atomic::Ordering::SeqCst) {
                        self.emit(McpOAuthEvent {
                            type_: McpOAuthEventType::TokensSaved,
                            server_name: server_name.clone(),
                            server_url: canonical_mcp_oauth_resource(&server_url)?,
                            scope: None,
                            error: None,
                        });
                    }
                    return Err(Box::new(AlreadyAuthorizedError::new(&server_name)) as WireAsyncError);
                }
                OAuthAuthorizeOutcome::Redirect => {
                    let Some(url) = provider.take_authorization_url() else {
                        return Err(Box::new(Error2::new(
                            MCP_OAUTH_FAILED,
                            "OAuth provider did not capture an authorization URL",
                        )) as WireAsyncError);
                    };
                    authorization_url = Some(url);
                    Ok(())
                }
            }
        }
        .await;
        if let Err(error) = flow_result {
            callback_server.close().await;
            provider.reset_flow();
            if error.downcast_ref::<AlreadyAuthorizedError>().is_some() {
                return Err(error);
            }
            return Err(wrap_auth_error(&format!("failed to start OAuth flow for \"{server_name}\""), error));
        }
        let authorization_url = authorization_url.unwrap();
        let settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let attached_handles = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_settled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_deadlines = Arc::new(Mutex::new(Vec::<Arc<dyn Fn() + Send + Sync>>::new()));
        let wait_ids = Arc::new(Mutex::new(HashSet::new()));
        let completion = Arc::new(Mutex::new(None::<Shared<BoxFuture<'static, Result<(), SharedOAuthError>>>>));
        let settle = {
            let this = self.clone_handle();
            let provider = provider.clone();
            let callback_server = callback_server.clone();
            let store_key = store_key.clone();
            let server_ref = server_ref.clone();
            let settled = settled.clone();
            let wait_ids = wait_ids.clone();
            Arc::new(move || {
                let this = this.clone();
                let provider = provider.clone();
                let callback_server = callback_server.clone();
                let store_key = store_key.clone();
                let server_ref = server_ref.clone();
                let settled = settled.clone();
                let wait_ids = wait_ids.clone();
                async move {
                    if settled.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    let should_remove = this
                        .state
                        .lock()
                        .active_authorizations
                        .get(&store_key)
                        .is_some_and(|active| Arc::ptr_eq(&active.server_ref, &server_ref));
                    if should_remove {
                        this.state.lock().active_authorizations.remove(&store_key);
                    }
                    for wait_id in wait_ids.lock().drain() {
                        this.state.lock().authorization_waits.remove(&wait_id);
                    }
                    provider.reset_flow();
                    callback_server.close().await;
                }
                .boxed()
            })
        };
        let start_completion = {
            let completion_controller = controller.clone();
            let this = self.clone_handle();
            let provider = provider.clone();
            let callback_server = callback_server.clone();
            let settled = settled.clone();
            let callback_settled = callback_settled.clone();
            let callback_deadlines = callback_deadlines.clone();
            let settle = settle.clone();
            let server_name = server_name.clone();
            let server_url = server_url.clone();
            Arc::new(move || {
                let mut completion_guard = completion.lock();
                if let Some(completion) = completion_guard.clone() {
                    return async move { completion.await.map_err(SharedOAuthError::boxed) }.boxed();
                }
                if settled.load(std::sync::atomic::Ordering::Relaxed) {
                    return async {
                        Err(Box::new(Error2::new(MCP_OAUTH_FAILED, "OAuth flow already completed or cancelled")) as WireAsyncError)
                    }.boxed();
                }
                let this = this.clone();
                let provider = provider.clone();
                let callback_server = callback_server.clone();
                let callback_settled = callback_settled.clone();
                let callback_deadlines = callback_deadlines.clone();
                let settle = settle.clone();
                let server_name = server_name.clone();
                let server_url = server_url.clone();
                let future = async move {
                    let result = async {
                        let callback_result = callback_server.wait_for_code(Some(completion_controller.signal()), None).await;
                        callback_settled.store(true, std::sync::atomic::Ordering::Relaxed);
                        let clears = callback_deadlines.lock().drain(..).collect::<Vec<_>>();
                        for clear in clears {
                            clear();
                        }
                        let CallbackResult { code, state } = callback_result?;
                        if provider.expected_state().is_some_and(|expected| Some(expected.as_str()) != state.as_deref()) {
                            return Err(Box::new(Error2::new(
                                MCP_OAUTH_FAILED,
                                "OAuth state mismatch — possible CSRF; refusing token exchange",
                            )) as WireAsyncError);
                        }
                        let result = oauth_authorize(
                            provider.clone(),
                            &server_url,
                            OAuthAuthorizeOptions {
                                scope: None,
                                authorization_code: Some(code),
                                fetch: this.auth_fetch(provider.clone(), vec![completion_controller.signal()]),
                            },
                        )
                        .await?;
                        if result != OAuthAuthorizeOutcome::Authorized {
                            let mut details = Map::new();
                            details.insert("result".to_string(), result.as_str().into());
                            return Err(Box::new(Error2::with_options(
                                MCP_OAUTH_FAILED,
                                format!("OAuth code exchange returned \"{}\" instead of AUTHORIZED", result.as_str()),
                                Error2Options {
                                    details: Some(details),
                                    ..Default::default()
                                },
                            )) as WireAsyncError);
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = &result {
                        let _ = error;
                    }
                    settle().await;
                    result.map_err(SharedOAuthError::new)
                }
                .boxed()
                .shared();
                *completion_guard = Some(future.clone());
                let completion_future = future.clone();
                let tracked = async move { let _ = future.await; }.boxed().shared();
                this.track_background_task(tracked);
                async move { completion_future.await.map_err(SharedOAuthError::boxed) }.boxed()
            })
        };
        let attach = {
            let this = self.clone_handle();
            let controller = controller.clone();
            let settled = settled.clone();
            let attached_handles = attached_handles.clone();
            let callback_settled = callback_settled.clone();
            let callback_deadlines = callback_deadlines.clone();
            let wait_ids = wait_ids.clone();
            let settle = settle.clone();
            let start_completion = start_completion.clone();
            let server_name = server_name.clone();
            Arc::new(move || {
                attached_handles.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let wait_id = uuid::Uuid::new_v4().to_string();
                let waiter = AbortController::new();
                let detached = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let waiting = Arc::new(Mutex::new(None::<Shared<BoxFuture<'static, Result<(), SharedOAuthError>>>>));
                let detach = {
                    let this = this.clone();
                    let wait_id = wait_id.clone();
                    let wait_ids = wait_ids.clone();
                    let attached_handles = attached_handles.clone();
                    let settled = settled.clone();
                    let detached = detached.clone();
                    let settle = settle.clone();
                    let controller = controller.clone();
                    Arc::new(move || {
                        let this = this.clone();
                        let wait_id = wait_id.clone();
                        let wait_ids = wait_ids.clone();
                        let attached_handles = attached_handles.clone();
                        let settled = settled.clone();
                        let detached = detached.clone();
                        let settle = settle.clone();
                        let controller = controller.clone();
                        async move {
                            if detached.swap(true, std::sync::atomic::Ordering::Relaxed) {
                                return Ok(());
                            }
                            this.state.lock().authorization_waits.remove(&wait_id);
                            wait_ids.lock().remove(&wait_id);
                            if attached_handles.fetch_sub(1, std::sync::atomic::Ordering::Relaxed) == 1
                                && !settled.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                controller.abort(None);
                                settle().await;
                            }
                            Ok(())
                        }
                        .boxed()
                    })
                };
                let cancel = {
                    let waiter = waiter.clone();
                    let detach = detach.clone();
                    Arc::new(move || {
                        let waiter = waiter.clone();
                        let detach = detach.clone();
                        async move {
                            waiter.abort(None);
                            detach().await
                        }
                        .boxed()
                    })
                };
                wait_ids.lock().insert(wait_id.clone());
                this.state.lock().authorization_waits.insert(wait_id.clone(), cancel.clone());
                let complete = {
                    let this = this.clone();
                    let detached = detached.clone();
                    let waiting = waiting.clone();
                    let waiter = waiter.clone();
                    let callback_settled = callback_settled.clone();
                    let callback_deadlines = callback_deadlines.clone();
                    let start_completion = start_completion.clone();
                    let detach = detach.clone();
                    let server_name = server_name.clone();
                    Arc::new(move |options: crate::mcp_core::oauth::service::CompleteAuthorizationOptions| {
                        let this = this.clone();
                        let detached = detached.clone();
                        let waiting = waiting.clone();
                        let waiter = waiter.clone();
                        let callback_settled = callback_settled.clone();
                        let callback_deadlines = callback_deadlines.clone();
                        let start_completion = start_completion.clone();
                        let detach = detach.clone();
                        let server_name = server_name.clone();
                        async move {
                            if detached.load(std::sync::atomic::Ordering::Relaxed) {
                                return Err(Box::new(Error2::new(
                                    MCP_OAUTH_FAILED,
                                    "OAuth flow already completed or cancelled",
                                )) as WireAsyncError);
                            }
                            let future = {
                            let mut waiting_guard = waiting.lock();
                            match waiting_guard.clone() {
                                Some(future) => future,
                                None => {
                                    let deadline = if options.timeout_ms.is_some() && !callback_settled.load(std::sync::atomic::Ordering::Relaxed) {
                                        options.timeout_ms.map(|timeout| TimeoutDeadline::new(waiter.signal(), timeout))
                                    } else {
                                        None
                                    };
                                    if let Some(deadline) = &deadline {
                                        callback_deadlines.lock().push(deadline.clear.clone());
                                    }
                                    let wait_signal = match options.signal {
                                        Some(signal) => AbortSignal::any(&[waiter.signal(), signal]),
                                        None => waiter.signal(),
                                    };
                                    let future = async move {
                                        let completion = (start_completion.clone())();
                                        let completion_signal = match &deadline {
                                            Some(deadline) => AbortSignal::any(&[wait_signal.clone(), deadline.signal.clone()]),
                                            None => wait_signal,
                                        };
                                        let result = tokio::select! {
                                            result = completion => result,
                                            _ = completion_signal.cancelled() => {
                                                if deadline.as_ref().is_some_and(|deadline| deadline.timed_out.load(std::sync::atomic::Ordering::Relaxed)) {
                                                    Err("OAuth callback timed out".into())
                                                } else if let Some(reason) = completion_signal.reason() {
                                                    Err(reason.to_string().into())
                                                } else {
                                                    Err("OAuth authorization wait cancelled".into())
                                                }
                                            }
                                        };
                                        if let Some(deadline) = &deadline {
                                            (deadline.clear)();
                                            callback_deadlines.lock().retain(|clear| !Arc::ptr_eq(clear, &deadline.clear));
                                        }
                                        result.map_err(|error| {
                                            SharedOAuthError::new(wrap_auth_error(&format!("OAuth flow for \"{server_name}\" failed"), error))
                                        })
                                    }
                                    .boxed()
                                    .shared();
                                    *waiting_guard = Some(future.clone());
                                    future
                                }
                            }
                            };
                            let result = future.await.map_err(SharedOAuthError::boxed);
                            detach().await?;
                            result
                        }
                        .boxed()
                    })
                };
                BeginAuthorizationResult {
                    wait_id,
                    authorization_url: authorization_url.clone(),
                    complete,
                    cancel,
                }
            })
        };
        Ok(SharedAuthorizationFlow {
            attach,
            cancel_underlying: settle,
        })
    }

    pub async fn invalidate(&self, server_name: &str, server_url: &str, scope: OAuthInvalidationScope) -> Result<(), WireAsyncError> {
        self.get_provider(server_name, server_url).clear_credentials(scope).await
    }

    pub async fn invalidate_tokens_if_current(&self, server_name: &str, server_url: &str, expected: &OAuthTokens) -> Result<bool, WireAsyncError> {
        self.get_provider(server_name, server_url).clear_tokens_if_current(expected).await
    }

    pub async fn peek_rejected_grant(&self, server_name: &str, server_url: &str, connected_at: Option<i64>) -> Result<Option<(StoredMcpOAuthTokens, bool)>, WireAsyncError> {
        let tokens = self.get_provider(server_name, server_url).tokens().await?;
        let Some(tokens) = tokens else {
            return Ok(None);
        };
        Ok(Some((tokens.clone(), is_concurrent_grant(&tokens, self.scheduler.now(), connected_at))))
    }

    pub fn now(&self) -> i64 {
        self.scheduler.now()
    }

    pub fn forget_provider(&self, server_name: &str, server_url: &str) {
        self.state.lock().providers.remove(&mcp_oauth_store_key(server_name, server_url).unwrap());
    }

    fn create_provider(&self, server_name: &str, server_url: &str, client_label: Option<String>) -> Arc<McpOAuthClientProvider> {
        let canonical_url = canonical_mcp_oauth_resource(server_url).unwrap();
        let this = self.clone_handle();
        let server_name = server_name.to_string();
        Arc::new(McpOAuthClientProvider::new(McpOAuthProviderOptions {
            server_name: server_name.clone(),
            server_url: canonical_url.clone(),
            store: self.store.clone(),
            client_label: client_label.or_else(|| self.client_label.clone()),
            client_name: self.resolve_client_name.as_ref().and_then(|resolve| resolve()),
            now: Some({
                let scheduler = self.scheduler.clone();
                Arc::new(move || scheduler.now())
            }),
            track: Some({
                let this = this.clone();
                Arc::new(move |future| {
                    this.track_background_task(async move {
                        let _ = future.await;
                    }
                    .boxed()
                    .shared())
                })
            }),
            on_tokens_saved: Some({
                let this = this.clone();
                let server_name = server_name.clone();
                let canonical_url = canonical_url.clone();
                Arc::new(move |tokens| {
                    this.emit(McpOAuthEvent {
                        type_: McpOAuthEventType::TokensSaved,
                        server_name: server_name.clone(),
                        server_url: canonical_url.clone(),
                        scope: None,
                        error: None,
                    });
                    if let (Some(obtained), Some(expires), Some(refresh)) = (tokens.obtained_at, tokens.expires_in, tokens.refresh_token.clone()) {
                        if !refresh.is_empty() {
                            this.schedule_refresh(&server_name, &canonical_url, obtained + expires * 1000);
                        }
                    }
                })
            }),
            on_credentials_invalidated: Some({
                let this = this.clone();
                let server_name = server_name.clone();
                let canonical_url = canonical_url.clone();
                Arc::new(move |scope| {
                    if matches!(scope, OAuthInvalidationScope::Tokens | OAuthInvalidationScope::All) {
                        this.cancel_scheduled_refresh(&server_name, &canonical_url);
                    }
                    this.emit(McpOAuthEvent {
                        type_: McpOAuthEventType::TokensInvalidated,
                        server_name: server_name.clone(),
                        server_url: canonical_url.clone(),
                        scope: Some(scope),
                        error: None,
                    });
                })
            }),
        }))
    }

    async fn refresh_now(&self, server_name: &str, server_url: &str) -> Result<(), WireAsyncError> {
        let store_key = mcp_oauth_store_key(server_name, server_url).unwrap();
        if self.state.lock().active_authorizations.contains_key(&store_key) {
            return Ok(());
        }
        let state = self.token_state(server_name, server_url).await?;
        if self.state.lock().active_authorizations.contains_key(&store_key) {
            return Ok(());
        }
        if !state.has_tokens || !state.has_refresh_token {
            return Err(Box::new(Error2::new(
                MCP_OAUTH_FAILED,
                format!("MCP server \"{server_name}\" has no refreshable OAuth grant"),
            )));
        }
        let provider = self.get_provider(server_name, server_url);
        provider.reset_flow();
        let result = oauth_authorize(
            provider.clone(),
            server_url,
            OAuthAuthorizeOptions {
                scope: None,
                authorization_code: None,
                fetch: self.auth_fetch(provider.clone(), Vec::new()),
            },
        )
        .await;
        provider.reset_flow();
        if result? != OAuthAuthorizeOutcome::Authorized {
            return Err(Box::new(Error2::new(
                MCP_OAUTH_FAILED,
                "the stored OAuth grant requires an interactive login",
            )));
        }
        Ok(())
    }

    fn schedule_refresh(&self, server_name: &str, server_url: &str, expires_at: i64) {
        if self.state.lock().shutting_down {
            return;
        }
        let canonical_url = canonical_mcp_oauth_resource(server_url).unwrap();
        let store_key = mcp_oauth_store_key(server_name, &canonical_url).unwrap();
        self.cancel_scheduled_refresh(server_name, &canonical_url);
        let now = self.scheduler.now();
        if expires_at <= now {
            return;
        }
        let lifetime_ms = expires_at - now;
        let refresh_ahead_ms = REFRESH_AHEAD_MS.min(lifetime_ms / 2);
        let delay = lifetime_ms - refresh_ahead_ms;
        let timer_key = store_key.clone();
        let timer_key_for_delay = timer_key.clone();
        let this = self.clone_handle();
        let server_name = server_name.to_string();
        let canonical_url = canonical_url.clone();
        let timer = if delay > MAX_TIMER_DELAY_MS {
            self.scheduler.schedule(MAX_TIMER_DELAY_MS, Arc::new(move || {
                this.state.lock().refresh_timers.remove(&timer_key_for_delay);
                this.schedule_refresh(&server_name, &canonical_url, expires_at);
                async {}.boxed()
            }))
        } else {
            let this = self.clone_handle();
            let server_name = server_name.clone();
            let canonical_url = canonical_url.clone();
            self.scheduler.schedule(delay, Arc::new(move || {
                let this = this.clone();
                let server_name = server_name.clone();
                let canonical_url = canonical_url.clone();
                let timer_key = timer_key.clone();
                async move {
                    this.state.lock().refresh_timers.remove(&timer_key);
                    if let Err(error) = this.refresh(&server_name, &canonical_url).await {
                        this.emit(McpOAuthEvent {
                            type_: McpOAuthEventType::RefreshFailed,
                            server_name: server_name.clone(),
                            server_url: canonical_url.clone(),
                            scope: None,
                            error: Some(error.to_string()),
                        });
                    }
                }
                .boxed()
            }))
        };
        self.state.lock().refresh_timers.insert(store_key, timer);
    }

    fn cancel_scheduled_refresh(&self, server_name: &str, server_url: &str) {
        let store_key = mcp_oauth_store_key(server_name, server_url).unwrap();
        if let Some(timer) = self.state.lock().refresh_timers.remove(&store_key) {
            timer.cancel();
        }
    }

    fn emit(&self, event: McpOAuthEvent) {
        if matches!(event.type_, McpOAuthEventType::TokensSaved | McpOAuthEventType::TokensInvalidated)
            && (event.type_ == McpOAuthEventType::TokensSaved
                || event.scope.is_some_and(|scope| matches!(scope, OAuthInvalidationScope::Tokens | OAuthInvalidationScope::All)))
        {
            if let Some(coordinator) = &self.coordinator {
                let _ = match event.type_ {
                    McpOAuthEventType::TokensSaved => coordinator.notify_credentials_changed(&event.server_name, &event.server_url),
                    _ => coordinator.notify_credentials_invalidated(&event.server_name, &event.server_url),
                };
            }
        }
        let mut visited = HashSet::new();
        loop {
            let next = {
                let state = self.state.lock();
                state
                    .listeners
                    .iter()
                    .find(|(id, _)| !visited.contains(*id))
                    .map(|(id, listener)| (*id, listener.clone()))
            };
            let Some((id, listener)) = next else {
                break;
            };
            visited.insert(id);
            let event = event.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)));
        }
    }

    fn clone_handle(&self) -> Self {
        Self {
            store: self.store.clone(),
            client_label: self.client_label.clone(),
            resolve_client_name: self.resolve_client_name.clone(),
            coordinator: self.coordinator.clone(),
            log: self.log.clone(),
            scheduler: self.scheduler.clone(),
            auth_request_timeout_ms: self.auth_request_timeout_ms,
            shutdown_drain_timeout_ms: self.shutdown_drain_timeout_ms,
            state: self.state.clone(),
        }
    }
}

#[derive(Debug)]
pub struct AlreadyAuthorizedError(Error2);

impl AlreadyAuthorizedError {
    pub fn new(server_name: &str) -> Self {
        Self(Error2::new(
            MCP_OAUTH_FAILED,
            format!("\"{server_name}\" is already authorized; no browser flow needed"),
        ).with_name("AlreadyAuthorizedError"))
    }
}

impl fmt::Display for AlreadyAuthorizedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for AlreadyAuthorizedError {}

#[derive(Debug, Clone, Copy, PartialEq)]
enum OAuthAuthorizeOutcome {
    Redirect,
    Authorized,
}

impl OAuthAuthorizeOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Redirect => "REDIRECT",
            Self::Authorized => "AUTHORIZED",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OAuthEndpointErrorKind {
    InvalidClient,
    InvalidGrant,
    UnauthorizedClient,
    Server,
    Other,
}

#[derive(Debug)]
struct OAuthEndpointError {
    kind: OAuthEndpointErrorKind,
    message: String,
}

impl fmt::Display for OAuthEndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for OAuthEndpointError {}

fn oauth_endpoint_error_kind(error: &(dyn Error + 'static)) -> Option<OAuthEndpointErrorKind> {
    error.downcast_ref::<OAuthEndpointError>().map(|error| error.kind)
}

fn oauth_issuer(discovery: &Value, server_url: &str) -> Result<String, WireAsyncError> {
    if let Some(issuer) = discovery.get("authorizationServerUrl").and_then(Value::as_str) {
        return Ok(issuer.to_string());
    }
    Ok(url::Url::parse(server_url)?.join("/")?.to_string())
}

fn resolved_oauth_scope(
    provider: &McpOAuthClientProvider,
    discovery: &Value,
    requested: Option<String>,
) -> Option<String> {
    if let Some(scope) = requested.filter(|scope| !scope.is_empty()) {
        return Some(scope);
    }
    let resource_scope = discovery
        .get("resourceMetadata")
        .and_then(|metadata| metadata.get("scopes_supported"))
        .and_then(Value::as_array)
        .map(|scopes| scopes.iter().filter_map(|scope| scope.as_str()).collect::<Vec<_>>().join(" "))
        .filter(|scope| !scope.is_empty());
    if resource_scope.is_some() {
        return resource_scope;
    }
    provider.client_metadata().scope.filter(|scope| !scope.is_empty())
}

fn oauth_resource(discovery: &Value, server_url: &str) -> Result<Option<String>, WireAsyncError> {
    let Some(metadata) = discovery.get("resourceMetadata").filter(|metadata| !metadata.is_null()) else {
        return Ok(None);
    };
    let configured = metadata
        .get("resource")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid URL".to_string())?;
    let mut requested = url::Url::parse(server_url)?;
    requested.set_fragment(None);
    let configured_url = url::Url::parse(configured)?;
    let same_origin = requested.scheme() == configured_url.scheme()
        && requested.host_str() == configured_url.host_str()
        && requested.port_or_known_default() == configured_url.port_or_known_default();
    let requested_path = if requested.path().ends_with('/') {
        requested.path().to_string()
    } else {
        format!("{}/", requested.path())
    };
    let configured_path = if configured_url.path().ends_with('/') {
        configured_url.path().to_string()
    } else {
        format!("{}/", configured_url.path())
    };
    if !same_origin || !requested_path.starts_with(&configured_path) {
        return Err(format!(
            "Protected resource {configured} does not match expected {} (or origin)",
            requested.as_str()
        ).into());
    }
    Ok(Some(configured.to_string()))
}

trait OAuthIssuer {
    fn issuer(&self) -> Option<&str>;
}

impl OAuthIssuer for Value {
    fn issuer(&self) -> Option<&str> {
        self.get("issuer").and_then(Value::as_str)
    }
}

impl OAuthIssuer for OAuthTokens {
    fn issuer(&self) -> Option<&str> {
        self.extra.get("issuer").and_then(Value::as_str)
    }
}

fn discard_if_issuer_mismatch<T: OAuthIssuer>(stored: T, issuer: &str) -> Option<T> {
    match stored.issuer() {
        Some(stored_issuer) if !issuers_match(stored_issuer, issuer) => None,
        _ => Some(stored),
    }
}

fn issuers_match(a: &str, b: &str) -> bool {
    let normalize = |value: &str| {
        url::Url::parse(value)
            .map(|url| url.to_string())
            .unwrap_or_else(|_| value.to_string())
    };
    let x = normalize(a);
    let y = normalize(b);
    x == y
        || (x.ends_with('/') && x[..x.len() - 1] == y)
        || (y.ends_with('/') && y[..y.len() - 1] == x)
}

fn pkce_verifier() -> String {
    const MASK: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-._~";
    const CUTOFF: u16 = 256 - (256 % MASK.len() as u16);
    let mut verifier = String::with_capacity(43);
    while verifier.len() < 43 {
        let mut bytes = [0u8; 43];
        getrandom::fill(&mut bytes).unwrap();
        for byte in bytes {
            if u16::from(byte) < CUTOFF {
                verifier.push(MASK[usize::from(byte) % MASK.len()] as char);
                if verifier.len() == 43 {
                    break;
                }
            }
        }
    }
    verifier
}

fn set_url_query_param(url: &mut url::Url, key: &str, value: &str) {
    let mut found = false;
    let pairs = url
        .query_pairs()
        .filter_map(|(current_key, current_value)| {
            if current_key == key {
                if found {
                    return None;
                }
                found = true;
                return Some((key.to_string(), value.to_string()));
            }
            Some((current_key.into_owned(), current_value.into_owned()))
        })
        .collect::<Vec<_>>();
    url.query_pairs_mut().clear().extend_pairs(pairs);
    if !found {
        url.query_pairs_mut().append_pair(key, value);
    }
}

fn set_form_param(params: &mut Vec<(String, String)>, key: &str, value: &str) {
    let mut found = false;
    for (current_key, current_value) in std::mem::take(params) {
        if current_key == key {
            if found {
                continue;
            }
            found = true;
            params.push((key.to_string(), value.to_string()));
        } else {
            params.push((current_key, current_value));
        }
    }
    if !found {
        params.push((key.to_string(), value.to_string()));
    }
}

#[derive(Clone)]
struct OAuthAuthorizeOptions {
    scope: Option<String>,
    authorization_code: Option<String>,
    fetch: Arc<dyn FetchLike>,
}

async fn oauth_authorize(
    provider: Arc<McpOAuthClientProvider>,
    server_url: &str,
    options: OAuthAuthorizeOptions,
) -> Result<OAuthAuthorizeOutcome, WireAsyncError> {
    let mut retried = false;
    loop {
        let result = oauth_authorize_internal(provider.clone(), server_url, options.clone()).await;
        let Err(error) = result else {
            return result;
        };
        if retried {
            return Err(error);
        }
        let Some(kind) = oauth_endpoint_error_kind(error.as_ref()) else {
            return Err(error);
        };
        let scope = match kind {
            OAuthEndpointErrorKind::InvalidClient | OAuthEndpointErrorKind::UnauthorizedClient => OAuthInvalidationScope::All,
            OAuthEndpointErrorKind::InvalidGrant => OAuthInvalidationScope::Tokens,
            _ => return Err(error),
        };
        provider.invalidate_credentials(scope).await?;
        retried = true;
    }
}

async fn oauth_authorize_internal(
    provider: Arc<McpOAuthClientProvider>,
    server_url: &str,
    options: OAuthAuthorizeOptions,
) -> Result<OAuthAuthorizeOutcome, WireAsyncError> {
    let discovery = ensure_oauth_discovery(provider.clone(), server_url, options.fetch.clone()).await?;
    let issuer = oauth_issuer(&discovery, server_url)?;
    let resource = oauth_resource(&discovery, server_url)?;
    if let Some(code) = options.authorization_code {
        let client = provider
            .client_information()
            .await?
            .and_then(|client| discard_if_issuer_mismatch(client, &issuer))
            .ok_or_else(|| "Existing OAuth client information is required when exchanging an authorization code".to_string())?;
        exchange_oauth_code(provider, &discovery, &client, &code, resource.as_deref(), options.fetch).await?;
        return Ok(OAuthAuthorizeOutcome::Authorized);
    }
    let scope = resolved_oauth_scope(&provider, &discovery, options.scope);
    let client = ensure_oauth_client(provider.clone(), &discovery, scope.clone(), options.fetch.clone()).await?;
    let tokens = provider
        .tokens()
        .await?
        .and_then(|tokens| discard_if_issuer_mismatch(tokens, &issuer));
    if let Some(tokens) = tokens {
        if tokens.refresh_token.as_ref().is_some_and(|token| !token.is_empty()) {
            match refresh_oauth_tokens(provider.clone(), &discovery, &client, &tokens, resource.as_deref(), options.fetch.clone()).await {
                Ok(()) => return Ok(OAuthAuthorizeOutcome::Authorized),
                Err(error) if oauth_endpoint_error_kind(error.as_ref()).is_none_or(|kind| kind == OAuthEndpointErrorKind::Server) => {}
                Err(error) => return Err(error),
            }
        }
    }
    let metadata = discovery.get("authorizationServerMetadata").cloned().unwrap_or_else(|| json!({}));
    if !metadata.is_null() {
        let response_types = metadata
            .get("response_types_supported")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !response_types.iter().any(|value| value.as_str() == Some("code")) {
            return Err("Incompatible auth server: does not support response type code".into());
        }
        if metadata
            .get("code_challenge_methods_supported")
            .and_then(Value::as_array)
            .is_some_and(|methods| !methods.iter().any(|value| value.as_str() == Some("S256")))
        {
            return Err("Incompatible auth server: does not support code challenge method S256".into());
        }
    }
    let endpoint = match metadata.get("authorization_endpoint").and_then(Value::as_str) {
        Some(endpoint) => endpoint.to_string(),
        None => url::Url::parse(&issuer)?.join("/authorize")?.to_string(),
    };
    let verifier = pkce_verifier();
    provider.save_code_verifier(&verifier);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = provider.state();
    let client_id = client
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| Box::new(Error2::new(MCP_OAUTH_FAILED, "OAuth client registration did not return client_id")) as WireAsyncError)?;
    let mut url = url::Url::parse(&endpoint)?;
    set_url_query_param(&mut url, "response_type", "code");
    set_url_query_param(&mut url, "client_id", client_id);
    set_url_query_param(&mut url, "code_challenge", &challenge);
    set_url_query_param(&mut url, "code_challenge_method", "S256");
    set_url_query_param(&mut url, "redirect_uri", &provider.redirect_url());
    set_url_query_param(&mut url, "state", &state);
    if let Some(scope) = scope.filter(|scope| !scope.is_empty()) {
        set_url_query_param(&mut url, "scope", &scope);
        if scope.contains(OFFLINE_ACCESS_SCOPE) {
            url.query_pairs_mut().append_pair("prompt", "consent");
        }
    }
    if let Some(resource) = resource {
        set_url_query_param(&mut url, "resource", &resource);
    }
    provider.redirect_to_authorization(url.as_str());
    Ok(OAuthAuthorizeOutcome::Redirect)
}

async fn ensure_oauth_discovery(
    provider: Arc<McpOAuthClientProvider>,
    server_url: &str,
    fetch: Arc<dyn FetchLike>,
) -> Result<Value, WireAsyncError> {
    if let Some(discovery) = provider.discovery_state().await? {
        if discovery.get("authorizationServerMetadata").is_some_and(|metadata| !metadata.is_null()) {
            return Ok(discovery);
        }
        if let Some(issuer) = discovery.get("authorizationServerUrl").and_then(Value::as_str) {
            let metadata = discover_authorization_server_metadata(issuer, fetch.clone()).await?;
            let mut next = discovery.clone();
            let mut changed = false;
            if let Some(metadata) = metadata {
                next["authorizationServerMetadata"] = metadata;
                changed = true;
            }
            if next.get("resourceMetadata").is_none_or(Value::is_null) {
                let resource = discover_oauth_resource_metadata(&url::Url::parse(server_url)?, fetch).await;
                if let Some(resource) = resource {
                    next["resourceMetadata"] = resource;
                    changed = true;
                }
            }
            if changed {
                provider.save_discovery_state(next.clone()).await?;
            }
            return Ok(next);
        }
    }
    let info = discover_oauth_server_info(server_url, fetch).await?;
    provider.save_discovery_state(info.clone()).await?;
    Ok(info)
}

async fn discover_oauth_server_info(server_url: &str, fetch: Arc<dyn FetchLike>) -> Result<Value, WireAsyncError> {
    let base = url::Url::parse(server_url)?;
    let resource = discover_oauth_resource_metadata(&base, fetch.clone()).await;
    let authorization_server = resource
        .as_ref()
        .and_then(|resource| resource.get("authorization_servers"))
        .and_then(Value::as_array)
        .and_then(|servers| servers.first())
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or(base.join("/")?.to_string());
    let metadata = discover_authorization_server_metadata(&authorization_server, fetch).await?;
    let mut result = Map::new();
    result.insert("authorizationServerUrl".to_string(), authorization_server.into());
    if let Some(resource) = resource {
        result.insert("resourceMetadata".to_string(), resource);
    }
    if let Some(metadata) = metadata {
        result.insert("authorizationServerMetadata".to_string(), metadata);
    }
    Ok(Value::Object(result))
}

async fn discover_oauth_resource_metadata(base: &url::Url, fetch: Arc<dyn FetchLike>) -> Option<Value> {
    let pathname = base.path().trim_end_matches('/');
    let mut candidates = vec![format!("/.well-known/oauth-protected-resource{pathname}")];
    if base.path() != "/" {
        candidates.push("/.well-known/oauth-protected-resource".to_string());
    }
    for (index, candidate) in candidates.into_iter().enumerate() {
        let mut url = base.join(&candidate).ok()?;
        if index == 0 {
            url.set_query(base.query());
        }
        let Ok((status, _, body)) = fetch_discovery_response(fetch.clone(), &url, false).await else {
            continue;
        };
        if !(200..300).contains(&status) {
            if status < 500 {
                continue;
            }
            return None;
        }
        let value = serde_json::from_str::<Value>(&body).ok()?;
        if !value.get("resource").and_then(Value::as_str).is_some_and(safe_oauth_url) {
            return None;
        }
        if value
            .get("authorization_servers")
            .and_then(Value::as_array)
            .is_some_and(|servers| servers.iter().any(|server| !server.as_str().is_some_and(safe_oauth_url)))
        {
            return None;
        }
        return Some(value);
    }
    None
}

async fn discover_authorization_server_metadata(issuer: &str, fetch: Arc<dyn FetchLike>) -> Result<Option<Value>, WireAsyncError> {
    let issuer = url::Url::parse(issuer)?;
    let pathname = issuer.path().trim_end_matches('/');
    let has_path = issuer.path() != "/";
    let mut candidates = vec![
        (format!("/.well-known/oauth-authorization-server{pathname}"), "OAuth"),
        (format!("/.well-known/openid-configuration{pathname}"), "OpenID provider"),
    ];
    if has_path {
        candidates.push((format!("{pathname}/.well-known/openid-configuration"), "OpenID provider"));
    }
    for (candidate, kind) in candidates {
        let url = issuer.join(&candidate)?;
        let (status, _, body) = match fetch_discovery_response(fetch.clone(), &url, true).await {
            Ok(response) => response,
            Err(_) => continue,
        };
        if ! (200..300).contains(&status) {
            if status < 500 {
                continue;
            }
            return Err(format!("HTTP {status} trying to load {kind} metadata from {}", url.as_str()).into());
        }
        let value = serde_json::from_str::<Value>(&body)?;
        validate_authorization_metadata(&value)?;
        return Ok(Some(value));
    }
    Ok(None)
}

async fn fetch_discovery_response(
    fetch: Arc<dyn FetchLike>,
    url: &url::Url,
    accept_json: bool,
) -> Result<(u16, String, String), WireAsyncError> {
    let mut headers = vec![("MCP-Protocol-Version".to_string(), "2025-11-25".to_string())];
    if accept_json {
        headers.push(("Accept".to_string(), "application/json".to_string()));
    }
    let response = fetch
        .fetch(
            url.as_str(),
            FetchInit {
                headers,
                method: Some("GET".to_string()),
                body: None,
                signal: None,
                manual_redirect: false,
                timeout_ms: None,
            },
        )
        .await?;
    let status = response.status;
    let status_text = response.status_text.clone();
    let body = response.text().await?;
    Ok((status, status_text, body))
}

fn validate_authorization_metadata(value: &Value) -> Result<(), WireAsyncError> {
    for key in ["authorization_endpoint", "token_endpoint"] {
        if !value.get(key).and_then(Value::as_str).is_some_and(safe_oauth_url) {
            return Err(format!("Invalid OAuth metadata field: {key}").into());
        }
    }
    if !value.get("issuer").is_some_and(Value::is_string)
        || !value.get("response_types_supported").is_some_and(Value::is_array)
    {
        return Err("Invalid OAuth metadata".into());
    }
    Ok(())
}

fn safe_oauth_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| !matches!(url.scheme(), "javascript" | "data" | "vbscript"))
}

async fn ensure_oauth_client(
    provider: Arc<McpOAuthClientProvider>,
    discovery: &Value,
    scope: Option<String>,
    fetch: Arc<dyn FetchLike>,
) -> Result<Value, WireAsyncError> {
    let issuer = discovery
        .get("authorizationServerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid URL".to_string())?
        .to_string();
    if let Some(client) = provider
        .client_information()
        .await?
        .and_then(|client| discard_if_issuer_mismatch(client, &issuer))
    {
        return Ok(client);
    }
    let metadata_value = discovery.get("authorizationServerMetadata").filter(|metadata| !metadata.is_null());
    let endpoint = match metadata_value {
        Some(metadata) => metadata
            .get("registration_endpoint")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| "Incompatible auth server: does not support dynamic client registration".to_string())?,
        None => url::Url::parse(&issuer)?.join("/register")?.to_string(),
    };
    let mut metadata = serde_json::to_value(provider.client_metadata())?;
    if let Some(scope) = scope {
        metadata["scope"] = scope.into();
    }
    let mut client = parse_client_information(fetch_json(fetch, &endpoint, Some(metadata)).await?)?;
    client["issuer"] = issuer.into();
    provider.save_client_information(client.clone()).await?;
    Ok(client)
}

async fn exchange_oauth_code(
    provider: Arc<McpOAuthClientProvider>,
    discovery: &Value,
    client: &Value,
    code: &str,
    resource: Option<&str>,
    fetch: Arc<dyn FetchLike>,
) -> Result<(), WireAsyncError> {
    let params = vec![
        ("grant_type".to_string(), "authorization_code".to_string()),
        ("code".to_string(), code.to_string()),
        ("code_verifier".to_string(), provider.code_verifier()?),
        ("redirect_uri".to_string(), provider.redirect_url()),
    ];
    let tokens = execute_oauth_token_request(discovery, client, params, resource, fetch).await?;
    save_oauth_tokens(provider, client, discovery, tokens).await
}

async fn refresh_oauth_tokens(
    provider: Arc<McpOAuthClientProvider>,
    discovery: &Value,
    client: &Value,
    tokens: &OAuthTokens,
    resource: Option<&str>,
    fetch: Arc<dyn FetchLike>,
) -> Result<(), WireAsyncError> {
    let refresh_token = tokens.refresh_token.clone().unwrap();
    let params = vec![
        ("grant_type".to_string(), "refresh_token".to_string()),
        ("refresh_token".to_string(), refresh_token.clone()),
    ];
    let mut refreshed = execute_oauth_token_request(discovery, client, params, resource, fetch).await?;
    if refreshed.get("refresh_token").is_none() {
        refreshed["refresh_token"] = refresh_token.into();
    }
    save_oauth_tokens(provider, client, discovery, refreshed).await
}

fn token_endpoint(discovery: &Value) -> Result<String, WireAsyncError> {
    if let Some(endpoint) = discovery
        .get("authorizationServerMetadata")
        .and_then(|metadata| metadata.get("token_endpoint"))
        .and_then(Value::as_str)
    {
        return Ok(url::Url::parse(endpoint)?.to_string());
    }
    let issuer = discovery
        .get("authorizationServerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid URL".to_string())?;
    Ok(url::Url::parse(issuer)?.join("/token")?.to_string())
}

async fn execute_oauth_token_request(
    discovery: &Value,
    client: &Value,
    mut params: Vec<(String, String)>,
    resource: Option<&str>,
    fetch: Arc<dyn FetchLike>,
) -> Result<Value, WireAsyncError> {
    let endpoint = token_endpoint(discovery)?;
    if let Some(resource) = resource {
        set_form_param(&mut params, "resource", resource);
    }
    let supported = discovery
        .get("authorizationServerMetadata")
        .and_then(|metadata| metadata.get("token_endpoint_auth_methods_supported"))
        .and_then(Value::as_array)
        .map(|methods| methods.iter().filter_map(|method| method.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let client_id = client.get("client_id").and_then(Value::as_str).unwrap_or("");
    let client_secret = client.get("client_secret").and_then(Value::as_str);
    let registered_method = client.get("token_endpoint_auth_method").and_then(Value::as_str);
    let method = if registered_method.is_some_and(|method| {
        matches!(method, "client_secret_basic" | "client_secret_post" | "none")
            && (supported.is_empty() || supported.contains(&method))
    }) {
        registered_method.unwrap()
    } else if supported.is_empty() {
        if client_secret.is_some() { "client_secret_basic" } else { "none" }
    } else if client_secret.is_some() && supported.contains(&"client_secret_basic") {
        "client_secret_basic"
    } else if client_secret.is_some() && supported.contains(&"client_secret_post") {
        "client_secret_post"
    } else if supported.contains(&"none") {
        "none"
    } else if client_secret.is_some() {
        "client_secret_post"
    } else {
        "none"
    };
    let mut headers = vec![
        ("content-type".to_string(), "application/x-www-form-urlencoded".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    match method {
        "client_secret_basic" => {
            let Some(secret) = client_secret.filter(|secret| !secret.is_empty()) else {
                return Err("client_secret_basic authentication requires a client_secret".into());
            };
            let credentials = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{secret}"));
            headers.push(("authorization".to_string(), format!("Basic {credentials}")));
        }
        "client_secret_post" => {
            set_form_param(&mut params, "client_id", client_id);
            if let Some(secret) = client_secret.filter(|secret| !secret.is_empty()) {
                set_form_param(&mut params, "client_secret", secret);
            }
        }
        "none" => set_form_param(&mut params, "client_id", client_id),
        _ => return Err(format!("Unsupported client authentication method: {method}").into()),
    }
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(params)
        .finish();
    let response = fetch
        .fetch(
            &endpoint,
            FetchInit {
                headers,
                method: Some("POST".to_string()),
                body: Some(body),
                signal: None,
                manual_redirect: false,
                timeout_ms: None,
            },
        )
        .await?;
    let status = response.status;
    let ok = response.ok;
    let text = response.text().await?;
    if !ok {
        return Err(parse_oauth_error_response(status, &text));
    }
    serde_json::from_str(&text).map_err(|error| error.into())
}

fn parse_oauth_error_response(status: u16, body: &str) -> WireAsyncError {
    match serde_json::from_str::<Value>(body) {
        Ok(value) => {
            let code = value.get("error").and_then(Value::as_str);
            let message = value
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let kind = match code {
                Some("invalid_client") => OAuthEndpointErrorKind::InvalidClient,
                Some("invalid_grant") => OAuthEndpointErrorKind::InvalidGrant,
                Some("unauthorized_client") => OAuthEndpointErrorKind::UnauthorizedClient,
                Some("invalid_request" | "unsupported_grant_type" | "invalid_scope" | "access_denied" | "temporarily_unavailable" | "unsupported_response_type" | "unsupported_token_type" | "invalid_token" | "method_not_allowed" | "too_many_requests" | "invalid_client_metadata" | "insufficient_scope" | "invalid_target") => OAuthEndpointErrorKind::Other,
                _ => OAuthEndpointErrorKind::Server,
            };
            Box::new(OAuthEndpointError { kind, message })
        }
        Err(error) => Box::new(OAuthEndpointError {
            kind: OAuthEndpointErrorKind::Server,
            message: format!("HTTP {status}: Invalid OAuth error response: {error}. Raw body: {body}"),
        }),
    }
}

async fn save_oauth_tokens(
    provider: Arc<McpOAuthClientProvider>,
    client: &Value,
    discovery: &Value,
    mut value: Value,
) -> Result<(), WireAsyncError> {
    let issuer = discovery
        .get("authorizationServerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid URL".to_string())?
        .to_string();
    if client.get("issuer").and_then(Value::as_str).is_none() {
        let mut bound = client.clone();
        bound["issuer"] = issuer.clone().into();
        let _ = provider.save_client_information(bound).await;
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("issuer");
        if let Some(expires) = object.get("expires_in") {
            let normalized = match expires {
                Value::String(value) => value.parse::<f64>().ok().filter(|value| value.fract() == 0.0).map(|value| value as i64),
                Value::Number(value) => value.as_f64().filter(|value| value.fract() == 0.0).map(|value| value as i64),
                _ => None,
            };
            if let Some(expires) = normalized {
                object.insert("expires_in".to_string(), expires.into());
            }
        }
        object.insert("issuer".to_string(), issuer.into());
    }
    let mut tokens = parse_oauth_tokens(value)?;
    tokens.obtained_at = None;
    provider.save_tokens(tokens).await
}

fn parse_oauth_tokens(value: Value) -> Result<OAuthTokens, WireAsyncError> {
    if ["expires_in", "refresh_token", "scope", "id_token"]
        .into_iter()
        .any(|key| value.get(key).is_some_and(Value::is_null))
    {
        return Err("OAuth token response contains null".into());
    }
    let mut tokens: OAuthTokens = serde_json::from_value(value)?;
    tokens.obtained_at = None;
    tokens.extra.retain(|key, _| matches!(key.as_str(), "id_token" | "issuer"));
    Ok(tokens)
}

fn parse_client_information(value: Value) -> Result<Value, WireAsyncError> {
    let mut object = value.as_object().cloned().ok_or_else(|| "client information must be an object".to_string())?;
    object.retain(|key, _| matches!(key.as_str(), "redirect_uris" | "token_endpoint_auth_method" | "grant_types" | "response_types" | "client_name" | "client_uri" | "logo_uri" | "scope" | "contacts" | "tos_uri" | "policy_uri" | "jwks_uri" | "jwks" | "software_id" | "software_version" | "software_statement" | "client_id" | "client_secret" | "client_id_issued_at" | "client_secret_expires_at" | "issuer"));
    if !object.get("client_id").is_some_and(Value::is_string) {
        return Err("client_id is required".into());
    }
    if !object.get("redirect_uris").is_some_and(Value::is_array) {
        return Err("redirect_uris is required".into());
    }
    Ok(Value::Object(object))
}

async fn fetch_json(fetch: Arc<dyn FetchLike>, url: &str, body: Option<Value>) -> Result<Value, WireAsyncError> {
    let response = fetch
        .fetch(
            url,
            FetchInit {
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                method: Some(if body.is_some() { "POST" } else { "GET" }.to_string()),
                body: body.map(|body| serde_json::to_string(&body).unwrap()),
                signal: None,
                manual_redirect: false,
                timeout_ms: None,
            },
        )
        .await?;
    let status = response.status;
    let ok = response.ok;
    let text = response.text().await?;
    if !ok {
        return Err(parse_oauth_error_response(status, &text));
    }
    serde_json::from_str(&text).map_err(|error| error.into())
}

struct TimeoutFetch {
    inner: Arc<dyn FetchLike>,
    timeout: u64,
    signals: Vec<AbortSignal>,
}

#[async_trait::async_trait]
impl FetchLike for TimeoutFetch {
    async fn fetch(&self, url: &str, mut init: FetchInit) -> Result<crate::_base::utils::fetch_like::FetchResponse, crate::_base::utils::fetch_like::FetchError> {
        let controller = AbortController::new();
        let timer = tokio::spawn({
            let controller = controller.clone();
            let timeout = self.timeout;
            async move {
                tokio::time::sleep(std::time::Duration::from_millis(timeout)).await;
                controller.abort(None);
            }
        });
        let mut signals = self.signals.clone();
        signals.push(controller.signal());
        if let Some(token) = init.signal.take() {
            signals.push(AbortSignal::from_cancellation_token(token));
        }
        init.signal = Some(AbortSignal::any(&signals).cancellation_token());
        let result = self.inner.fetch(url, init).await;
        timer.abort();
        result
    }
}

struct TimeoutDeadline {
    signal: AbortSignal,
    clear: Arc<dyn Fn() + Send + Sync>,
    timed_out: Arc<std::sync::atomic::AtomicBool>,
}

impl TimeoutDeadline {
    fn new(_parent: AbortSignal, timeout_ms: u64) -> Self {
        let controller = AbortController::new();
        let signal = controller.signal();
        let timer = Arc::new(Mutex::new(None::<tokio::task::JoinHandle<()>>));
        let clear_controller = controller.clone();
        let clear_timer = timer.clone();
        let clear = Arc::new(move || {
            if let Some(task) = clear_timer.lock().take() {
                task.abort();
            }
            clear_controller.abort(None);
        });
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timed_out_for_timer = timed_out.clone();
        let clear_for_timer = clear.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)).await;
            timed_out_for_timer.store(true, std::sync::atomic::Ordering::Relaxed);
            clear_for_timer();
        });
        *timer.lock() = Some(task);
        Self {
            signal,
            clear,
            timed_out,
        }
    }
}

async fn read_store_meta(
    store: &dyn McpOAuthStore,
    key: &str,
    log: &dyn ILogService,
) -> Result<Option<McpOAuthStoreMeta>, WireAsyncError> {
    let Some(raw) = store.read(key).await? else {
        return Ok(None);
    };
    let Some(server_name) = raw.get("serverName").and_then(Value::as_str).filter(|value| !value.is_empty()) else {
        log.warn("ignoring malformed MCP OAuth meta file", Some(json!({ "file": key })));
        return Ok(None);
    };
    let Some(server_url) = raw.get("serverUrl").and_then(Value::as_str) else {
        log.warn("ignoring malformed MCP OAuth meta file", Some(json!({ "file": key })));
        return Ok(None);
    };
    if url::Url::parse(server_url).is_err() {
        log.warn(
            "ignoring MCP OAuth meta file with unparseable serverUrl",
            Some(json!({ "file": key, "serverUrl": server_url })),
        );
        return Ok(None);
    }
    Ok(Some(McpOAuthStoreMeta {
        server_name: server_name.to_string(),
        server_url: server_url.to_string(),
    }))
}

fn is_concurrent_grant(tokens: &StoredMcpOAuthTokens, now: i64, connected_at: Option<i64>) -> bool {
    let Some(obtained_at) = tokens.obtained_at else {
        return false;
    };
    let age = now - obtained_at;
    if age < 0 || age >= CONCURRENT_GRANT_GRACE_MS {
        return false;
    }
    connected_at.is_none_or(|connected_at| obtained_at >= connected_at)
}

fn wrap_auth_error(prefix: &str, error: WireAsyncError) -> WireAsyncError {
    if error.downcast_ref::<Error2>().is_some() {
        return error;
    }
    Box::new(Error2::with_options(
        MCP_OAUTH_FAILED,
        format!("{prefix}: {error}"),
        Error2Options {
            cause: Some(error),
            ..Default::default()
        },
    ))
}

mod service_log {
    pub struct NoopLog;

    impl crate::_base::log::log::ILogger for NoopLog {
        fn error(&self, _message: &str, _payload: Option<serde_json::Value>) {}
        fn warn(&self, _message: &str, _payload: Option<serde_json::Value>) {}
        fn info(&self, _message: &str, _payload: Option<serde_json::Value>) {}
        fn debug(&self, _message: &str, _payload: Option<serde_json::Value>) {}
        fn child(
            &self,
            _context: indexmap::IndexMap<String, serde_json::Value>,
        ) -> std::sync::Arc<dyn crate::_base::log::log::ILogger> {
            std::sync::Arc::new(Self)
        }
    }

    impl crate::_base::log::log::ILogService for NoopLog {
        fn level(&self) -> crate::_base::log::log::LogLevel {
            crate::_base::log::log::LogLevel::Off
        }
        fn set_level(&self, _level: crate::_base::log::log::LogLevel) {}
        fn flush(&self) -> futures_util::future::BoxFuture<'static, ()> {
            futures_util::FutureExt::boxed(async {})
        }
    }
}
