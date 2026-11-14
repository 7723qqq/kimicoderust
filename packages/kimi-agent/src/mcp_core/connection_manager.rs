use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::log::log::ILogService;
use crate::_base::utils::abort::{AbortError, AbortSignal, abort_error};
use crate::app::environment::environment::EnvironmentResolver;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::environment::environment_registry::EnvironmentError;
use crate::mcp_core::errors::{
    MCP_SERVER_DISABLED, MCP_SERVER_NOT_FOUND, MCP_STARTUP_FAILED,
};
use crate::mcp_core::oauth::provider::OAuthTokens;
use crate::mcp_core::oauth::service::McpOAuthService;
use crate::os::interface::host_process::{HostProcessError, OS_PROCESS_SPAWN_FAILED};
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::{Error2, NOT_IMPLEMENTED_CODE};
use kimi_core::primitives::errors::domains::config::CONFIG_INVALID;

use super::client_http::{HttpMcpClient, HttpMcpClientOptions};
use super::client_remote::{
    classify_remote_discovery_error, is_remote_mcp_config,
};
use super::client_shared::*;
use super::client_sse::{SseMcpClient, SseMcpClientOptions};
use super::client_stdio::{StdioMcpClient, StdioMcpClientOptions};
use super::config_schema::{McpServerConfig, McpTransport};
use super::schema_compaction::compact_mcp_input_schema;
use super::types::{MCPClient, MCPToolDefinition};

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 5_000;
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 300_000;
const MAX_FAILURE_STREAK: u32 = 31;
pub const AUTO_DEFER_TOOL_THRESHOLD: u64 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerStatus {
    Pending,
    Connected,
    Failed,
    Disabled,
    NeedsAuth,
    Removed,
}

impl McpServerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected => "connected",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
            Self::NeedsAuth => "needs-auth",
            Self::Removed => "removed",
        }
    }
}

#[derive(Clone)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: McpTransport,
    pub status: McpServerStatus,
    pub tool_count: usize,
    pub error: Option<String>,
    pub next_retry_at: Option<i64>,
}

#[derive(Clone, Default)]
pub struct McpDefaults {
    pub startup_timeout_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,
    pub default_deferred: Option<bool>,
    pub auto_defer_tool_threshold: Option<u64>,
    pub compact_tool_schemas: Option<bool>,
    pub reconnect_initial_delay_ms: Option<u64>,
    pub reconnect_max_delay_ms: Option<u64>,
}

pub type McpStatusListener = Arc<dyn Fn(McpServerEntry) + Send + Sync>;

pub struct McpResolved {
    pub client: Arc<dyn MCPClient>,
    pub tools: Vec<super::super::human::llm::message::ToolDescription>,
    pub raw_tools: Vec<MCPToolDefinition>,
    pub enabled_names: HashSet<String>,
    pub deferred: bool,
}

#[async_trait]
pub trait McpConnectionView: Send + Sync {
    fn oauth_service(&self) -> Option<Arc<McpOAuthService>>;
    fn list(&self) -> Vec<McpServerEntry>;
    fn get(&self, name: &str) -> Option<McpServerEntry>;
    fn deferred_of(&self, name: &str) -> bool;
    fn resolved(&self, name: &str) -> Option<McpResolved>;
    fn get_remote_server_url(&self, name: &str) -> Option<String>;
    async fn mark_needs_auth(
        &self,
        name: &str,
        error: WireAsyncError,
        client: Option<Arc<dyn MCPClient>>,
    ) -> Result<bool, WireAsyncError>;
    async fn reconnect(&self, name: &str) -> Result<(), WireAsyncError>;
    async fn reconnect_and_join(&self, name: &str) -> Result<(), WireAsyncError>;
    async fn wait_for_initial_load(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError>;
    fn initial_load_duration_ms(&self) -> u64;
    fn on_status_change(&self, listener: McpStatusListener) -> Box<dyn IDisposable>;
}

pub struct McpConnectionManagerOptions {
    pub env_lookup: Option<Arc<dyn Fn(&str) -> Option<String> + Send + Sync>>,
    pub stdio_cwd: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    pub environment_resolver: Option<Arc<dyn EnvironmentResolver>>,
    pub environment_id: Option<String>,
    pub require_stdio_environment_id: bool,
    pub session_id: Option<String>,
    pub reconnect_retry_enabled: bool,
    pub oauth_service: Option<Arc<McpOAuthService>>,
    pub telemetry: Option<Arc<dyn ITelemetryService>>,
    pub log: Option<Arc<dyn ILogService>>,
    pub resolve_defaults: Option<Arc<dyn Fn() -> McpDefaults + Send + Sync>>,
    pub resolve_client_name: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
}

impl Default for McpConnectionManagerOptions {
    fn default() -> Self {
        Self {
            env_lookup: None,
            stdio_cwd: None,
            environment_resolver: None,
            environment_id: None,
            require_stdio_environment_id: false,
            session_id: None,
            reconnect_retry_enabled: false,
            oauth_service: None,
            telemetry: None,
            log: None,
            resolve_defaults: None,
            resolve_client_name: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InternalState {
    Connecting,
    Connected,
    Probing,
    RetryWaiting,
    Failed,
    NeedsAuth,
    Disabled,
    Removed,
}

#[derive(Clone, PartialEq)]
enum RuntimeMcpClient {
    Stdio(Arc<StdioMcpClient>),
    Http(Arc<HttpMcpClient>),
    Sse(Arc<SseMcpClient>),
}

impl RuntimeMcpClient {
    fn as_mcp_client(&self) -> &dyn MCPClient {
        match self {
            Self::Stdio(client) => client.as_ref(),
            Self::Http(client) => client.as_ref(),
            Self::Sse(client) => client.as_ref(),
        }
    }

    fn as_mcp_client_arc(&self) -> Arc<dyn MCPClient> {
        match self {
            Self::Stdio(client) => client.clone(),
            Self::Http(client) => client.clone(),
            Self::Sse(client) => client.clone(),
        }
    }

    fn on_session_evidence(&self, listener: SessionEvidenceListener) {
        match self {
            Self::Stdio(client) => client.on_session_evidence(listener),
            Self::Http(client) => client.on_session_evidence(listener),
            Self::Sse(client) => client.on_session_evidence(listener),
        }
    }

    fn stderr_snapshot(&self) -> Option<String> {
        match self {
            Self::Stdio(client) => Some(client.stderr_snapshot()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct SharedManagerError {
    message: Arc<String>,
    code: Option<String>,
    name: Option<String>,
    details: Option<Map<String, Value>>,
    retryable: Option<bool>,
    status_code: Option<i64>,
}

impl SharedManagerError {
    fn new(error: WireAsyncError) -> Self {
        let error2 = error.downcast_ref::<Error2>();
        Self {
            message: Arc::new(error.to_string()),
            code: error2.map(|error| error.code().as_str().to_string()),
            name: error2.map(|error| error.name().to_string()),
            details: error2.and_then(|error| error.details().cloned()),
            retryable: error2.and_then(|error| error.retryable()),
            status_code: error2.and_then(|error| error.status_code()),
        }
    }

    fn from_message(message: String) -> Self {
        Self {
            message: Arc::new(message),
            code: None,
            name: None,
            details: None,
            retryable: None,
            status_code: None,
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
        Box::new(error)
    }
}

impl fmt::Display for SharedManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message.as_str())
    }
}

impl Error for SharedManagerError {}

type AttemptFuture = Shared<BoxFuture<'static, Result<(), SharedManagerError>>>;
type OperationTail = Shared<BoxFuture<'static, ()>>;
type NeedsAuthFuture = Shared<BoxFuture<'static, Result<bool, SharedManagerError>>>;

struct InternalEntry {
    name: String,
    config: McpServerConfig,
    state: InternalState,
    attempt_id: u64,
    attempt_promise: Option<AttemptFuture>,
    attempt_client: Option<RuntimeMcpClient>,
    deferred: bool,
    failure_streak: u32,
    client: Option<RuntimeMcpClient>,
    tools: Option<Vec<super::super::human::llm::message::ToolDescription>>,
    raw_tools: Option<Vec<MCPToolDefinition>>,
    enabled_names: Option<HashSet<String>>,
    connected_at: Option<i64>,
    error: Option<String>,
    next_retry_at: Option<i64>,
}

struct NeedsAuthTransition {
    id: u64,
    attempt_id: u64,
    promise: Option<NeedsAuthFuture>,
}

pub struct McpConnectionManager {
    options: McpConnectionManagerOptions,
    state: Arc<Mutex<ManagerState>>,
}

struct ManagerState {
    entries: IndexMap<String, Arc<Mutex<InternalEntry>>>,
    listeners: HashMap<u64, McpStatusListener>,
    next_listener_id: u64,
    operation_tails: HashMap<String, AttemptFuture>,
    operation_serials: HashMap<String, u64>,
    needs_auth_transitions: HashMap<usize, NeedsAuthTransition>,
    pending_needs_auth_transitions: HashMap<u64, NeedsAuthFuture>,
    next_needs_auth_transition_id: u64,
    reconcile_timer: Option<tokio::task::JoinHandle<()>>,
    reconcile_timer_at: Option<i64>,
    initial_load: Shared<BoxFuture<'static, ()>>,
    initial_load_attempt_id: u64,
    initial_load_started_at: Option<i64>,
    initial_load_finished_at: Option<i64>,
}

impl McpConnectionManager {
    pub fn new(options: McpConnectionManagerOptions) -> Self {
        Self {
            options,
            state: Arc::new(Mutex::new(ManagerState {
                entries: IndexMap::new(),
                listeners: HashMap::new(),
                next_listener_id: 0,
                operation_tails: HashMap::new(),
                operation_serials: HashMap::new(),
                needs_auth_transitions: HashMap::new(),
                pending_needs_auth_transitions: HashMap::new(),
                next_needs_auth_transition_id: 0,
                reconcile_timer: None,
                reconcile_timer_at: None,
                initial_load: async {}.boxed().shared(),
                initial_load_attempt_id: 0,
                initial_load_started_at: None,
                initial_load_finished_at: None,
            })),
        }
    }

    pub fn oauth_service(&self) -> Option<Arc<McpOAuthService>> {
        self.options.oauth_service.clone()
    }

    pub fn list(&self) -> Vec<McpServerEntry> {
        let entries = self
            .state
            .lock()
            .entries
            .values()
            .cloned()
            .collect::<Vec<_>>();
        entries
            .iter()
            .map(|entry| to_public_entry(&entry.lock()))
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<McpServerEntry> {
        let entry = self.state.lock().entries.get(name).cloned();
        entry.map(|entry| to_public_entry(&entry.lock()))
    }

    pub fn deferred_of(&self, name: &str) -> bool {
        self.state
            .lock()
            .entries
            .get(name)
            .is_some_and(|entry| entry.lock().deferred)
    }

    pub fn resolved(&self, name: &str) -> Option<McpResolved> {
        let entry = self.state.lock().entries.get(name).cloned()?;
        let entry = entry.lock();
        if !matches!(entry.state, InternalState::Connected | InternalState::Probing) {
            return None;
        }
        let client = entry.client.as_ref()?;
        let tools = entry.tools.clone()?;
        let raw_tools = entry.raw_tools.clone()?;
        let enabled_names = entry.enabled_names.clone().unwrap_or_else(|| {
            tools.iter().map(|tool| tool.name.clone()).collect()
        });
        Some(McpResolved {
            client: client.as_mcp_client_arc(),
            tools,
            raw_tools,
            enabled_names,
            deferred: entry.deferred,
        })
    }

    pub fn get_remote_server_url(&self, name: &str) -> Option<String> {
        let entry = self.state.lock().entries.get(name).cloned();
        entry.and_then(|entry| {
            if is_remote_mcp_config(&entry.lock().config) {
                entry.lock().config.url.clone()
            } else {
                None
            }
        })
    }

    pub fn get_http_server_url(&self, name: &str) -> Option<String> {
        self.get_remote_server_url(name)
    }

    pub fn on_status_change(&self, listener: McpStatusListener) -> Box<dyn IDisposable> {
        let id = {
            let mut state = self.state.lock();
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.listeners.insert(id, listener);
            id
        };
        let state = self.state.clone();
        to_disposable(move || {
            state.lock().listeners.remove(&id);
        })
    }

    pub fn connect_all(&self, configs: HashMap<String, McpServerConfig>) -> OperationTail {
        let attempt_id = {
            let mut state = self.state.lock();
            state.initial_load_attempt_id += 1;
            state.initial_load_started_at = Some(now_ms());
            state.initial_load_finished_at = None;
            state.initial_load_attempt_id
        };
        let this = self.clone_handle();
        let tasks = configs
            .into_iter()
            .map(|(name, config)| {
                let entry = this.upsert(name.clone(), config);
                entry.lock().attempt_promise.clone().or_else(|| {
                    this.state.lock().operation_tails.get(&name).cloned()
                })
            })
            .collect::<Vec<_>>();
        let initial_load = async move {
            for operation in tasks {
                if let Some(operation) = operation {
                    let _ = operation.await;
                }
            }
            let mut state = this.state.lock();
            if state.initial_load_attempt_id == attempt_id {
                state.initial_load_finished_at = Some(now_ms());
            }
        }
        .boxed()
        .shared();
        self.state.lock().initial_load = initial_load.clone();
        tokio::spawn(initial_load.clone());
        initial_load
    }

    pub async fn connect(&self, name: &str, config: McpServerConfig) -> Result<(), WireAsyncError> {
        let entry = self.upsert(name.to_string(), config);
        let operation = entry.lock().attempt_promise.clone().or_else(|| {
            self.state.lock().operation_tails.get(name).cloned()
        });
        if let Some(operation) = operation {
            operation.await.map_err(SharedManagerError::boxed)?;
        }
        Ok(())
    }

    pub async fn remove(&self, name: &str) -> Result<bool, WireAsyncError> {
        let Some(entry) = self.state.lock().entries.get(name).cloned() else {
            return Ok(false);
        };
        self.transition(&entry, McpEntryEvent::Disable);
        let closing = self.state.lock().operation_tails.get(name).cloned();
        self.state.lock().entries.remove(name);
        if let Some(closing) = closing {
            closing.await;
        }
        Ok(true)
    }

    pub async fn mark_removed(&self, name: &str) -> Result<bool, WireAsyncError> {
        let Some(entry) = self.state.lock().entries.get(name).cloned() else {
            return Ok(false);
        };
        self.transition(&entry, McpEntryEvent::Remove);
        if let Some(closing) = self.state.lock().operation_tails.get(name).cloned() {
            closing.await;
        }
        Ok(true)
    }

    pub async fn wait_for_initial_load(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError> {
        if let Some(signal) = &signal {
            if signal.aborted() {
                return Err(initial_load_abort_error(signal));
            }
        }
        let initial_load = self.state.lock().initial_load.clone();
        match signal {
            Some(signal) => {
                tokio::select! {
                    _ = initial_load => Ok(()),
                    _ = signal.cancelled() => Err(initial_load_abort_error(&signal)),
                }
            }
            None => {
                initial_load.await;
                Ok(())
            }
        }
    }

    pub fn initial_load_duration_ms(&self) -> u64 {
        let state = self.state.lock();
        let Some(started) = state.initial_load_started_at else {
            return 0;
        };
        let ended = state.initial_load_finished_at.unwrap_or_else(now_ms);
        (ended - started).max(0) as u64
    }

    pub async fn reconnect(&self, name: &str) -> Result<(), WireAsyncError> {
        let Some(entry) = self.state.lock().entries.get(name).cloned() else {
            return Err(Box::new(Error2::new(
                MCP_SERVER_NOT_FOUND,
                format!("Unknown MCP server: {name}"),
            )));
        };
        if entry.lock().state == InternalState::Removed {
            return Err(Box::new(Error2::new(
                MCP_SERVER_NOT_FOUND,
                format!("Unknown MCP server: {name}"),
            )));
        }
        if entry.lock().config.enabled == Some(false) {
            return Err(Box::new(Error2::new(
                MCP_SERVER_DISABLED,
                format!("MCP server is disabled: {name}"),
            )));
        }
        if entry.lock().state != InternalState::Connecting || entry.lock().attempt_promise.is_none() {
            self.transition(&entry, McpEntryEvent::Kick);
        }
        let operation = entry.lock().attempt_promise.clone();
        if let Some(operation) = operation {
            operation.await.map_err(SharedManagerError::boxed)?;
        }
        Ok(())
    }

    pub async fn reconnect_and_join(&self, name: &str) -> Result<(), WireAsyncError> {
        self.reconnect(name).await
    }

    pub async fn reconnect_after_current(&self, name: &str) -> Result<(), WireAsyncError> {
        let operation = self.state.lock().operation_tails.get(name).cloned();
        let entry = self.state.lock().entries.get(name).cloned();
        let transition = entry.as_ref().and_then(|entry| {
            let address = Arc::as_ptr(entry) as usize;
            self.state
                .lock()
                .needs_auth_transitions
                .get(&address)
                .and_then(|transition| {
                    (transition.attempt_id == entry.lock().attempt_id)
                        .then(|| transition.promise.clone())
                        .flatten()
                })
        });
        let attempt = entry.as_ref().and_then(|entry| entry.lock().attempt_promise.clone());
        if let Some(operation) = operation {
            let _ = operation.await;
        }
        if let Some(transition) = transition {
            let _ = transition.await;
        }
        if let Some(attempt) = attempt {
            let _ = attempt.await;
        }
        self.reconnect_and_join(name).await
    }

    pub async fn mark_needs_auth(
        &self,
        name: &str,
        error: WireAsyncError,
        client: Option<Arc<dyn MCPClient>>,
    ) -> Result<bool, WireAsyncError> {
        let Some(entry) = self.state.lock().entries.get(name).cloned() else {
            return Ok(false);
        };
        let state = entry.lock().state;
        if !matches!(state, InternalState::Connected | InternalState::Probing | InternalState::NeedsAuth)
            || !self.should_mark_needs_auth(&entry, error.as_ref())
        {
            return Ok(false);
        }
        let address = Arc::as_ptr(&entry) as usize;
        let (redrive, existing) = {
            let state = self.state.lock();
            let existing = state.needs_auth_transitions.get(&address);
            (
                existing.is_some_and(|existing| existing.attempt_id != entry.lock().attempt_id),
                existing.and_then(|existing| existing.promise.clone()),
            )
        };
        if !redrive {
            if let Some(existing) = existing {
                return existing.await.map_err(SharedManagerError::boxed);
            }
            if state == InternalState::NeedsAuth {
                return Ok(true);
            }
        }
        if state != InternalState::NeedsAuth
            && client.is_some()
            && entry.lock().client.as_ref().map(|client| client.as_mcp_client_arc()) != client.as_ref()
        {
            return Ok(false);
        }
        let transition_id = {
            let mut state = self.state.lock();
            state.next_needs_auth_transition_id += 1;
            state.next_needs_auth_transition_id
        };
        let transition = NeedsAuthTransition {
            id: transition_id,
            attempt_id: entry.lock().attempt_id,
            promise: None,
        };
        let this = self.clone_handle();
        let work = async move {
            let result = this
                .perform_mark_needs_auth(name, error, client, redrive, transition_id)
                .await;
            let mut state = this.state.lock();
            if state
                .needs_auth_transitions
                .get(&address)
                .is_some_and(|transition| transition.id == transition_id)
            {
                state.needs_auth_transitions.remove(&address);
            }
            state.pending_needs_auth_transitions.remove(&transition_id);
            result.map_err(SharedManagerError::new)
        }
        .boxed()
        .shared();
        {
            let mut state = self.state.lock();
            state.needs_auth_transitions.insert(address, transition);
            state.pending_needs_auth_transitions.insert(transition_id, work.clone());
            state.needs_auth_transitions.get_mut(&address).unwrap().promise = Some(work.clone());
        }
        tokio::spawn(work.clone());
        work.await.map_err(SharedManagerError::boxed)
    }

    async fn perform_mark_needs_auth(
        &self,
        name: String,
        error: WireAsyncError,
        client: Option<Arc<dyn MCPClient>>,
        redrive: bool,
        transition_id: u64,
    ) -> Result<bool, WireAsyncError> {
        let Some(entry) = self.state.lock().entries.get(&name).cloned() else {
            return Ok(false);
        };
        let state = entry.lock().state;
        if !matches!(state, InternalState::Connected | InternalState::Probing | InternalState::NeedsAuth)
            || !self.should_mark_needs_auth(&entry, error.as_ref())
        {
            return Ok(false);
        }
        if state == InternalState::NeedsAuth && !redrive {
            return Ok(true);
        }
        if state != InternalState::NeedsAuth
            && client.is_some()
            && entry.lock().client.as_ref().map(|client| client.as_mcp_client_arc()) != client.as_ref()
        {
            return Ok(false);
        }
        let guard = entry.lock().attempt_id;
        let oauth_service = self.options.oauth_service.clone();
        let rejected_grant = if let Some(oauth) = &oauth_service {
            if is_remote_mcp_config(&entry.lock().config) {
                let connected_at = entry.lock().connected_at;
                let url = entry.lock().config.url.clone().unwrap();
                oauth.peek_rejected_grant(&name, &url, connected_at).await?
            } else {
                None
            }
        } else {
            None
        };
        if !self.is_current(&entry, guard) {
            return Ok(false);
        }
        if rejected_grant.as_ref().is_some_and(|(_, concurrent)| *concurrent) {
            return Ok(false);
        }
        self.transition(&entry, McpEntryEvent::NeedsAuth);
        let current = self.state.lock().entries.get(&name).cloned();
        if !current.as_ref().is_some_and(|current| Arc::ptr_eq(current, &entry))
            || entry.lock().state != InternalState::NeedsAuth
        {
            return Ok(false);
        }
        let auth_guard = entry.lock().attempt_id;
        let address = Arc::as_ptr(&entry) as usize;
        self.state
            .lock()
            .needs_auth_transitions
            .entry(address)
            .and_modify(|transition| {
                if transition.id == transition_id {
                    transition.attempt_id = auth_guard;
                }
            });
        let operation = self.state.lock().operation_tails.get(&name).cloned();
        if let Some(operation) = operation {
            let _ = operation.await;
        }
        if !self.is_current(&entry, auth_guard) {
            return Ok(false);
        }
        if let (Some((tokens, _)), Some(oauth)) = (rejected_grant, oauth_service) {
            let url = entry.lock().config.url.clone().unwrap();
            if let Err(error) = oauth.invalidate_tokens_if_current(&name, &url, &tokens).await {
                if let Some(log) = &self.options.log {
                    log.warn(
                        "mcp oauth token invalidation failed",
                        Some(serde_json::json!({ "server": name, "reason": error.to_string() })),
                    );
                }
            }
        }
        Ok(self.is_current(&entry, auth_guard))
    }

    pub async fn shutdown(&self) -> Result<(), WireAsyncError> {
        self.stop_reconcile();
        let entries = self
            .state
            .lock()
            .entries
            .drain()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();
        for entry in &entries {
            if let Some(client) = entry.lock().client.take() {
                self.close_for_lifecycle(entry, client);
            }
            if let Some(client) = entry.lock().attempt_client.take() {
                self.close_for_lifecycle(entry, client);
            }
        }
        let operations = self
            .state
            .lock()
            .operation_tails
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let auth_transitions = self
            .state
            .lock()
            .pending_needs_auth_transitions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for operation in operations {
            let _ = operation.await;
        }
        for work in auth_transitions {
            let _ = work.await;
        }
        Ok(())
    }

    pub fn reschedule_retries(&self) {
        let entries = self
            .state
            .lock()
            .entries
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in entries {
            if entry.lock().state == InternalState::RetryWaiting {
                self.transition(&entry, McpEntryEvent::RetryPolicyChanged);
            }
        }
    }

    fn upsert(&self, name: String, config: McpServerConfig) -> Arc<Mutex<InternalEntry>> {
        if let Some(previous) = self.state.lock().entries.get(&name).cloned() {
            let previous_state = previous.lock().state;
            let previous_config = previous.lock().config.clone();
            if is_live_state(previous_state) && mcp_server_configs_equal(&previous_config, &config) {
                return previous;
            }
            self.retire_entry(&previous);
        }
        let disabled = config.enabled == Some(false);
        let entry = Arc::new(Mutex::new(InternalEntry {
            name: name.clone(),
            deferred: self.resolve_deferred(&config, None),
            config,
            state: if disabled { InternalState::Disabled } else { InternalState::Connecting },
            attempt_id: 0,
            attempt_promise: None,
            attempt_client: None,
            failure_streak: 0,
            client: None,
            tools: None,
            raw_tools: None,
            enabled_names: None,
            connected_at: None,
            error: None,
            next_retry_at: None,
        }));
        self.state.lock().entries.insert(name, entry.clone());
        self.emit(&entry);
        if entry.lock().state == InternalState::Connecting {
            self.transition(&entry, McpEntryEvent::Start);
        }
        entry
    }

    fn retire_entry(&self, entry: &Arc<Mutex<InternalEntry>>) {
        if let Some(client) = entry.lock().client.take() {
            self.close_for_lifecycle(entry, client);
        }
        if let Some(client) = entry.lock().attempt_client.take() {
            self.close_for_lifecycle(entry, client);
        }
    }

    fn resolve_deferred(&self, config: &McpServerConfig, tool_count: Option<usize>) -> bool {
        if let Some(deferred) = config.deferred {
            return deferred;
        }
        let defaults = self.defaults();
        let threshold = defaults
            .auto_defer_tool_threshold
            .unwrap_or(AUTO_DEFER_TOOL_THRESHOLD);
        if let Some(tool_count) = tool_count {
            if threshold > 0 && tool_count as u64 >= threshold {
                return true;
            }
        }
        defaults.default_deferred.unwrap_or(false)
    }

    fn transition(&self, entry: &Arc<Mutex<InternalEntry>>, event: McpEntryEvent) {
        let Some(plan) = self.plan_transition(entry, event) else {
            return;
        };
        let automatic_retry = entry.lock().state == InternalState::Connecting
            && entry.lock().failure_streak > 0;
        let before = to_public_entry(&entry.lock());
        let from_family = matches!(entry.lock().state, InternalState::Connected | InternalState::Probing);
        let to_family = matches!(plan.target, InternalState::Connected | InternalState::Probing);
        if !(from_family && to_family) {
            entry.lock().attempt_id += 1;
        }
        if from_family && !to_family {
            if let Some(client) = entry.lock().client.take() {
                self.close_for_lifecycle(entry, client);
            }
        }
        let attempt_client = entry.lock().attempt_client.take();
        if plan.target == InternalState::Connected {
            if let Some(client) = plan.client.clone() {
                entry.lock().client = Some(client);
            }
            if let Some(attempt_client) = attempt_client {
                if plan.client.as_ref() != Some(&attempt_client) {
                    self.close_for_lifecycle(entry, attempt_client);
                }
            }
        } else if let Some(attempt_client) = attempt_client {
            self.close_for_lifecycle(entry, attempt_client);
        }
        if !to_family {
            entry.lock().tools = None;
            entry.lock().raw_tools = None;
            entry.lock().enabled_names = None;
            entry.lock().connected_at = None;
        }
        entry.lock().error = None;
        entry.lock().next_retry_at = None;
        if plan.target != InternalState::Connecting {
            entry.lock().attempt_promise = None;
        }
        entry.lock().state = plan.target;
        match plan.target {
            InternalState::Connecting => {
                if plan.reset_streak {
                    entry.lock().failure_streak = 0;
                }
                let deferred = self.resolve_deferred(&entry.lock().config, None);
                entry.lock().deferred = deferred;
                let attempt_id = entry.lock().attempt_id;
                let this = self.clone_handle();
                let entry_clone = entry.clone();
                let work = async move {
                    this.run_connect_attempt(entry_clone, attempt_id).await;
                    Ok::<(), WireAsyncError>(())
                }
                .boxed();
                let operation = self.enqueue_operation(&entry.lock().name, work);
                entry.lock().attempt_promise = Some(operation);
            }
            InternalState::Connected => {
                if let Some(client) = plan.client {
                    entry.lock().tools = plan.tools;
                    entry.lock().raw_tools = plan.raw_tools;
                    entry.lock().enabled_names = plan.enabled_names;
                    entry.lock().connected_at = Some(
                        self.options
                            .oauth_service
                            .as_ref()
                            .map(|service| service.now())
                            .unwrap_or_else(now_ms),
                    );
                    entry.lock().failure_streak = 0;
                    let attempt_id = entry.lock().attempt_id;
                    self.attach_session_evidence(entry, client, attempt_id);
                }
            }
            InternalState::Probing => {
                if let Some(client) = entry.lock().client.clone() {
                    let attempt_id = entry.lock().attempt_id;
                    let this = self.clone_handle();
                    let entry_clone = entry.clone();
                    let reason = plan.weak_reason.unwrap_or_else(empty_close_reason);
                    let work = async move {
                        this.run_probe(entry_clone, client, attempt_id, reason).await;
                        Ok::<(), WireAsyncError>(())
                    }
                    .boxed();
                    let _ = self.enqueue_operation(&entry.lock().name, work);
                }
            }
            InternalState::RetryWaiting => {
                entry.lock().error = plan.error;
                let streak = if plan.reschedule_retry {
                    entry.lock().failure_streak.saturating_sub(1)
                } else {
                    entry.lock().failure_streak
                };
                let delay_ms = self.next_retry_delay_ms(streak);
                if !plan.reschedule_retry {
                    entry.lock().failure_streak = (entry.lock().failure_streak + 1).min(MAX_FAILURE_STREAK);
                }
                entry.lock().next_retry_at = Some(now_ms() + delay_ms as i64);
                if let Some(log) = &self.options.log {
                    log.info(
                        "mcp server reconnect scheduled",
                        Some(serde_json::json!({
                            "server": entry.lock().name,
                            "delayMs": delay_ms,
                            "attempt": entry.lock().failure_streak,
                        })),
                    );
                }
            }
            InternalState::Failed | InternalState::NeedsAuth => {
                entry.lock().error = plan.error;
            }
            InternalState::Disabled | InternalState::Removed => {}
        }
        let after = to_public_entry(&entry.lock());
        let suppress_retry_status = event.event_type() == Some(McpEntryEventType::RetryDue)
            || (automatic_retry && plan.target == InternalState::RetryWaiting);
        if !suppress_retry_status && (plan.force_emit || !public_entries_equal(&before, &after)) {
            self.emit(entry);
        }
        self.schedule_reconcile();
    }

    fn plan_transition(&self, entry: &Arc<Mutex<InternalEntry>>, event: McpEntryEvent) -> Option<TransitionPlan> {
        let from = entry.lock().state;
        match event {
            McpEntryEvent::Start => (from == InternalState::Connecting).then(|| TransitionPlan {
                target: InternalState::Connecting,
                reset_streak: true,
                ..Default::default()
            }),
            McpEntryEvent::Kick => (!matches!(from, InternalState::Disabled | InternalState::Removed)).then(|| {
                TransitionPlan {
                    target: InternalState::Connecting,
                    reset_streak: true,
                    ..Default::default()
                }
            }),
            McpEntryEvent::RetryDue => (from == InternalState::RetryWaiting).then(|| TransitionPlan {
                target: InternalState::Connecting,
                ..Default::default()
            }),
            McpEntryEvent::RetryPolicyChanged => (from == InternalState::RetryWaiting).then(|| {
                TransitionPlan {
                    target: InternalState::RetryWaiting,
                    error: entry.lock().error.clone(),
                    reschedule_retry: true,
                    ..Default::default()
                }
            }),
            McpEntryEvent::HandshakeSucceeded { attempt_id, client, tools, raw_tools, enabled_names } => {
                if from != InternalState::Connecting || !self.is_current(entry, attempt_id) {
                    self.close_for_lifecycle(entry, client);
                    return None;
                }
                Some(TransitionPlan {
                    target: InternalState::Connected,
                    client: Some(client),
                    tools: Some(tools),
                    raw_tools: Some(raw_tools),
                    enabled_names: Some(enabled_names),
                    ..Default::default()
                })
            }
            McpEntryEvent::HandshakeFailed { attempt_id, error, client } => {
                if from != InternalState::Connecting || !self.is_current(entry, attempt_id) {
                    if let Some(client) = client {
                        self.close_for_lifecycle(entry, client);
                    }
                    return None;
                }
                let target = self.failure_target(classify_startup_failure(error.as_ref()));
                let error = if target == InternalState::NeedsAuth {
                    needs_auth_hint(&entry.lock().name)
                } else {
                    format_startup_error(error.as_ref(), client.as_ref())
                };
                Some(TransitionPlan {
                    target,
                    error: Some(error),
                    ..Default::default()
                })
            }
            McpEntryEvent::ProbeSucceeded { attempt_id, client } => {
                if from != InternalState::Probing
                    || !self.is_current(entry, attempt_id)
                    || entry.lock().client.as_ref() != Some(&client)
                {
                    return None;
                }
                Some(TransitionPlan {
                    target: InternalState::Connected,
                    ..Default::default()
                })
            }
            McpEntryEvent::ProbeFailed { attempt_id, client, reason } => {
                if from != InternalState::Probing
                    || !self.is_current(entry, attempt_id)
                    || entry.lock().client.as_ref() != Some(&client)
                {
                    return None;
                }
                Some(TransitionPlan {
                    target: self.retry_or_failed(),
                    error: Some(format_unexpected_close_error(&entry.lock().name, &reason)),
                    ..Default::default()
                })
            }
            McpEntryEvent::SessionEvidence { evidence } => {
                if !matches!(from, InternalState::Connected | InternalState::Probing) {
                    return None;
                }
                if evidence.kind == SessionEvidenceKind::Weak {
                    return (from == InternalState::Connected).then(|| TransitionPlan {
                        target: InternalState::Probing,
                        weak_reason: Some(evidence.reason),
                        ..Default::default()
                    });
                }
                Some(TransitionPlan {
                    target: self.retry_or_failed(),
                    error: Some(format_unexpected_close_error(&entry.lock().name, &evidence.reason)),
                    ..Default::default()
                })
            }
            McpEntryEvent::NeedsAuth => {
                if !matches!(from, InternalState::Connected | InternalState::Probing) {
                    return None;
                }
                Some(TransitionPlan {
                    target: InternalState::NeedsAuth,
                    error: Some(needs_auth_hint(&entry.lock().name)),
                    ..Default::default()
                })
            }
            McpEntryEvent::Disable => Some(TransitionPlan {
                target: InternalState::Disabled,
                force_emit: true,
                ..Default::default()
            }),
            McpEntryEvent::Remove => Some(TransitionPlan {
                target: InternalState::Removed,
                ..Default::default()
            }),
        }
    }

    fn failure_target(&self, kind: McpFailureKind) -> InternalState {
        match kind {
            McpFailureKind::Config | McpFailureKind::Protocol | McpFailureKind::AuthStatic => InternalState::Failed,
            McpFailureKind::AuthOauth if self.options.oauth_service.is_some() => InternalState::NeedsAuth,
            _ => self.retry_or_failed(),
        }
    }

    fn retry_or_failed(&self) -> InternalState {
        if self.options.reconnect_retry_enabled {
            InternalState::RetryWaiting
        } else {
            InternalState::Failed
        }
    }

    fn next_retry_delay_ms(&self, streak: u32) -> u64 {
        let defaults = self.defaults();
        let initial = defaults.reconnect_initial_delay_ms.unwrap_or(DEFAULT_RECONNECT_INITIAL_DELAY_MS);
        let max = defaults.reconnect_max_delay_ms.unwrap_or(DEFAULT_RECONNECT_MAX_DELAY_MS);
        let base = initial.saturating_mul(2u64.saturating_pow(streak)).min(max);
        (base as f64 * (0.5 + fastrand::f64() * 0.5)) as u64
    }

    fn enqueue_operation(
        &self,
        name: &str,
        work: BoxFuture<'static, Result<(), WireAsyncError>>,
    ) -> AttemptFuture {
        let previous = self.state.lock().operation_tails.get(name).cloned();
        let operation = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            work.await.map_err(SharedManagerError::new)
        }
        .boxed()
        .shared();
        let tail = {
            let operation = operation.clone();
            async move {
                let _ = operation.await;
                Ok::<(), SharedManagerError>(())
            }
            .boxed()
            .shared()
        };
        let serial = {
            let mut state = self.state.lock();
            let serial = state.operation_serials.get(name).copied().unwrap_or(0) + 1;
            state.operation_serials.insert(name.to_string(), serial);
            state.operation_tails.insert(name.to_string(), tail.clone());
            serial
        };
        let state = self.state.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let _ = tail.await;
            let mut state = state.lock();
            if state.operation_serials.get(&name) == Some(&serial) {
                state.operation_tails.remove(&name);
            }
        });
        operation
    }

    fn close_for_lifecycle(&self, entry: &Arc<Mutex<InternalEntry>>, client: RuntimeMcpClient) {
        let name = entry.lock().name.clone();
        let _ = self.enqueue_operation(&name, async move {
            close_runtime_client(&client).await;
            Ok(())
        }.boxed());
    }

    async fn run_connect_attempt(&self, entry: Arc<Mutex<InternalEntry>>, attempt_id: u64) {
        let timeout_ms = entry
            .lock()
            .config
            .startup_timeout_ms
            .or(self.defaults().startup_timeout_ms)
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS);
        let mut client = None;
        let result = async {
            if !self.is_current(&entry, attempt_id) {
                return Ok(());
            }
            let startup_client = self
                .create_client(&entry.lock().config, &entry.lock().name, timeout_ms)
                .await?;
            client = Some(startup_client.clone());
            if !self.is_current(&entry, attempt_id) {
                close_runtime_client(&startup_client).await;
                return Ok(());
            }
            entry.lock().attempt_client = Some(startup_client.clone());
            let discovery = {
                let this = self.clone_handle();
                let client = startup_client.clone();
                let config = entry.lock().config.clone();
                async move { this.connect_and_discover_tools(&client, &config).await }.boxed()
            };
            let discovered = with_timeout(
                discovery,
                timeout_ms,
                {
                    let this = self.clone_handle();
                    let entry = entry.clone();
                    let startup_client = startup_client.clone();
                    move || {
                        if entry.lock().attempt_client.as_ref() == Some(&startup_client) {
                            entry.lock().attempt_client = None;
                        }
                        this.close_for_lifecycle(&entry, startup_client);
                    }
                },
            )
            .await?;
            let enabled_names = compute_enabled_names(&entry.lock().config, &discovered.tools);
            let deferred = self.resolve_deferred(&entry.lock().config, Some(enabled_names.len()));
            entry.lock().deferred = deferred;
            self.transition(
                &entry,
                McpEntryEvent::HandshakeSucceeded {
                    attempt_id,
                    client: startup_client,
                    tools: discovered.tools,
                    raw_tools: discovered.raw_tools,
                    enabled_names,
                },
            );
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.transition(
                &entry,
                McpEntryEvent::HandshakeFailed {
                    attempt_id,
                    error,
                    client,
                },
            );
        }
    }

    fn attach_session_evidence(&self, entry: &Arc<Mutex<InternalEntry>>, client: RuntimeMcpClient, attempt_id: u64) {
        let this = self.clone_handle();
        let entry = entry.clone();
        client.on_session_evidence(Arc::new(move |evidence| {
            if !this.is_current(&entry, attempt_id) {
                return;
            }
            if entry.lock().client.as_ref() != Some(&client) {
                return;
            }
            this.transition(&entry, McpEntryEvent::SessionEvidence { evidence });
        }));
    }

    async fn run_probe(
        &self,
        entry: Arc<Mutex<InternalEntry>>,
        client: RuntimeMcpClient,
        attempt_id: u64,
        reason: UnexpectedCloseReason,
    ) {
        let alive = probe_mcp_liveness(
            client.as_mcp_client(),
            AbortSignal::timeout(MCP_LIVENESS_PROBE_TIMEOUT_MS),
        )
        .await;
        if alive {
            self.transition(
                &entry,
                McpEntryEvent::ProbeSucceeded { attempt_id, client },
            );
        } else {
            self.transition(
                &entry,
                McpEntryEvent::ProbeFailed {
                    attempt_id,
                    client,
                    reason,
                },
            );
        }
    }

    fn schedule_reconcile(&self) {
        let entries = self
            .state
            .lock()
            .entries
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let nearest = entries
            .iter()
            .filter(|entry| entry.lock().state == InternalState::RetryWaiting)
            .filter_map(|entry| entry.lock().next_retry_at)
            .min();
        let Some(nearest) = nearest else {
            self.stop_reconcile();
            return;
        };
        if self.state.lock().reconcile_timer_at.is_some_and(|current| current <= nearest) {
            return;
        }
        self.stop_reconcile();
        self.state.lock().reconcile_timer_at = Some(nearest);
        let this = self.clone_handle();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis((nearest - now_ms()).max(0) as u64)).await;
            this.state.lock().reconcile_timer = None;
            this.state.lock().reconcile_timer_at = None;
            let now = now_ms();
            let entries = this
                .state
                .lock()
                .entries
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for entry in entries {
                if entry.lock().state == InternalState::RetryWaiting
                    && entry.lock().next_retry_at.is_some_and(|retry| retry <= now)
                {
                    this.transition(&entry, McpEntryEvent::RetryDue);
                }
            }
            this.schedule_reconcile();
        });
        self.state.lock().reconcile_timer = Some(timer);
    }

    fn stop_reconcile(&self) {
        let mut state = self.state.lock();
        if let Some(timer) = state.reconcile_timer.take() {
            timer.abort();
        }
        state.reconcile_timer_at = None;
    }

    async fn create_client(
        &self,
        config: &McpServerConfig,
        name: &str,
        startup_timeout_ms: u64,
    ) -> Result<RuntimeMcpClient, WireAsyncError> {
        let tool_call_timeout_ms = config
            .tool_timeout_ms
            .or(self.defaults().tool_timeout_ms);
        let client_name = self.options.resolve_client_name.as_ref().and_then(|resolve| resolve());
        match config.transport {
            McpTransport::Stdio => {
                let environment_resolver = self.options.environment_resolver.clone();
                let environment_id = config
                    .environment_id
                    .clone()
                    .or_else(|| self.options.environment_id.clone());
                if environment_resolver.is_none()
                    || environment_id.is_none()
                    || (self.options.require_stdio_environment_id && config.environment_id.is_none())
                {
                    return Err(Box::new(McpClassifiedStartupError::new(
                        McpFailureKind::Config,
                        "MCP stdio requires environment_id and environment binding",
                        None,
                    )));
                }
                let stdio_cwd = self
                    .options
                    .stdio_cwd
                    .as_ref()
                    .map(|resolve| resolve());
                Ok(RuntimeMcpClient::Stdio(Arc::new(StdioMcpClient::new(
                    config.clone(),
                    StdioMcpClientOptions {
                        client_name,
                        client_version: None,
                        startup_timeout_ms: Some(startup_timeout_ms),
                        tool_call_timeout_ms,
                        default_cwd: stdio_cwd,
                        environment_resolver: environment_resolver.unwrap(),
                        environment_id: environment_id.unwrap(),
                        session_id: self.options.session_id.clone(),
                    },
                ))))
            }
            McpTransport::Sse => {
                let oauth_provider = self.resolve_oauth_provider(config, name).await?;
                Ok(RuntimeMcpClient::Sse(Arc::new(SseMcpClient::new(
                    config.clone(),
                    SseMcpClientOptions {
                        client_name,
                        client_version: None,
                        startup_timeout_ms: Some(startup_timeout_ms),
                        tool_call_timeout_ms,
                        env_lookup: self.options.env_lookup.clone(),
                        fetch: None,
                        oauth_provider,
                    },
                )?)))
            }
            McpTransport::Http => {
                let oauth_provider = self.resolve_oauth_provider(config, name).await?;
                Ok(RuntimeMcpClient::Http(Arc::new(HttpMcpClient::new(
                    config.clone(),
                    HttpMcpClientOptions {
                        client_name,
                        client_version: None,
                        startup_timeout_ms: Some(startup_timeout_ms),
                        tool_call_timeout_ms,
                        env_lookup: self.options.env_lookup.clone(),
                        fetch: None,
                        oauth_provider,
                    },
                )?)))
            }
        }
    }

    async fn resolve_oauth_provider(
        &self,
        config: &McpServerConfig,
        name: &str,
    ) -> Result<Option<Arc<crate::mcp_core::oauth::provider::McpOAuthClientProvider>>, WireAsyncError> {
        let Some(oauth_service) = &self.options.oauth_service else {
            return Ok(None);
        };
        if !is_remote_mcp_config(config) || config.bearer_token_env_var.is_some() {
            return Ok(None);
        }
        if !oauth_service.has_tokens(name, config.url.as_ref().unwrap()).await? {
            return Ok(None);
        }
        Ok(Some(oauth_service.get_provider(name, config.url.as_ref().unwrap())))
    }

    fn should_mark_needs_auth(&self, entry: &Arc<Mutex<InternalEntry>>, error: &(dyn Error + 'static)) -> bool {
        if self.options.oauth_service.is_none() || !is_remote_mcp_config(&entry.lock().config) {
            return false;
        }
        if entry.lock().config.bearer_token_env_var.is_some() {
            return false;
        }
        if entry.lock().config.headers.is_some() && entry.lock().config.auth.is_none() {
            return false;
        }
        is_unauthorized_like_error(error)
    }

    async fn connect_and_discover_tools(
        &self,
        client: &RuntimeMcpClient,
        config: &McpServerConfig,
    ) -> Result<DiscoveredTools, WireAsyncError> {
        client.as_mcp_client().connect().await?;
        let mcp_tools = match client.as_mcp_client().list_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                if is_remote_mcp_config(config) {
                    return Err(classify_remote_discovery_error(config, error));
                }
                return Err(classify_mcp_discovery_error(error));
            }
        };
        let compact = self.defaults().compact_tool_schemas == Some(true);
        let prepared = mcp_tools
            .iter()
            .map(|tool| {
                Ok(super::super::human::llm::message::ToolDescription {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: super::types::assert_mcp_input_schema(&tool.name, &tool.input_schema)?,
                })
            })
            .collect::<Result<Vec<_>, WireAsyncError>>()
            .map_err(|error| {
                if error.downcast_ref::<Error2>().is_some_and(|error| {
                    error.code().as_str() == MCP_STARTUP_FAILED
                }) {
                    let message = error.to_string();
                    Box::new(McpClassifiedStartupError::new(
                        McpFailureKind::Protocol,
                        message,
                        Some(error),
                    )) as WireAsyncError
                } else {
                    error
                }
            })?;
        let tools = if compact {
            prepared
                .iter()
                .map(|tool| super::super::human::llm::message::ToolDescription {
                    parameters: compact_mcp_input_schema(&tool.parameters),
                    ..tool.clone()
                })
                .collect::<Vec<_>>()
        } else {
            prepared.clone()
        };
        if compact {
            if let Some(telemetry) = &self.options.telemetry {
                let mut properties = Map::new();
                properties.insert("tool_count".to_string(), tools.len().into());
                properties.insert(
                    "original_chars".to_string(),
                    serde_json::to_string(&prepared.iter().map(|tool| &tool.parameters).collect::<Vec<_>>())
                        .unwrap()
                        .encode_utf16()
                        .count()
                        .into(),
                );
                properties.insert(
                    "compacted_chars".to_string(),
                    serde_json::to_string(&tools.iter().map(|tool| &tool.parameters).collect::<Vec<_>>())
                        .unwrap()
                        .encode_utf16()
                        .count()
                        .into(),
                );
                telemetry.track2("mcp_schema_compacted", Some(properties));
            }
        }
        Ok(DiscoveredTools {
            tools,
            raw_tools: mcp_tools,
        })
    }

    fn is_current(&self, entry: &Arc<Mutex<InternalEntry>>, attempt_id: u64) -> bool {
        let name = entry.lock().name.clone();
        let current = self.state.lock().entries.get(&name).cloned();
        current.is_some_and(|current| {
            Arc::ptr_eq(&current, entry) && current.lock().attempt_id == attempt_id
        })
    }

    fn emit(&self, entry: &Arc<Mutex<InternalEntry>>) {
        let view = to_public_entry(&entry.lock());
        if matches!(view.status, McpServerStatus::Failed | McpServerStatus::NeedsAuth) {
            if let Some(log) = &self.options.log {
                log.error(
                    "mcp server unavailable",
                    Some(serde_json::json!({
                        "server": &view.name,
                        "transport": view.transport.as_str(),
                        "status": view.status.as_str(),
                        "reason": &view.error,
                    })),
                );
            }
        }
        let listeners = self.state.lock().listeners.values().cloned().collect::<Vec<_>>();
        for listener in listeners {
            let view = view.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(view)));
        }
    }

    fn defaults(&self) -> McpDefaults {
        self.options
            .resolve_defaults
            .as_ref()
            .map(|resolve| resolve())
            .unwrap_or_default()
    }

    fn clone_handle(&self) -> Self {
        Self {
            options: McpConnectionManagerOptions {
                env_lookup: self.options.env_lookup.clone(),
                stdio_cwd: self.options.stdio_cwd.clone(),
                environment_resolver: self.options.environment_resolver.clone(),
                environment_id: self.options.environment_id.clone(),
                require_stdio_environment_id: self.options.require_stdio_environment_id,
                session_id: self.options.session_id.clone(),
                reconnect_retry_enabled: self.options.reconnect_retry_enabled,
                oauth_service: self.options.oauth_service.clone(),
                telemetry: self.options.telemetry.clone(),
                log: self.options.log.clone(),
                resolve_defaults: self.options.resolve_defaults.clone(),
                resolve_client_name: self.options.resolve_client_name.clone(),
            },
            state: self.state.clone(),
        }
    }
}

#[async_trait]
impl McpConnectionView for McpConnectionManager {
    fn oauth_service(&self) -> Option<Arc<McpOAuthService>> {
        McpConnectionManager::oauth_service(self)
    }

    fn list(&self) -> Vec<McpServerEntry> {
        McpConnectionManager::list(self)
    }

    fn get(&self, name: &str) -> Option<McpServerEntry> {
        McpConnectionManager::get(self, name)
    }

    fn deferred_of(&self, name: &str) -> bool {
        McpConnectionManager::deferred_of(self, name)
    }

    fn resolved(&self, name: &str) -> Option<McpResolved> {
        McpConnectionManager::resolved(self, name)
    }

    fn get_remote_server_url(&self, name: &str) -> Option<String> {
        McpConnectionManager::get_remote_server_url(self, name)
    }

    async fn mark_needs_auth(&self, name: &str, error: WireAsyncError, client: Option<Arc<dyn MCPClient>>) -> Result<bool, WireAsyncError> {
        McpConnectionManager::mark_needs_auth(self, name, error, client).await
    }

    async fn reconnect(&self, name: &str) -> Result<(), WireAsyncError> {
        McpConnectionManager::reconnect(self, name).await
    }

    async fn reconnect_and_join(&self, name: &str) -> Result<(), WireAsyncError> {
        McpConnectionManager::reconnect_and_join(self, name).await
    }

    async fn wait_for_initial_load(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError> {
        McpConnectionManager::wait_for_initial_load(self, signal).await
    }

    fn initial_load_duration_ms(&self) -> u64 {
        McpConnectionManager::initial_load_duration_ms(self)
    }

    fn on_status_change(&self, listener: McpStatusListener) -> Box<dyn IDisposable> {
        McpConnectionManager::on_status_change(self, listener)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum McpEntryEventType {
    RetryDue,
}

enum McpEntryEvent {
    Start,
    Kick,
    RetryDue,
    RetryPolicyChanged,
    HandshakeSucceeded {
        attempt_id: u64,
        client: RuntimeMcpClient,
        tools: Vec<super::super::human::llm::message::ToolDescription>,
        raw_tools: Vec<MCPToolDefinition>,
        enabled_names: HashSet<String>,
    },
    HandshakeFailed {
        attempt_id: u64,
        error: WireAsyncError,
        client: Option<RuntimeMcpClient>,
    },
    ProbeSucceeded {
        attempt_id: u64,
        client: RuntimeMcpClient,
    },
    ProbeFailed {
        attempt_id: u64,
        client: RuntimeMcpClient,
        reason: UnexpectedCloseReason,
    },
    SessionEvidence {
        evidence: SessionEvidence,
    },
    NeedsAuth,
    Disable,
    Remove,
}

impl McpEntryEvent {
    fn event_type(&self) -> Option<McpEntryEventType> {
        match self {
            Self::RetryDue => Some(McpEntryEventType::RetryDue),
            _ => None,
        }
    }
}

struct TransitionPlan {
    target: InternalState,
    client: Option<RuntimeMcpClient>,
    tools: Option<Vec<super::super::human::llm::message::ToolDescription>>,
    raw_tools: Option<Vec<MCPToolDefinition>>,
    enabled_names: Option<HashSet<String>>,
    error: Option<String>,
    reset_streak: bool,
    reschedule_retry: bool,
    force_emit: bool,
    weak_reason: Option<UnexpectedCloseReason>,
}

impl Default for TransitionPlan {
    fn default() -> Self {
        Self {
            target: InternalState::Failed,
            client: None,
            tools: None,
            raw_tools: None,
            enabled_names: None,
            error: None,
            reset_streak: false,
            reschedule_retry: false,
            force_emit: false,
            weak_reason: None,
        }
    }
}

struct DiscoveredTools {
    tools: Vec<super::super::human::llm::message::ToolDescription>,
    raw_tools: Vec<MCPToolDefinition>,
}

fn is_live_state(state: InternalState) -> bool {
    matches!(state, InternalState::Connecting | InternalState::Connected | InternalState::Probing)
}

fn to_public_entry(entry: &InternalEntry) -> McpServerEntry {
    McpServerEntry {
        name: entry.name.clone(),
        transport: entry.config.transport,
        status: match entry.state {
            InternalState::Connecting => McpServerStatus::Pending,
            InternalState::Connected | InternalState::Probing => McpServerStatus::Connected,
            InternalState::RetryWaiting | InternalState::Failed => McpServerStatus::Failed,
            InternalState::NeedsAuth => McpServerStatus::NeedsAuth,
            InternalState::Disabled => McpServerStatus::Disabled,
            InternalState::Removed => McpServerStatus::Removed,
        },
        tool_count: entry.enabled_names.as_ref().map_or(0, HashSet::len),
        error: entry.error.clone(),
        next_retry_at: (entry.state == InternalState::RetryWaiting)
            .then_some(entry.next_retry_at)
            .flatten(),
    }
}

fn public_entries_equal(a: &McpServerEntry, b: &McpServerEntry) -> bool {
    a.status == b.status
        && a.tool_count == b.tool_count
        && a.error == b.error
        && a.next_retry_at == b.next_retry_at
}

fn compute_enabled_names(config: &McpServerConfig, tools: &[super::super::human::llm::message::ToolDescription]) -> HashSet<String> {
    let enabled = config.enabled_tools.as_ref().map(|tools| tools.iter().collect::<HashSet<_>>());
    let disabled = config.disabled_tools.as_ref().map(|tools| tools.iter().collect::<HashSet<_>>());
    tools
        .iter()
        .filter(|tool| enabled.as_ref().is_none_or(|enabled| enabled.contains(&tool.name)))
        .filter(|tool| disabled.as_ref().is_none_or(|disabled| !disabled.contains(&tool.name)))
        .map(|tool| tool.name.clone())
        .collect()
}

fn classify_startup_failure(error: &(dyn Error + 'static)) -> McpFailureKind {
    if let Some(error) = error.downcast_ref::<McpClassifiedStartupError>() {
        return error.failure_kind;
    }
    if let Some(error) = error.downcast_ref::<EnvironmentError>() {
        return if error.code() == "environment.unavailable" {
            McpFailureKind::Transport
        } else {
            McpFailureKind::Config
        };
    }
    if error.downcast_ref::<Error2>().is_some_and(|error| {
        error.code().as_str() == CONFIG_INVALID || error.code().as_str() == NOT_IMPLEMENTED_CODE
    }) {
        return McpFailureKind::Config;
    }
    if is_unauthorized_like_error(error) {
        return McpFailureKind::AuthOauth;
    }
    McpFailureKind::Transport
}

fn is_unauthorized_like_error(error: &(dyn Error + 'static)) -> bool {
    if error.downcast_ref::<McpSdkError>().is_some() {
        return false;
    }
    if error.downcast_ref::<super::client_remote::UnauthorizedError>().is_some() {
        return true;
    }
    if error.downcast_ref::<super::client_remote::SseError>().is_some_and(|error| error.code == Some(401)) {
        return true;
    }
    if error.downcast_ref::<super::client_remote::StreamableHTTPError>().is_some_and(|error| error.code == Some(401)) {
        return true;
    }
    if error.downcast_ref::<Error2>().is_some_and(|error| {
        error.name() == "InvalidGrantError"
            || error.name() == "UnauthorizedError"
            || error.status_code() == Some(401)
            || error.code().as_str() == "401"
    }) {
        return true;
    }
    let message = error.to_string();
    regex::Regex::new(r"\b401\b").unwrap().is_match(&message)
        || regex::Regex::new(r"(?i)unauthorized").unwrap().is_match(&message)
}

fn needs_auth_hint(name: &str) -> String {
    format!("{name} requires OAuth — run /mcp-config login {name}")
}

fn format_startup_error(error: &(dyn Error + 'static), client: Option<&RuntimeMcpClient>) -> String {
    let unwrapped = error
        .downcast_ref::<McpClassifiedStartupError>()
        .and_then(|error| error.source())
        .unwrap_or(error);
    let source = unwrapped
        .downcast_ref::<HostProcessError>()
        .filter(|error| error.code().as_str() == OS_PROCESS_SPAWN_FAILED)
        .and_then(|error| error.source())
        .unwrap_or(unwrapped);
    let base = source.to_string();
    let Some(snapshot) = client.and_then(RuntimeMcpClient::stderr_snapshot) else {
        return base;
    };
    if snapshot.is_empty() {
        return base;
    }
    format!("{base}\nstderr: {}", snapshot.trim_end())
}

fn format_unexpected_close_error(name: &str, reason: &UnexpectedCloseReason) -> String {
    let mut parts = vec![format!("MCP server \"{name}\" closed unexpectedly")];
    if let Some(error) = &reason.error {
        parts.push(error.to_string());
    }
    if let Some(stderr) = &reason.stderr {
        if !stderr.is_empty() {
            parts.push(format!("stderr: {}", stderr.trim_end()));
        }
    }
    parts.join("\n")
}

pub fn mcp_server_configs_equal(a: &McpServerConfig, b: &McpServerConfig) -> bool {
    stable_config_json(&serde_json::to_value(a).unwrap()) == stable_config_json(&serde_json::to_value(b).unwrap())
}

fn stable_config_json(value: &Value) -> String {
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(stable_config_json).collect::<Vec<_>>().join(",")
        ),
        Value::Object(values) => {
            let mut entries = values
                .iter()
                .map(|(key, value)| format!("{}:{}", serde_json::to_string(key).unwrap(), stable_config_json(value)))
                .collect::<Vec<_>>();
            entries.sort();
            format!("{{{}}}", entries.join(","))
        }
        value => serde_json::to_string(value).unwrap(),
    }
}

async fn with_timeout<T: Send + 'static>(
    future: BoxFuture<'static, Result<T, WireAsyncError>>,
    timeout_ms: u64,
    on_timeout: impl FnOnce() + Send + 'static,
) -> Result<T, WireAsyncError> {
    let mut task = tokio::spawn(future);
    tokio::select! {
        result = &mut task => match result {
            Ok(result) => result,
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        },
        _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
            on_timeout();
            Err(Box::new(Error2::new(MCP_STARTUP_FAILED, format!("Timed out after {timeout_ms}ms"))) as WireAsyncError)
        }
    }
}

async fn close_runtime_client(client: &RuntimeMcpClient) {
    match client {
        RuntimeMcpClient::Stdio(client) => {
            let _ = client.close().await;
        }
        RuntimeMcpClient::Http(client) => {
            let _ = client.close().await;
        }
        RuntimeMcpClient::Sse(client) => {
            let _ = client.close().await;
        }
    }
}

fn initial_load_abort_error(signal: &AbortSignal) -> WireAsyncError {
    let Some(reason) = signal.reason() else {
        return Box::new(abort_error());
    };
    if let Some(error) = reason.downcast_ref::<AbortError>() {
        return Box::new(error.clone());
    }
    Box::new(SharedManagerError::from_message(reason.to_string()))
}

fn empty_close_reason() -> UnexpectedCloseReason {
    UnexpectedCloseReason {
        error: None,
        stderr: None,
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
