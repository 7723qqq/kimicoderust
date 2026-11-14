use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;

use crate::i18n::{LocalizedText, i18n_params};
use crate::mcp::client::McpClient;
use crate::mcp::errors::McpError;
use crate::mcp::types::McpTool;
use crate::native::tool_naming::qualify_mcp_tool_name;
use crate::primitives::teardown::{Disposer, Ledger, TeardownError, TeardownReason};
use crate::server::files::FileStore;
use crate::tools::registry::{ToolDefinition, ToolDisclosure, ToolRegistry, ToolSource};
use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpToolSummary {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub tool_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
    pub tools: Vec<McpToolSummary>,
}

fn public_status(internal: &str) -> &str {
    match internal {
        "retry-waiting" => "failed",
        "probing" => "connected",
        other => other,
    }
}

fn is_live_status(internal: &str) -> bool {
    matches!(internal, "connected" | "probing")
}

#[derive(Clone)]
struct EvidenceContext {
    name: String,
    client: Arc<McpClient>,
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
    servers: Arc<RwLock<HashMap<String, ServerState>>>,
    cached_tools: Arc<RwLock<HashMap<String, (String, McpTool)>>>,
    registry: Arc<RwLock<ToolRegistry>>,
    status_listeners: Arc<Mutex<Vec<(McpStatusSubscription, McpStatusListener)>>>,
    probe_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    policy: RetryPolicy,
}

impl EvidenceContext {
    async fn is_current(&self) -> bool {
        let clients = self.clients.read().await;
        matches!(clients.get(&self.name), Some(client) if Arc::ptr_eq(client, &self.client))
    }

    async fn fail(&self, reason: String) {
        if !self.is_current().await {
            return;
        }
        {
            let now = chrono::Utc::now().timestamp_millis();
            let mut servers = self.servers.write().await;
            let Some(state) = servers.get_mut(&self.name) else {
                return;
            };

            if !is_live_status(&state.status) {
                return;
            }
            let (status, retry_at, streak) =
                next_failure_state(self.policy, state.retry_streak, now);
            state.status = status.into();
            state.retry_at_ms = retry_at;
            state.retry_streak = streak;
            state.error = Some(reason);
            state.raw_tools.clear();
        }
        let removed: Vec<String> = {
            let mut cached = self.cached_tools.write().await;
            let names = cached
                .iter()
                .filter(|(_, (server, _))| *server == self.name)
                .map(|(qualified, _)| qualified.clone())
                .collect();
            cached.retain(|_, (server, _)| *server != self.name);
            names
        };
        let mut registry = self.registry.write().await;
        for qualified in removed {
            registry.unregister(&qualified);
        }
        self.emit().await;
    }

    async fn probe(&self, reason: String) {
        if !self.is_current().await {
            return;
        }
        {
            let mut servers = self.servers.write().await;
            let Some(state) = servers.get_mut(&self.name) else {
                return;
            };
            if state.status != "connected" {
                return;
            }
            state.status = "probing".into();
        }

        if self.client.probe_liveness().await {
            let mut servers = self.servers.write().await;
            if let Some(state) = servers.get_mut(&self.name)
                && state.status == "probing"
            {
                state.status = "connected".into();
            }
            return;
        }
        self.fail(format_unexpected_close_error(&self.name, &reason))
            .await;
    }

    async fn emit(&self) {
        let entry = {
            let servers = self.servers.read().await;
            let Some(state) = servers.get(&self.name) else {
                return;
            };
            to_public_entry(&self.name, state)
        };
        let listeners = self
            .status_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        fan_out_status(&listeners, &entry);
    }
}

fn format_unexpected_close_error(name: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("MCP server \"{name}\" closed unexpectedly")
    } else {
        format!("MCP server \"{name}\" closed unexpectedly\n{detail}")
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpSessionWarning {
    pub code: String,
    pub message: String,

    pub severity: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpServerRecipe {
    Sse {
        url: String,
        headers: HashMap<String, String>,
        bearer_token_env_var: Option<String>,
    },

    Http {
        url: String,
        headers: HashMap<String, String>,
        bearer_token_env_var: Option<String>,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    },

    Mock,
}

impl McpServerRecipe {
    fn transport_label(&self) -> String {
        match self {
            McpServerRecipe::Sse { .. } => "sse".into(),
            McpServerRecipe::Http { .. } => "http".into(),
            McpServerRecipe::Stdio { .. } => "stdio".into(),
            McpServerRecipe::Mock => "mock".into(),
        }
    }
}

struct ServerState {
    recipe: McpServerRecipe,

    status: String,
    error: Option<String>,

    raw_tools: Vec<McpTool>,

    deferred: bool,

    deferred_declared: Option<bool>,

    connected_at_ms: Option<i64>,

    oauth: bool,

    retry_at_ms: Option<i64>,

    retry_streak: u32,
}

#[derive(Debug, Clone, PartialEq)]
struct ToolFilter {
    enabled: bool,
    enabled_tools: Option<HashSet<String>>,
    disabled_tools: HashSet<String>,
}

impl ToolFilter {
    fn all() -> Self {
        Self {
            enabled: true,
            enabled_tools: None,
            disabled_tools: HashSet::new(),
        }
    }

    fn allows(&self, tool_name: &str) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(enabled) = &self.enabled_tools
            && !enabled.contains(tool_name)
        {
            return false;
        }
        !self.disabled_tools.contains(tool_name)
    }
}

pub const DEFAULT_MCP_STARTUP_TIMEOUT_MS: u64 = 30_000;

pub const MCP_INITIAL_LOAD_WAIT_MS: u64 = 2_000;

const MCP_STARTUP_TIMEOUT_ENV: &str = "KIMI_MCP_STARTUP_TIMEOUT_MS";
const MCP_TOOL_TIMEOUT_ENV: &str = "KIMI_MCP_TOOL_TIMEOUT_MS";

const MCP_AUTO_DEFER_TOOL_THRESHOLD_ENV: &str = "KIMI_MCP_AUTO_DEFER_TOOL_THRESHOLD";

const MCP_RECONNECT_INITIAL_DELAY_ENV: &str = "KIMI_MCP_RECONNECT_INITIAL_DELAY_MS";
const MCP_RECONNECT_MAX_DELAY_ENV: &str = "KIMI_MCP_RECONNECT_MAX_DELAY_MS";

const MAX_FAILURE_STREAK: u32 = 31;

const DEFAULT_AUTO_DEFER_TOOL_THRESHOLD: u64 = 24;

const MAX_AUTO_DEFER_TOOL_THRESHOLD: u64 = 1_000_000;

const MAX_MCP_TIMEOUT_MS: u64 = 2_147_483_647;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct McpStatusSubscription(u64);

pub type McpStatusListener = Box<dyn Fn(McpServerEntry) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct McpServerOptions {
    pub enabled: bool,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
    pub startup_timeout_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,

    pub deferred: Option<bool>,

    pub oauth: Option<bool>,
}

impl Default for McpServerOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            enabled_tools: None,
            disabled_tools: None,
            startup_timeout_ms: None,
            tool_timeout_ms: None,
            deferred: None,
            oauth: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct McpTimeouts {
    startup: Duration,
    tool: Option<Duration>,
}

fn parse_timeout_env(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let parsed: u64 = raw.trim().parse().ok()?;
    (1..=MAX_MCP_TIMEOUT_MS).contains(&parsed).then_some(parsed)
}

fn resolve_startup_timeout(per_server: Option<u64>, env: Option<u64>, global: Option<u64>) -> u64 {
    per_server
        .or(env)
        .or(global)
        .unwrap_or(DEFAULT_MCP_STARTUP_TIMEOUT_MS)
}

fn resolve_tool_timeout(
    per_server: Option<u64>,
    env: Option<u64>,
    global: Option<u64>,
) -> Option<u64> {
    per_server.or(env).or(global)
}

fn parse_auto_defer_tool_threshold_env(name: &str) -> Option<u64> {
    parse_auto_defer_tool_threshold(std::env::var(name).ok().as_deref())
}

fn parse_auto_defer_tool_threshold(raw: Option<&str>) -> Option<u64> {
    let parsed: u64 = raw?.trim().parse().ok()?;
    (parsed <= MAX_AUTO_DEFER_TOOL_THRESHOLD).then_some(parsed)
}

fn resolve_deferred(
    declared: Option<bool>,
    tool_count: usize,
    threshold: u64,
    default_deferred: Option<bool>,
) -> bool {
    if let Some(value) = declared {
        return value;
    }
    if threshold > 0 && tool_count as u64 >= threshold {
        return true;
    }
    default_deferred.unwrap_or(false)
}

pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
    cached_tools: Arc<RwLock<HashMap<String, (String, McpTool)>>>,
    removed_tools: Arc<RwLock<HashMap<String, String>>>,

    registry: Arc<RwLock<ToolRegistry>>,
    servers: Arc<RwLock<HashMap<String, ServerState>>>,
    filters: Arc<RwLock<HashMap<String, ToolFilter>>>,
    timeouts: Arc<RwLock<HashMap<String, McpTimeouts>>>,

    defaults: Arc<std::sync::RwLock<crate::config::McpConfig>>,

    in_flight: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,

    oauth: std::sync::RwLock<Option<Arc<crate::mcp::oauth::McpOAuthService>>>,

    attachments: Arc<RwLock<FileStore>>,

    status_listeners: Arc<Mutex<Vec<(McpStatusSubscription, McpStatusListener)>>>,
    next_status_id: std::sync::atomic::AtomicU64,

    ready_tx: tokio::sync::watch::Sender<bool>,
    ready_rx: tokio::sync::watch::Receiver<bool>,

    initial_load_wait_expired: std::sync::atomic::AtomicBool,

    default_stdio_cwd: std::sync::RwLock<Option<std::path::PathBuf>>,

    retry_ticker_started: std::sync::atomic::AtomicBool,

    retry_ticker_stop: Arc<std::sync::atomic::AtomicBool>,

    probe_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,

    connect_tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,

    client_name: Arc<std::sync::RwLock<Option<String>>>,

    dispose: tokio::sync::Mutex<Ledger>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(true);
        let clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let retry_ticker_stop: Arc<std::sync::atomic::AtomicBool> =
            Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let connect_tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>> =
            Arc::new(Mutex::new(Vec::new()));

        let mut dispose = Ledger::new("mcp-manager");
        {
            let clients = Arc::clone(&clients);
            dispose
                .register_labeled(
                    "close-clients",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            let drained: Vec<Arc<McpClient>> = {
                                let mut guard = clients.write().await;
                                guard.drain().map(|(_, client)| client).collect()
                            };
                            for client in drained {
                                client.close().await;
                            }
                            Ok(())
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }
        {
            let probes = Arc::clone(&probe_tasks);
            dispose
                .register_labeled(
                    "drain-probes",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            let drained: Vec<tokio::task::JoinHandle<()>> = {
                                let mut guard = probes.lock().unwrap_or_else(|e| e.into_inner());
                                std::mem::take(&mut *guard)
                            };

                            let mut failure: Option<TeardownError> = None;
                            for probe in drained {
                                if let Err(error) = probe.await
                                    && failure.is_none()
                                {
                                    failure = Some(Box::new(error));
                                }
                            }
                            match failure {
                                Some(error) => Err(error),
                                None => Ok(()),
                            }
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }
        {
            let startups = Arc::clone(&connect_tasks);
            dispose
                .register_labeled(
                    "abort-startups",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            let drained: Vec<tokio::task::AbortHandle> = {
                                let mut guard = startups.lock().unwrap_or_else(|e| e.into_inner());
                                std::mem::take(&mut *guard)
                            };
                            for handle in drained {
                                handle.abort();
                            }
                            Ok(())
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }
        {
            let stop = Arc::clone(&retry_ticker_stop);
            dispose
                .register_labeled(
                    "stop-retry-ticker",
                    Disposer::new(move |_| {
                        Box::pin(async move {
                            stop.store(true, std::sync::atomic::Ordering::SeqCst);
                            Ok(())
                        })
                    }),
                )
                .expect("a fresh ledger accepts registrations");
        }

        Self {
            clients,
            cached_tools: Arc::new(RwLock::new(HashMap::new())),
            removed_tools: Arc::new(RwLock::new(HashMap::new())),
            registry: Arc::new(RwLock::new(ToolRegistry::new())),
            servers: Arc::new(RwLock::new(HashMap::new())),
            filters: Arc::new(RwLock::new(HashMap::new())),
            timeouts: Arc::new(RwLock::new(HashMap::new())),
            defaults: Arc::new(std::sync::RwLock::new(crate::config::McpConfig::default())),
            in_flight: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            oauth: std::sync::RwLock::new(None),
            attachments: Arc::new(RwLock::new(FileStore::new())),
            status_listeners: Arc::new(Mutex::new(Vec::new())),
            next_status_id: std::sync::atomic::AtomicU64::new(1),
            ready_tx,
            ready_rx,
            initial_load_wait_expired: std::sync::atomic::AtomicBool::new(false),
            default_stdio_cwd: std::sync::RwLock::new(None),
            retry_ticker_started: std::sync::atomic::AtomicBool::new(false),
            retry_ticker_stop,
            probe_tasks,
            connect_tasks,
            client_name: Arc::new(std::sync::RwLock::new(None)),
            dispose: tokio::sync::Mutex::new(dispose),
        }
    }

    pub fn set_default_stdio_cwd(&self, cwd: impl Into<std::path::PathBuf>) {
        *self
            .default_stdio_cwd
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(cwd.into());
    }

    pub fn set_client_name(&self, name: Option<String>) {
        *self.client_name.write().unwrap_or_else(|e| e.into_inner()) =
            name.filter(|name| !name.trim().is_empty());
    }

    fn client_name(&self) -> Option<String> {
        self.client_name
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn stdio_cwd_for(&self, declared: Option<&str>) -> Option<String> {
        if let Some(cwd) = declared {
            return Some(cwd.to_string());
        }
        self.default_stdio_cwd
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().into_owned())
    }

    pub fn connect_all(
        self: &Arc<Self>,
        servers: Vec<(String, McpServerRecipe, McpServerOptions)>,
    ) -> tokio::task::JoinHandle<()> {
        self.ready_tx.send_replace(false);

        self.start_retry_ticker();
        let manager = self.clone();
        tokio::spawn(async move {
            let handles: Vec<tokio::task::JoinHandle<()>> = servers
                .into_iter()
                .map(|(name, recipe, options)| {
                    let manager = manager.clone();
                    tokio::spawn(async move {
                        let _ = manager.configure(&name, recipe, options).await;
                    })
                })
                .collect();
            {
                let mut tracked = manager
                    .connect_tasks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                tracked.extend(handles.iter().map(tokio::task::JoinHandle::abort_handle));
            }
            for handle in handles {
                let _ = handle.await;
            }
            manager.ready_tx.send_replace(true);
        })
    }

    pub async fn wait_for_initial_load(&self) {
        if *self.ready_rx.borrow() {
            return;
        }
        if self
            .initial_load_wait_expired
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let mut rx = self.ready_rx.clone();
        let ready = rx.wait_for(|ready| *ready);
        let settled = tokio::select! {
            biased;
            result = ready => result.is_ok(),
            () = tokio::time::sleep(std::time::Duration::from_millis(MCP_INITIAL_LOAD_WAIT_MS)) => false,
        };
        if !settled {
            self.initial_load_wait_expired
                .store(true, std::sync::atomic::Ordering::Relaxed);

            let entries = self.server_entries().await;
            let pending = entries
                .iter()
                .filter(|entry| entry.status == "pending")
                .count();
            tracing::info!(
                budget_ms = MCP_INITIAL_LOAD_WAIT_MS,
                pending_count = pending,
                total_count = entries.len(),
                "MCP initial load wait expired"
            );
        }
    }

    pub fn on_status_change(&self, listener: McpStatusListener) -> McpStatusSubscription {
        let id = McpStatusSubscription(
            self.next_status_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        self.status_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, listener));
        id
    }

    pub fn unsubscribe_status(&self, sub: McpStatusSubscription) -> bool {
        let mut listeners = self
            .status_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = listeners.iter().position(|(id, _)| *id == sub) {
            let _ = listeners.swap_remove(pos);
            true
        } else {
            false
        }
    }

    async fn emit_status(&self, name: &str) {
        let entry = {
            let servers = self.servers.read().await;
            let Some(state) = servers.get(name) else {
                return;
            };
            to_public_entry(name, state)
        };
        let listeners = self
            .status_listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        fan_out_status(&listeners, &entry);
    }

    pub fn set_oauth_service(&self, service: Arc<crate::mcp::oauth::McpOAuthService>) {
        *self
            .oauth
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(service);
    }

    pub async fn set_attachment_store(&self, store: FileStore) {
        *self.attachments.write().await = store;
    }

    fn oauth_service(&self) -> Option<Arc<crate::mcp::oauth::McpOAuthService>> {
        self.oauth
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    async fn oauth_clock(&self) -> i64 {
        match self.oauth_service() {
            Some(service) => service.now(),
            None => chrono::Utc::now().timestamp_millis(),
        }
    }

    async fn register_needs_auth_tool(&self, name: &str) {
        let Some((url, deferred)) =
            self.servers
                .read()
                .await
                .get(name)
                .and_then(|state| match &state.recipe {
                    McpServerRecipe::Sse { url, .. } | McpServerRecipe::Http { url, .. } => {
                        Some((url.clone(), state.deferred))
                    }
                    _ => None,
                })
        else {
            return;
        };
        if self.oauth_service().is_none() || url.is_empty() {
            return;
        }
        self.drop_server_tools(name).await;
        let qualified = qualify_mcp_tool_name(name, "authenticate");
        let description = format!(
            concat!(
                "Authenticate with MCP server \"{name}\" via OAuth.\n\n",
                "This server requires an OAuth login that has not yet been completed. ",
                "Calling this tool starts the authorization flow:\n\n",
                "  1. The tool prints an authorization URL.\n",
                "  2. **You must show that URL to the user verbatim** and ask them to open it\n",
                "     in a browser, sign in, and approve the client.\n",
                "  3. The tool blocks (up to 15 minutes) until the browser redirects back to\n",
                "     the local callback listener.\n",
                "  4. On success, the client reconnects the MCP server and the real tools\n",
                "     replace this synthetic tool.\n\n",
                "Take no arguments. Treat the URL as sensitive — do not modify it or strip\n",
                "query parameters."
            ),
            name = name
        );
        let input_schema = serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });
        self.cached_tools.write().await.insert(
            qualified.clone(),
            (
                name.to_string(),
                McpTool {
                    name: "authenticate".into(),
                    description: Some(description.clone()),
                    input_schema: input_schema.clone(),
                },
            ),
        );
        self.registry.write().await.register(ToolDefinition {
            name: qualified,
            description,
            input_schema,
            source: ToolSource::Mcp,
            disclosure: if deferred {
                ToolDisclosure::Deferred
            } else {
                ToolDisclosure::Inline
            },
        });
    }

    pub async fn authenticate_tool_description(&self, qualified: &str) -> Option<String> {
        let servers = self.servers.read().await;
        servers.iter().find_map(|(name, state)| {
            (state.status == "needs-auth"
                && crate::native::tool_naming::qualify_mcp_tool_name(name, "authenticate")
                    == qualified)
                .then(|| format!("Authenticating {name}"))
        })
    }

    pub async fn mark_needs_auth(
        &self,
        name: &str,
        error: &McpError,
        client: Option<&Arc<McpClient>>,
    ) -> bool {
        let oauth = self.oauth_service();
        let (recipe, status, connected_at_ms, explicit_oauth) = {
            let servers = self.servers.read().await;
            let Some(state) = servers.get(name) else {
                return false;
            };
            (
                state.recipe.clone(),
                state.status.clone(),
                state.connected_at_ms,
                state.oauth,
            )
        };
        if status != "connected" && status != "needs-auth" {
            return false;
        }
        if !should_mark_needs_auth(
            &recipe,
            explicit_oauth,
            oauth.is_some(),
            error.is_unauthorized(),
        ) {
            return false;
        }
        if status == "needs-auth" {
            return true;
        }
        if let Some(client) = client {
            let live = self.clients.read().await.get(name).cloned();
            if !live.is_some_and(|live| Arc::ptr_eq(&live, client)) {
                return false;
            }
        }
        let key = match &recipe {
            McpServerRecipe::Sse { url, .. } | McpServerRecipe::Http { url, .. } => {
                crate::mcp::oauth::mcp_oauth_store_key(name, url).ok()
            }
            _ => None,
        };
        let rejected = match (&key, oauth.as_ref()) {
            (Some(key), Some(service)) => service.peek_rejected_grant(key, connected_at_ms),
            _ => None,
        };
        if rejected.as_ref().is_some_and(|(_, concurrent)| *concurrent) {
            return false;
        }

        if let Some(dead) = self.clients.write().await.remove(name) {
            dead.close().await;
        }
        self.drop_server_tools(name).await;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            state.status = "needs-auth".into();
            state.error = Some(needs_auth_hint(name));
            state.raw_tools.clear();
        }
        self.register_needs_auth_tool(name).await;

        if let (Some((tokens, _)), Some(service), Some(key)) = (rejected, oauth.as_ref(), key)
            && let Err(e) = service.clear_tokens_if_current(&key, &tokens)
        {
            tracing::warn!(
                server = %name,
                reason = %e,
                "mcp oauth token invalidation failed"
            );
        }
        self.emit_status(name).await;
        true
    }

    async fn apply_oauth_header(
        &self,
        name: &str,
        url: &str,
        headers: &mut HashMap<String, String>,
    ) {
        if headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"))
        {
            return;
        }
        let Some(service) = self.oauth_service() else {
            return;
        };
        let Ok(key) = crate::mcp::oauth::mcp_oauth_store_key(name, url) else {
            return;
        };
        if let Some(token) = service.access_token(&key).await {
            headers.insert("Authorization".into(), format!("Bearer {token}"));
        }
    }

    pub async fn set_tool_filter(
        &self,
        name: &str,
        enabled: bool,
        enabled_tools: Option<Vec<String>>,
        disabled_tools: Option<Vec<String>>,
    ) {
        self.filters.write().await.insert(
            name.to_string(),
            ToolFilter {
                enabled,
                enabled_tools: enabled_tools.map(|names| names.into_iter().collect()),
                disabled_tools: disabled_tools.unwrap_or_default().into_iter().collect(),
            },
        );
    }

    pub fn set_defaults(&self, defaults: crate::config::McpConfig) {
        *self.defaults.write().unwrap_or_else(|e| e.into_inner()) = defaults;
    }

    async fn register_client(&self, mut client: McpClient) -> Result<usize, McpError> {
        let name = client.server_name().to_string();
        if let Some(timeouts) = self.timeouts.read().await.get(&name).cloned() {
            client.set_tool_timeout(timeouts.tool);
        }
        let client_arc = Arc::new(client);
        let tools = client_arc.list_tools().await?;

        for tool in &tools {
            if !tool.input_schema.is_object() {
                return Err(McpError::Protocol(format!(
                    "Invalid inputSchema for MCP tool \"{}\": schema must be a JSON object",
                    tool.name
                )));
            }
        }
        let filter = {
            let filters = self.filters.read().await;
            filters.get(&name).cloned().unwrap_or_else(ToolFilter::all)
        };

        let enabled_tool_count = tools
            .iter()
            .filter(|tool| filter.allows(&tool.name))
            .count();
        let defaults = self
            .defaults
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let threshold = parse_auto_defer_tool_threshold_env(MCP_AUTO_DEFER_TOOL_THRESHOLD_ENV)
            .or(defaults.auto_defer_tool_threshold)
            .unwrap_or(DEFAULT_AUTO_DEFER_TOOL_THRESHOLD);
        let deferred = {
            let declared = self
                .servers
                .read()
                .await
                .get(&name)
                .and_then(|state| state.deferred_declared);
            resolve_deferred(
                declared,
                enabled_tool_count,
                threshold,
                defaults.default_deferred,
            )
        };

        let compact_schemas = defaults.compact_tool_schemas.unwrap_or(false);

        self.removed_tools
            .write()
            .await
            .retain(|_, server| server != &name);
        let enabled_count = {
            let mut cached = self.cached_tools.write().await;
            let previous_names = cached
                .iter()
                .filter(|(_, (server, _))| *server == name)
                .map(|(qualified, _)| qualified.clone())
                .collect::<HashSet<_>>();

            cached.retain(|_, (server, _)| *server != name);
            let mut count = 0;
            let mut registered_names = HashSet::new();
            let mut seen_in_this_call = HashSet::new();
            let mut collisions: Vec<String> = Vec::new();
            for tool in &tools {
                if !filter.allows(&tool.name) {
                    continue;
                }
                let qualified_name = qualify_mcp_tool_name(&name, &tool.name);

                if !seen_in_this_call.insert(qualified_name.clone()) {
                    collisions.push(format!(
                        "\"{}\" -> {} (collides with a same-server tool)",
                        tool.name, qualified_name
                    ));
                    continue;
                }
                if let Some((other_server, _)) = cached.get(&qualified_name)
                    && other_server != &name
                {
                    collisions.push(format!(
                        "\"{}\" -> {} (collides with server \"{other_server}\")",
                        tool.name, qualified_name
                    ));
                    continue;
                }
                count += 1;

                cached.insert(qualified_name.clone(), (name.clone(), tool.clone()));

                let definition = ToolDefinition {
                    name: qualified_name.clone(),
                    description: tool.description.clone().unwrap_or_default(),
                    input_schema: if compact_schemas {
                        crate::mcp::schema_compaction::compact_mcp_input_schema(&tool.input_schema)
                    } else {
                        tool.input_schema.clone()
                    },
                    source: ToolSource::Mcp,
                    disclosure: if deferred {
                        ToolDisclosure::Deferred
                    } else {
                        ToolDisclosure::Inline
                    },
                };
                self.registry.write().await.register(definition);
                registered_names.insert(qualified_name);
            }
            let mut registry = self.registry.write().await;
            for stale in previous_names.difference(&registered_names) {
                registry.unregister(stale);
            }
            drop(registry);
            if !collisions.is_empty() {
                tracing::warn!(
                    server = %name,
                    collisions = %collisions.join("; "),
                    "MCP tool name collisions; the losing tools were dropped"
                );
            }
            count
        };

        if let Some(state) = self.servers.write().await.get_mut(&name) {
            state.raw_tools = tools;

            state.deferred = deferred;
        }
        self.clients.write().await.insert(name, client_arc);
        Ok(enabled_count)
    }

    pub async fn add_client(&self, client: McpClient) {
        let _ = self.register_client(client).await;
    }

    pub async fn is_removed_tool(&self, tool_name: &str) -> bool {
        self.removed_tools.read().await.contains_key(tool_name)
    }

    pub async fn handles(&self, tool_name: &str) -> bool {
        let cached = self.cached_tools.read().await;
        cached.contains_key(tool_name)
    }

    pub async fn is_tool_available(&self, tool_name: &str) -> bool {
        let clients = self.clients.read().await;
        let cached = self.cached_tools.read().await;
        let servers = self.servers.read().await;
        match cached.get(tool_name) {
            Some((server, tool)) => {
                clients
                    .get(server)
                    .is_some_and(|client| !client.is_closed())
                    || (tool.name == "authenticate"
                        && servers
                            .get(server)
                            .is_some_and(|state| state.status == "needs-auth"))
            }
            None => false,
        }
    }

    pub async fn list_tool_infos(&self) -> Vec<crate::turn_loop::types::ToolInfo> {
        let clients = self.clients.read().await;
        let cached = self.cached_tools.read().await;
        let servers = self.servers.read().await;
        let live_servers: HashSet<String> = cached
            .values()
            .filter(|(server, tool)| {
                clients
                    .get(server)
                    .is_some_and(|client| !client.is_closed())
                    || (tool.name == "authenticate"
                        && servers
                            .get(server)
                            .is_some_and(|state| state.status == "needs-auth"))
            })
            .map(|(server, _)| server.clone())
            .collect();
        self.registry
            .read()
            .await
            .list()
            .into_iter()
            .filter(|tool| {
                cached
                    .get(&tool.name)
                    .is_some_and(|(server, _)| live_servers.contains(server))
            })
            .map(|tool| ToolInfo {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect()
    }

    pub async fn list_live_tool_infos(&self) -> Vec<ToolInfo> {
        self.registry
            .read()
            .await
            .list_live()
            .into_iter()
            .map(|tool| ToolInfo {
                name: tool.name,
                description: tool.description,
                input_schema: tool.input_schema,
            })
            .collect()
    }

    pub async fn has_pending_tool(&self, name: &str) -> bool {
        self.registry.read().await.has_pending(name)
    }

    pub fn live_definitions_blocking(&self) -> Vec<ToolDefinition> {
        self.registry
            .try_read()
            .map(|registry| registry.list_live())
            .unwrap_or_default()
    }

    pub fn active_definitions_blocking(&self) -> Vec<ToolDefinition> {
        self.registry
            .try_read()
            .map(|registry| registry.list())
            .unwrap_or_default()
    }

    pub async fn deferred_tool_names(&self) -> HashSet<String> {
        let servers = self.servers.read().await;
        let cached = self.cached_tools.read().await;
        cached
            .iter()
            .filter(|(name, (server, _))| {
                name.starts_with("mcp__") && servers.get(server).is_some_and(|state| state.deferred)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub fn deferred_tool_names_blocking(&self) -> HashSet<String> {
        let servers = self.servers.try_read();
        let cached = self.cached_tools.try_read();
        if let (Ok(servers), Ok(cached)) = (servers, cached) {
            cached
                .iter()
                .filter(|(name, (server, _))| {
                    name.starts_with("mcp__") && servers.get(server).is_some_and(|s| s.deferred)
                })
                .map(|(name, _)| name.clone())
                .collect()
        } else {
            HashSet::new()
        }
    }

    pub fn deferred_tool_infos_blocking(&self) -> Vec<(String, String, serde_json::Value)> {
        let servers = self.servers.try_read();
        let cached = self.cached_tools.try_read();
        let mut infos: Vec<(String, String, serde_json::Value)> = match (servers, cached) {
            (Ok(servers), Ok(cached)) => cached
                .iter()
                .filter(|(name, (server, _))| {
                    name.starts_with("mcp__") && servers.get(server).is_some_and(|s| s.deferred)
                })
                .map(|(name, (_, tool))| {
                    (
                        name.clone(),
                        tool.description.clone().unwrap_or_default(),
                        tool.input_schema.clone(),
                    )
                })
                .collect(),
            _ => Vec::new(),
        };
        infos.sort_by(|left, right| left.0.cmp(&right.0));
        infos
    }

    pub async fn server_entries(&self) -> Vec<McpServerEntry> {
        let clients = self.clients.read().await;
        let cached = self.cached_tools.read().await;
        let servers = self.servers.read().await;

        let mut entries: Vec<McpServerEntry> = servers
            .iter()
            .map(|(name, state)| {
                let client = clients.get(name);
                let live = client.is_some_and(|client| !client.is_closed());

                let mut tools = Vec::new();
                if is_live_status(&state.status) && live {
                    for (qual_name, (srv, tool)) in cached.iter() {
                        if srv == name && qual_name.starts_with("mcp__") {
                            tools.push(McpToolSummary {
                                name: tool.name.clone(),
                                description: tool.description.clone().unwrap_or_default(),
                            });
                        }
                    }
                    tools.sort_by(|a, b| a.name.cmp(&b.name));
                }

                let (status, error) = if is_live_status(&state.status) && !live {
                    (
                        "failed".to_string(),
                        Some("server closed unexpectedly".to_string()),
                    )
                } else {
                    (
                        public_status(&state.status).to_string(),
                        state.error.clone(),
                    )
                };
                McpServerEntry {
                    name: name.clone(),
                    transport: client.map_or_else(
                        || state.recipe.transport_label(),
                        |client| client.transport_type().to_string(),
                    ),
                    status,
                    tool_count: tools.len(),
                    error,
                    next_retry_at: (state.status == "retry-waiting")
                        .then_some(state.retry_at_ms)
                        .flatten(),
                    tools,
                }
            })
            .collect();

        for (name, client) in clients.iter() {
            if servers.contains_key(name) {
                continue;
            }
            let mut tools = Vec::new();
            let (status, error) = if client.is_closed() {
                (
                    "failed".to_string(),
                    Some("server closed unexpectedly".to_string()),
                )
            } else {
                for (qual_name, (srv, tool)) in cached.iter() {
                    if srv == name && qual_name.starts_with("mcp__") {
                        tools.push(McpToolSummary {
                            name: tool.name.clone(),
                            description: tool.description.clone().unwrap_or_default(),
                        });
                    }
                }
                tools.sort_by(|a, b| a.name.cmp(&b.name));
                ("connected".to_string(), None)
            };
            entries.push(McpServerEntry {
                name: name.clone(),
                transport: client.transport_type().to_string(),
                status,
                tool_count: tools.len(),
                error,
                next_retry_at: None,
                tools,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }

    pub async fn session_warnings(&self) -> Vec<McpSessionWarning> {
        self.server_entries()
            .await
            .into_iter()
            .filter_map(|entry| match entry.status.as_str() {
                "failed" => Some(McpSessionWarning {
                    code: "mcp.server_failed".into(),
                    message: match entry.error {
                        Some(error) => format!("MCP server \"{}\" failed: {error}", entry.name),
                        None => format!("MCP server \"{}\" failed to connect", entry.name),
                    },
                    severity: "warning".into(),
                }),
                "needs-auth" => Some(McpSessionWarning {
                    code: "mcp.server_needs_auth".into(),
                    message: format!(
                        "MCP server \"{}\" needs authorization before its tools are available",
                        entry.name
                    ),
                    severity: "warning".into(),
                }),
                _ => None,
            })
            .collect()
    }

    pub async fn remove_server(&self, name: &str) -> bool {
        let removed_server = self.servers.write().await.remove(name).is_some();
        let removed_client = self.clients.write().await.remove(name);
        self.drop_server_tools(name).await;
        self.removed_tools
            .write()
            .await
            .retain(|_, server| server != name);
        self.filters.write().await.remove(name);
        self.timeouts.write().await.remove(name);
        let had_client = removed_client.is_some();
        if let Some(client) = removed_client {
            client.close().await;
        }
        removed_server || had_client
    }

    async fn drop_server_tools(&self, name: &str) {
        let removed: Vec<String> = {
            let mut cached = self.cached_tools.write().await;
            let names = cached
                .iter()
                .filter(|(_, (server, _))| server == name)
                .map(|(qualified, _)| qualified.clone())
                .collect();
            cached.retain(|_, (server, _)| server != name);
            names
        };
        let mut registry = self.registry.write().await;
        for qualified in removed {
            registry.unregister(&qualified);
        }
    }

    pub async fn mark_removed(&self, name: &str) -> bool {
        let client = self.clients.write().await.remove(name);
        let known = client.is_some() || self.servers.read().await.contains_key(name);
        if !known {
            return false;
        }
        let removed = self.cached_tools.read().await;
        let mut tombstones = self.removed_tools.write().await;
        for (qualified, (server, _)) in removed.iter() {
            if server == name {
                tombstones.insert(qualified.clone(), name.to_string());
            }
        }
        drop(tombstones);
        drop(removed);
        self.drop_server_tools(name).await;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            state.status = "removed".into();
            state.error = None;
            state.raw_tools.clear();
        }
        self.emit_status(name).await;

        if let Some(client) = client {
            client.close().await;
        }
        true
    }

    pub async fn shutdown(&self) {
        let mut dispose = self.dispose.lock().await;
        if !dispose.is_active() {
            return;
        }
        dispose.teardown(TeardownReason::ScopeClose).await;
        drop(dispose);

        {
            let servers: Vec<String> = self.servers.read().await.keys().cloned().collect();
            for server in servers {
                self.drop_server_tools(&server).await;
            }
            self.cached_tools.write().await.clear();
            self.removed_tools.write().await.clear();
        }
        self.servers.write().await.clear();
        self.filters.write().await.clear();
        self.timeouts.write().await.clear();
    }

    pub async fn inspect_server(&self, name: &str) -> Option<Vec<Value>> {
        let client = { self.clients.read().await.get(name).cloned() };
        if let Some(client) = client
            && let Ok(tools) = client.list_tools().await
        {
            return Some(tools.iter().map(tool_to_json).collect());
        }
        let servers = self.servers.read().await;
        let state = servers.get(name)?;
        Some(state.raw_tools.iter().map(tool_to_json).collect())
    }

    async fn call_authenticate_tool(
        &self,
        server_name: &str,
        cancelled: Option<&(dyn Fn() -> bool + Sync)>,
        on_update: Option<crate::tools::OutputUpdate<'_>>,
    ) -> ExecutableToolResult {
        let make_result = |content: String, is_error: bool| ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content,
            is_error,
            note: None,
            display: None,
        };
        let failure = |error: String, authorization_url: Option<&str>| {
            let suffix = authorization_url
                .map(|url| {
                    format!(
                        "\n\nAuthorization URL (still valid if the listener has not timed out): {url}"
                    )
                })
                .unwrap_or_default();
            make_result(
                format!(
                    "OAuth flow for MCP server \"{server_name}\" did not complete: {error}{suffix}"
                ),
                true,
            )
        };
        if cancelled.is_some_and(|probe| probe()) {
            return failure("aborted".into(), None);
        }
        let status = |text: &str| {
            if let Some(callback) = on_update {
                callback(&crate::tools::ToolUpdate::text("status", text));
            }
        };
        let url = self
            .servers
            .read()
            .await
            .get(server_name)
            .and_then(|state| match &state.recipe {
                McpServerRecipe::Sse { url, .. } | McpServerRecipe::Http { url, .. } => {
                    Some(url.clone())
                }
                _ => None,
            });
        let (Some(service), Some(url)) = (self.oauth_service(), url) else {
            return failure("MCP OAuth is not configured".into(), None);
        };
        status(&format!("Discovering OAuth metadata for {server_name}…"));
        let client_name = self
            .client_name()
            .unwrap_or_else(|| "Kimi Code".to_string());
        let mut flow = match service
            .begin_authorization(server_name, &url, &client_name)
            .await
        {
            Ok(flow) => flow,
            Err(crate::mcp::oauth::service::BeginAuthorizationError::AlreadyAuthorized) => {
                status(&format!("Already authorized; reconnecting {server_name}…"));
                return match self.reconnect(server_name).await {
                    Ok(()) => make_result(
                        format!(
                            "MCP server \"{server_name}\" already had valid OAuth credentials. Reconnected; real tools are available now."
                        ),
                        false,
                    ),
                    Err(error) => failure(error, None),
                };
            }
            Err(crate::mcp::oauth::service::BeginAuthorizationError::Failed(error)) => {
                return failure(error, None);
            }
        };
        let authorization_url = flow.authorization_url.clone();
        let timeout = Duration::from_secs(15 * 60);
        let custom_data = serde_json::json!({
            "waitId": flow.wait_id,
            "serverName": server_name,
            "authorizationUrl": authorization_url,
            "expiresAt": chrono::Utc::now().timestamp_millis() + timeout.as_millis() as i64,
        });
        if let Some(callback) = on_update {
            callback(&crate::tools::ToolUpdate {
                kind: "custom",
                text: "",
                percent: None,
                custom_kind: Some("mcp.oauth.authorization_url"),
                custom_data: Some(&custom_data),
                replace: false,
            });
        }
        status(&format!(
            "Open this URL in your browser to authorize \"{server_name}\":\n\n{authorization_url}\n\nWaiting for the OAuth callback (timeout 15 min). If you cancel, call this tool again to restart the flow."
        ));
        if let Err(error) = flow.complete(cancelled, timeout).await {
            return failure(error, Some(&authorization_url));
        }
        status(&format!("Authorized — reconnecting {server_name}…"));
        match self.reconnect(server_name).await {
            Ok(()) => make_result(
                format!(
                    "MCP server \"{server_name}\" authenticated successfully. The real MCP tools have replaced this synthetic authenticate tool."
                ),
                false,
            ),
            Err(error) => failure(error, None),
        }
    }

    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: &Value,
        cancelled: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Option<ExecutableToolResult> {
        self.call_tool_streaming(tool_name, arguments, cancelled, None)
            .await
    }

    pub async fn call_tool_streaming(
        &self,
        tool_name: &str,
        arguments: &Value,
        cancelled: Option<&(dyn Fn() -> bool + Sync)>,
        on_update: Option<crate::tools::OutputUpdate<'_>>,
    ) -> Option<ExecutableToolResult> {
        if self.is_removed_tool(tool_name).await {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: format!(
                    "MCP server for tool \"{tool_name}\" has been removed (plugin uninstalled or config deleted). Do not call this tool again."
                ),
                is_error: true,
                note: None,
                display: None,
            });
        }
        let (server_name, mcp_tool) = {
            let cached = self.cached_tools.read().await;
            cached.get(tool_name).cloned()?
        };

        if mcp_tool.name == "authenticate" {
            return Some(
                self.call_authenticate_tool(&server_name, cancelled, on_update)
                    .await,
            );
        }

        let client = {
            let clients = self.clients.read().await;
            clients.get(&server_name)?.clone()
        };

        match client.call_tool(&mcp_tool.name, arguments).await {
            Ok(res) => {
                let attachments = self.attachments.read().await.clone();
                let converted = crate::mcp::output::mcp_result_to_output(
                    &res,
                    tool_name,
                    Some(&attachments),
                    None,
                    cancelled,
                );
                Some(ExecutableToolResult {
                    spill: None,
                    parts: converted.parts,
                    delivery: None,
                    stop_turn: false,
                    content: converted.content,
                    is_error: res.is_error,
                    note: None,
                    display: None,
                })
            }
            Err(e) => {
                let unauthorized = self.mark_needs_auth(&server_name, &e, Some(&client)).await;
                let content = if unauthorized {
                    format!(
                        "MCP server \"{server_name}\" rejected the call with 401 Unauthorized and \
                         is now marked needs-auth. Call the {} tool to complete the OAuth login, \
                         then retry the original call.",
                        qualify_mcp_tool_name(&server_name, "authenticate")
                    )
                } else {
                    format!("MCP execution error: {e}")
                };
                Some(ExecutableToolResult {
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: false,
                    content,
                    is_error: true,
                    note: None,
                    display: None,
                })
            }
        }
    }

    pub async fn spawn_from_config(self: &Arc<Self>, config: &crate::config::KimiConfig) {
        self.set_defaults(config.mcp.clone());

        self.start_retry_ticker();
        let tasks = config
            .mcp_servers
            .iter()
            .filter_map(|(name, conf)| {
                let recipe = recipe_from_config(conf)?;
                let options = McpServerOptions {
                    enabled: conf.enabled.unwrap_or(true),
                    enabled_tools: conf.enabled_tools.clone(),
                    disabled_tools: conf.disabled_tools.clone(),
                    startup_timeout_ms: conf.startup_timeout_ms,
                    tool_timeout_ms: conf.tool_timeout_ms,

                    deferred: conf.deferred,
                    oauth: conf.auth.as_deref().map(|auth| auth == "oauth"),
                };
                Some(self.configure(name, recipe, options))
            })
            .collect::<Vec<_>>();
        futures_util::future::join_all(tasks).await;
    }

    fn retry_policy(&self) -> RetryPolicy {
        let defaults = self.defaults.read().unwrap_or_else(|e| e.into_inner());
        RetryPolicy {
            enabled: defaults.reconnect_retry_enabled.unwrap_or(true),
            ticker: self
                .retry_ticker_started
                .load(std::sync::atomic::Ordering::SeqCst),
            initial_delay_ms: parse_timeout_env(MCP_RECONNECT_INITIAL_DELAY_ENV)
                .or(defaults.reconnect_initial_delay_ms)
                .unwrap_or(DEFAULT_RECONNECT_INITIAL_DELAY_MS),
            max_delay_ms: parse_timeout_env(MCP_RECONNECT_MAX_DELAY_ENV)
                .or(defaults.reconnect_max_delay_ms)
                .unwrap_or(DEFAULT_RECONNECT_MAX_DELAY_MS),
        }
    }

    fn start_retry_ticker(self: &Arc<Self>) {
        if self
            .retry_ticker_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let manager = self.clone();
        let stop = self.retry_ticker_stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                manager.retry_due_servers().await;
            }
        });
    }

    pub async fn reschedule_retries(&self) {
        let policy = self.retry_policy();
        let now = chrono::Utc::now().timestamp_millis();
        let mut servers = self.servers.write().await;
        for state in servers.values_mut() {
            if state.status != "retry-waiting" {
                continue;
            }
            let streak = state.retry_streak.max(1);
            let delay = retry_delay_ms(streak - 1, policy.initial_delay_ms, policy.max_delay_ms);
            state.retry_at_ms = Some(now + delay as i64);
        }
    }

    async fn retry_due_servers(&self) {
        let now = chrono::Utc::now().timestamp_millis();
        let due: Vec<(String, i64)> = {
            let servers = self.servers.read().await;
            servers
                .iter()
                .filter(|(_, state)| state.status == "retry-waiting")
                .filter_map(|(name, state)| state.retry_at_ms.map(|at| (name.clone(), at)))
                .filter(|(_, at)| *at <= now)
                .collect()
        };
        for (name, due_at) in due {
            {
                let mut servers = self.servers.write().await;
                let Some(state) = servers.get_mut(&name) else {
                    continue;
                };
                if state.status != "retry-waiting" || state.retry_at_ms != Some(due_at) {
                    continue;
                }
                state.status = "pending".into();
                state.retry_at_ms = None;
            }

            let _ = self.connect_one(&name).await;
        }
    }

    pub async fn configure(
        &self,
        name: &str,
        recipe: McpServerRecipe,
        options: McpServerOptions,
    ) -> Result<(), String> {
        let defaults = self
            .defaults
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let deferred = options
            .deferred
            .or(defaults.default_deferred)
            .unwrap_or(false);
        let filter = ToolFilter {
            enabled: options.enabled,
            enabled_tools: options
                .enabled_tools
                .clone()
                .map(|names| names.into_iter().collect()),
            disabled_tools: options
                .disabled_tools
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect(),
        };
        let timeouts = McpTimeouts {
            startup: Duration::from_millis(resolve_startup_timeout(
                options.startup_timeout_ms,
                parse_timeout_env(MCP_STARTUP_TIMEOUT_ENV),
                defaults.startup_timeout_ms,
            )),
            tool: resolve_tool_timeout(
                options.tool_timeout_ms,
                parse_timeout_env(MCP_TOOL_TIMEOUT_ENV),
                defaults.tool_timeout_ms,
            )
            .map(Duration::from_millis),
        };
        let oauth = options.oauth.unwrap_or(false);

        {
            let unchanged = {
                let servers = self.servers.read().await;
                servers.get(name).is_some_and(|state| {
                    is_live_status(&state.status)
                        && state.recipe == recipe
                        && state.deferred_declared == options.deferred
                        && state.oauth == oauth
                })
            };
            if unchanged {
                let same_filter = self
                    .filters
                    .read()
                    .await
                    .get(name)
                    .is_some_and(|current| current == &filter);
                let same_timeouts = self
                    .timeouts
                    .read()
                    .await
                    .get(name)
                    .is_some_and(|current| current == &timeouts);
                if same_filter && same_timeouts {
                    return Ok(());
                }
            }
        }
        self.filters.write().await.insert(name.to_string(), filter);
        self.timeouts
            .write()
            .await
            .insert(name.to_string(), timeouts);
        self.servers.write().await.insert(
            name.to_string(),
            ServerState {
                recipe,

                status: if options.enabled {
                    "pending".into()
                } else {
                    "disabled".into()
                },
                error: None,
                raw_tools: Vec::new(),
                deferred,
                deferred_declared: options.deferred,
                connected_at_ms: None,
                oauth: options.oauth.unwrap_or(false),
                retry_at_ms: None,
                retry_streak: 0,
            },
        );
        self.emit_status(name).await;
        if !options.enabled {
            return Ok(());
        }

        self.connect_one(name).await
    }

    async fn mark_connect_failure(&self, name: &str, error: &McpError, display: &str) {
        let oauth_installed = self.oauth_service().is_some();
        let (recipe, explicit_oauth) = self
            .servers
            .read()
            .await
            .get(name)
            .map(|s| (s.recipe.clone(), s.oauth))
            .unwrap_or((McpServerRecipe::Mock, false));

        let needs_auth = should_mark_needs_auth(
            &recipe,
            explicit_oauth,
            oauth_installed,
            error.is_auth_rejection(),
        );
        let policy = self.retry_policy();
        let now = chrono::Utc::now().timestamp_millis();

        let mut suppress = false;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            let streak_before = state.retry_streak;
            state.error = Some(display.to_string());
            if needs_auth {
                state.error = Some(needs_auth_hint(name));
                state.status = "needs-auth".into();
                state.retry_at_ms = None;
                state.retry_streak = 0;
            } else if is_permanent_failure(&recipe, error) {
                state.status = "failed".into();
                state.retry_at_ms = None;
                state.retry_streak = 0;
            } else {
                let (status, retry_at, streak) =
                    next_failure_state(policy, state.retry_streak, now);
                suppress = status == "retry-waiting" && streak_before > 0;
                state.status = status.into();
                state.retry_streak = streak;
                state.retry_at_ms = retry_at;
            }
        }
        if needs_auth {
            self.register_needs_auth_tool(name).await;
        }
        if !suppress {
            self.emit_status(name).await;
        }
    }

    async fn connect_one(&self, name: &str) -> Result<(), String> {
        let recipe = {
            let servers = self.servers.read().await;
            let Some(state) = servers.get(name) else {
                return Err(format!("MCP server '{name}' is not configured"));
            };
            state.recipe.clone()
        };
        let startup = {
            let timeouts = self.timeouts.read().await;
            timeouts
                .get(name)
                .map(|t| t.startup)
                .unwrap_or_else(|| Duration::from_millis(DEFAULT_MCP_STARTUP_TIMEOUT_MS))
        };

        let mut stderr_tail = String::new();
        let connect = async {
            let client = match &recipe {
                McpServerRecipe::Sse {
                    url,
                    headers,
                    bearer_token_env_var,
                } => {
                    let mut headers =
                        resolve_bearer_headers("SSE", headers, bearer_token_env_var.as_deref())
                            .map_err(McpError::Config)?;
                    self.apply_oauth_header(name, url, &mut headers).await;
                    let client_name = self.client_name();
                    McpClient::connect_sse(name, url, headers, client_name.as_deref()).await?
                }
                McpServerRecipe::Http {
                    url,
                    headers,
                    bearer_token_env_var,
                } => {
                    let mut headers =
                        resolve_bearer_headers("HTTP", headers, bearer_token_env_var.as_deref())
                            .map_err(McpError::Config)?;
                    self.apply_oauth_header(name, url, &mut headers).await;
                    let client_name = self.client_name();
                    McpClient::connect_http(name, url, headers, client_name.as_deref()).await?
                }
                McpServerRecipe::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                } => {
                    let effective_cwd = self.stdio_cwd_for(cwd.as_deref());
                    let client_name = self.client_name();
                    McpClient::spawn_stdio(
                        name,
                        command,
                        &args.iter().map(String::as_str).collect::<Vec<_>>(),
                        env,
                        effective_cwd.as_deref(),
                        client_name.as_deref(),
                    )
                    .await?
                }
                McpServerRecipe::Mock => {
                    let client_name = self.client_name();
                    McpClient::mock(name).with_client_name(client_name.as_deref())
                }
            };
            stderr_tail = client.stderr_snapshot();

            if self
                .servers
                .read()
                .await
                .get(name)
                .is_some_and(|state| state.status == "removed")
            {
                client.close().await;
                return Ok::<ConnectOutcome, McpError>(ConnectOutcome::Removed);
            }
            self.register_client(client).await?;

            if let Some(client_arc) = self.clients.read().await.get(name).cloned() {
                self.watch_session_evidence(name, client_arc).await;
            }
            Ok(ConnectOutcome::Registered)
        };

        match tokio::time::timeout(startup, connect).await {
            Ok(Ok(ConnectOutcome::Removed)) => {
                return Err(format!("MCP server '{name}' was removed during startup"));
            }
            Ok(Ok(_)) => {
                let closed = self
                    .clients
                    .read()
                    .await
                    .get(name)
                    .is_some_and(|client| client.is_closed());
                if closed {
                    let error = McpError::closed("MCP server closed during the handshake");
                    let display = error.to_string();
                    self.mark_connect_failure(name, &error, &display).await;
                    return Err(display);
                }
            }
            Ok(Err(error)) => {
                let display = if stderr_tail.is_empty() {
                    error.to_string()
                } else {
                    format!("{error}\nstderr: {}", stderr_tail.trim_end())
                };
                self.mark_connect_failure(name, &error, &display).await;
                return Err(display);
            }
            Err(_) => {
                let display = format!("Timed out after {}ms", startup.as_millis());
                let error = McpError::timeout(display.clone());
                self.mark_connect_failure(name, &error, &display).await;
                return Err(display);
            }
        }
        let connected_at_ms = self.oauth_clock().await;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            state.status = "connected".into();
            state.error = None;
            state.connected_at_ms = Some(connected_at_ms);

            state.retry_at_ms = None;
            state.retry_streak = 0;
        }
        self.emit_status(name).await;
        Ok(())
    }

    fn evidence_context(&self, name: &str, client: Arc<McpClient>) -> EvidenceContext {
        EvidenceContext {
            name: name.to_string(),
            client,
            clients: self.clients.clone(),
            servers: self.servers.clone(),
            cached_tools: self.cached_tools.clone(),
            registry: self.registry.clone(),
            status_listeners: self.status_listeners.clone(),
            probe_tasks: self.probe_tasks.clone(),
            policy: self.retry_policy(),
        }
    }

    async fn watch_session_evidence(&self, name: &str, client: Arc<McpClient>) {
        let context = self.evidence_context(name, client.clone());

        let hard = context.clone();
        client
            .on_unexpected_close(Box::new(move |reason| {
                let hard = hard.clone();
                tokio::spawn(async move {
                    hard.fail(reason).await;
                });
            }))
            .await;

        let weak = context;
        client
            .on_weak_evidence(Box::new(move |reason| {
                let weak = weak.clone();
                let tasks = weak.probe_tasks.clone();
                let handle = tokio::spawn(async move {
                    weak.probe(reason).await;
                });

                let mut guard = tasks.lock().unwrap_or_else(|e| e.into_inner());
                guard.retain(|task| !task.is_finished());
                guard.push(handle);
            }))
            .await;
    }

    pub async fn reconnect(&self, name: &str) -> Result<(), String> {
        let _notify = {
            let mut in_flight = self.in_flight.lock().await;
            if let Some(existing) = in_flight.get(name) {
                let notify = existing.clone();
                drop(in_flight);
                notify.notified().await;
                return Ok(());
            }
            let notify = Arc::new(tokio::sync::Notify::new());
            in_flight.insert(name.to_string(), notify.clone());
            notify
        };
        let result = self.reconnect_inner(name).await;

        let mut in_flight = self.in_flight.lock().await;
        if let Some(notify) = in_flight.remove(name) {
            notify.notify_waiters();
        }
        drop(in_flight);
        result
    }

    pub async fn reconnect_after_current(&self, name: &str) -> Result<(), String> {
        let notify = {
            let in_flight = self.in_flight.lock().await;
            in_flight.get(name).cloned()
        };
        if let Some(notify) = notify {
            notify.notified().await;
        }
        self.reconnect(name).await
    }

    async fn reconnect_inner(&self, name: &str) -> Result<(), String> {
        let Some((_recipe, status)) = self
            .servers
            .read()
            .await
            .get(name)
            .map(|s| (s.recipe.clone(), s.status.clone()))
        else {
            return Err(format!("MCP server '{name}' is not configured"));
        };

        if status == "removed" {
            return Err(format!("Unknown MCP server: {name}"));
        }

        let enabled = {
            let filters = self.filters.read().await;
            filters.get(name).map(|f| f.enabled).unwrap_or(true)
        };
        if !enabled {
            return Err(format!("MCP server is disabled: {name}"));
        }

        let dead = self.clients.write().await.remove(name);
        self.drop_server_tools(name).await;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            state.status = "pending".into();
            state.error = None;
            state.raw_tools.clear();
        }
        if let Some(dead) = dead {
            dead.close().await;
        }
        self.emit_status(name).await;

        self.connect_one(name).await
    }
}

fn tool_to_json(tool: &McpTool) -> Value {
    serde_json::json!({
        "name": tool.name,
        "description": tool.description,
        "inputSchema": tool.input_schema,
    })
}

fn fan_out_status(
    listeners: &[(McpStatusSubscription, McpStatusListener)],
    entry: &McpServerEntry,
) {
    if entry.status == "failed" || entry.status == "needs-auth" {
        tracing::error!(
            server = %entry.name,
            transport = %entry.transport,
            status = %entry.status,
            reason = ?entry.error,
            "mcp server unavailable"
        );
    }
    for (_, listener) in listeners {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            listener(entry.clone());
        }));
    }
}

fn to_public_entry(name: &str, state: &ServerState) -> McpServerEntry {
    McpServerEntry {
        name: name.to_string(),
        transport: state.recipe.transport_label(),
        status: public_status(&state.status).to_string(),
        tool_count: state.raw_tools.len(),
        error: state.error.clone(),

        next_retry_at: (state.status == "retry-waiting")
            .then_some(state.retry_at_ms)
            .flatten(),
        tools: state
            .raw_tools
            .iter()
            .map(|tool| McpToolSummary {
                name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
            })
            .collect(),
    }
}

const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 5_000;

const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 300_000;

fn retry_delay_ms(streak: u32, initial: u64, max: u64) -> u64 {
    let nominal = initial
        .saturating_mul(1u64 << streak.min(32))
        .min(max)
        .max(1);
    let jitter = 0.5 + fastrand::f64() * 0.5;
    ((nominal as f64) * jitter) as u64
}

#[derive(Clone, Copy)]
struct RetryPolicy {
    enabled: bool,
    ticker: bool,
    initial_delay_ms: u64,
    max_delay_ms: u64,
}

enum ConnectOutcome {
    Registered,
    Removed,
}

fn next_failure_state(
    policy: RetryPolicy,
    streak: u32,
    now_ms: i64,
) -> (&'static str, Option<i64>, u32) {
    if !policy.enabled || !policy.ticker {
        return ("failed", None, 0);
    }
    let streak = streak.saturating_add(1).min(MAX_FAILURE_STREAK);
    let delay = retry_delay_ms(streak - 1, policy.initial_delay_ms, policy.max_delay_ms);
    ("retry-waiting", Some(now_ms + delay as i64), streak)
}

fn needs_auth_hint(name: &str) -> String {
    LocalizedText::with_params("engine.mcp.needsAuthHint", i18n_params!["name" => name]).render()
}

/// v2 `hasStaticMcpCredentials`: a remote server carrying its own bearer token or
/// headers authenticates by static credentials, so an auth rejection is
/// `auth-static` rather than `auth-oauth`.
fn has_static_credentials(recipe: &McpServerRecipe) -> bool {
    match recipe {
        McpServerRecipe::Sse {
            headers,
            bearer_token_env_var,
            ..
        }
        | McpServerRecipe::Http {
            headers,
            bearer_token_env_var,
            ..
        } => bearer_token_env_var.is_some() || !headers.is_empty(),
        _ => false,
    }
}

/// v2 `ConnectionManager.failureTarget`: `config` / `protocol` / `auth-static`
/// fail outright, an `auth-oauth` rejection without an OAuth service is retried
/// (or failed when the retry policy is off), everything else follows the error's
/// own permanence. The `needsAuth` arm is handled by [`should_mark_needs_auth`]
/// before this is consulted.
fn is_permanent_failure(recipe: &McpServerRecipe, error: &McpError) -> bool {
    if matches!(error, McpError::Config(_) | McpError::Protocol(_)) {
        return true;
    }
    if error.is_auth_rejection() {
        return has_static_credentials(recipe);
    }
    error.is_permanent()
}

fn should_mark_needs_auth(
    recipe: &McpServerRecipe,
    explicit_oauth: bool,
    oauth_installed: bool,
    unauthorized: bool,
) -> bool {
    if !oauth_installed || !unauthorized {
        return false;
    }
    let (headers, bearer_token_env_var) = match recipe {
        McpServerRecipe::Sse {
            headers,
            bearer_token_env_var,
            ..
        }
        | McpServerRecipe::Http {
            headers,
            bearer_token_env_var,
            ..
        } => (headers, bearer_token_env_var),
        _ => return false,
    };

    if bearer_token_env_var.is_some() {
        return false;
    }

    if !headers.is_empty() && !explicit_oauth {
        return false;
    }
    true
}

fn recipe_from_config(conf: &crate::config::McpServerConfig) -> Option<McpServerRecipe> {
    let transport = conf.transport.as_deref().map(str::to_ascii_lowercase);
    let stdio = || {
        conf.command.as_ref().map(|cmd| McpServerRecipe::Stdio {
            command: cmd.clone(),
            args: conf.args.clone().unwrap_or_default(),
            env: conf.env.clone().unwrap_or_default(),
            cwd: conf.cwd.clone(),
        })
    };
    let remote = |build: fn(String, HashMap<String, String>, Option<String>) -> McpServerRecipe| {
        conf.url.as_ref().map(|url| {
            build(
                url.clone(),
                conf.headers.clone().unwrap_or_default(),
                conf.bearer_token_env_var.clone(),
            )
        })
    };
    match transport.as_deref() {
        Some("stdio") => stdio(),
        Some("sse") => remote(|url, headers, bearer_token_env_var| McpServerRecipe::Sse {
            url,
            headers,
            bearer_token_env_var,
        }),
        Some("http") => remote(|url, headers, bearer_token_env_var| McpServerRecipe::Http {
            url,
            headers,
            bearer_token_env_var,
        }),
        Some(_) => None,
        None => {
            if conf.url.is_some() {
                remote(|url, headers, bearer_token_env_var| McpServerRecipe::Http {
                    url,
                    headers,
                    bearer_token_env_var,
                })
            } else {
                stdio()
            }
        }
    }
}

fn resolve_bearer_headers(
    transport: &str,
    headers: &HashMap<String, String>,
    env_var: Option<&str>,
) -> Result<HashMap<String, String>, String> {
    let mut headers = headers.clone();
    let Some(var) = env_var else {
        return Ok(headers);
    };
    let token = std::env::var(var).ok().filter(|token| !token.is_empty());
    let Some(token) = token else {
        return Err(format!(
            "MCP {transport} bearer token env var \"{var}\" is not set or is empty"
        ));
    };
    headers.retain(|key, _| !key.eq_ignore_ascii_case("authorization"));
    headers.insert("Authorization".into(), format!("Bearer {token}"));
    Ok(headers)
}

#[cfg(test)]
mod tests;
