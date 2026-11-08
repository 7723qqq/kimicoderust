//! Dynamic MCP server manager and tool registry.

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
use crate::server::files::FileStore;
use crate::tools::registry::{ToolDefinition, ToolDisclosure, ToolRegistry, ToolSource};
use crate::turn_loop::types::{ExecutableToolResult, ToolDelivery, ToolInfo};

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
    /// When the next automatic retry is due, epoch ms (v2
    /// `McpServerEntry.nextRetryAt`, connection-manager.ts:39). Only set while
    /// a retry is scheduled; a connected entry clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<i64>,
    pub tools: Vec<McpToolSummary>,
}

/// v2 `publicStatusByState` (connection-manager.ts:966-975): the internal
/// `retryWaiting` state is surfaced as `failed` plus `nextRetryAt`, and
/// `probing` is surfaced as `connected`. The two extra internal states are
/// scheduling details, not a wider public vocabulary.
fn public_status(internal: &str) -> &str {
    match internal {
        "retry-waiting" => "failed",
        "probing" => "connected",
        other => other,
    }
}

/// The internal states whose tools are live: `probing` keeps its client,
/// tools and streak while the liveness probe runs (v2 `isLiveState`,
/// connection-manager.ts:962-964).
fn is_live_status(internal: &str) -> bool {
    matches!(internal, "connected" | "probing")
}

/// What a session-evidence callback needs, cloned out of the manager so the
/// callback can outlive the borrow that registered it (the manager's own
/// closure, in v2 terms).
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
    /// v2 `isCurrent`: a callback for a client the entry no longer holds
    /// (reconnect, removal, shutdown) is ignored.
    async fn is_current(&self) -> bool {
        let clients = self.clients.read().await;
        matches!(clients.get(&self.name), Some(client) if Arc::ptr_eq(client, &self.client))
    }

    /// Fail the entry on hard evidence or a failed probe (v2
    /// `sessionEvidence` hard / `probeFailed`): leave the live family, drop the
    /// tools, and let the retry policy decide between a scheduled retry and
    /// `failed`.
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
            // Only a live entry flips; a stale close event for an entry that
            // already retried (or was removed) must not bump the streak again.
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

    /// v2 `sessionEvidence` with `{ kind: 'weak' }`: park the entry in
    /// `probing` and ask the server whether the session is still there.
    ///
    /// A ping that answers restores `connected` with the client, tools and
    /// streak untouched and emits nothing — the public entry never changed
    /// (`probing` maps to `connected`). A ping that fails is a dead session and
    /// takes the hard path. Weak evidence that arrives while a probe is already
    /// running is ignored, exactly as v2's `probing` arm ignores it.
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

/// v2 `formatUnexpectedCloseError` (connection-manager.ts:1057-1066): the
/// entry-level message names the server, then the transport's own reason.
fn format_unexpected_close_error(name: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("MCP server \"{name}\" closed unexpectedly")
    } else {
        format!("MCP server \"{name}\" closed unexpectedly\n{detail}")
    }
}

/// One thing a session should tell the user at startup. The wire shape the
/// host renders (`{ code, message, severity }`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpSessionWarning {
    pub code: String,
    pub message: String,
    /// `"warning"` — a degraded session, not a failed one. The host maps
    /// anything that is not `"error"` to a warning.
    pub severity: String,
}

/// The spawn recipe for a configured server, kept so `reconnect` can
/// re-derive the client (the TS connection-manager keeps `entry.config` for
/// the same reason, connection-manager.ts:33-41).
#[derive(Debug, Clone, PartialEq)]
pub enum McpServerRecipe {
    Sse {
        url: String,
        headers: HashMap<String, String>,
        bearer_token_env_var: Option<String>,
    },
    /// Streamable HTTP (`transport = "http"`), the v2 default for a bare url.
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
    /// In-process stub used by the napi binding path and tests.
    Mock,
}

impl McpServerRecipe {
    /// The wire transport label surfaced on `McpServerEntry.transport`
    /// (v2 `McpServerConfig['transport']`).
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
    /// "connected" | "failed" | "pending" | "disabled" (v2 `McpServerStatus`
    /// subset; `disabled` covers `config.enabled === false`).
    status: String,
    error: Option<String>,
    /// Every tool the server advertised (v2 `rawTools`), including the ones
    /// the filter hides from the model; `/mcp inspect` reads this.
    raw_tools: Vec<McpTool>,
    /// Whether this server's tools are disclosed on demand rather than in the
    /// top-level tool list (v2 `disclosure: 'deferred'`).
    deferred: bool,
    /// When the live connection was established (v2 `entry.connectedAt`,
    /// connection-manager.ts:37). Compared against a stored grant's obtain
    /// time to tell a concurrent login from the credential a 401 rejected.
    connected_at_ms: Option<i64>,
    /// The user's explicit `auth = "oauth"` opt-in (v2 `config.auth`), which
    /// only affects the 401 → `needs-auth` decision.
    oauth: bool,
    /// When the next automatic startup retry is due, epoch ms (v2
    /// `retryWaiting` + `nextRetryDueAt`). `None` means no retry scheduled.
    retry_at_ms: Option<i64>,
    /// Consecutive failed startup attempts, the exponent in the backoff
    /// (v2 `retryStreak`). Reset by a successful connect.
    retry_streak: u32,
}

/// Per-server tool visibility (v2 `computeEnabledNames` plus `config.enabled`).
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

/// v2 `DEFAULT_STARTUP_TIMEOUT_MS` (connection-manager.ts:65).
pub const DEFAULT_MCP_STARTUP_TIMEOUT_MS: u64 = 30_000;
/// v2 env bindings (configSection.ts:16-17).
const MCP_STARTUP_TIMEOUT_ENV: &str = "KIMI_MCP_STARTUP_TIMEOUT_MS";
const MCP_TOOL_TIMEOUT_ENV: &str = "KIMI_MCP_TOOL_TIMEOUT_MS";
/// v2 `MAX_MCP_TIMEOUT_MS` (config-schema.ts:5).
const MAX_MCP_TIMEOUT_MS: u64 = 2_147_483_647;

/// A status-change subscription token (v2 `onStatusChange`'s unsubscribe
/// closure, connection-manager.ts:360-378).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct McpStatusSubscription(u64);

/// Listener invoked with the public entry whenever a server's status changes.
pub type McpStatusListener = Box<dyn Fn(McpServerEntry) + Send + Sync>;

/// Per-server registration options: visibility, enabled flag and timeouts.
#[derive(Debug, Clone)]
pub struct McpServerOptions {
    pub enabled: bool,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Option<Vec<String>>,
    pub startup_timeout_ms: Option<u64>,
    pub tool_timeout_ms: Option<u64>,
    /// Keep this server's tools out of the top-level tool list and load them on
    /// demand through `select_tools` (v2 per-server `deferred`). `None` means
    /// "not declared": the `[mcp] defaultDeferred` section value applies at
    /// [`McpManager::configure`] time (v2 #4210). The caller applies the gate —
    /// the `tool_select` flag plus the model's `dynamically_loaded_tools`
    /// capability — before setting it; the manager only records the disclosure
    /// this server was registered with.
    pub deferred: Option<bool>,
    /// The server's explicit `auth = "oauth"` opt-in (v2 `config.auth`).
    ///
    /// It only changes one decision: a 401 on a remote server that also
    /// carries extra headers keeps flipping to `needs-auth` when the user asked
    /// for OAuth, instead of being reported as a failed server with a bad
    /// header (v2 `shouldMarkNeedsAuth`: `headers !== undefined && auth !==
    /// 'oauth'`).
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

/// Resolved timeouts for one server (v2 `startupTimeoutMs` wrapping
/// connect + discovery, `toolCallTimeoutMs` for a single request).
#[derive(Debug, Clone, PartialEq)]
struct McpTimeouts {
    startup: Duration,
    tool: Option<Duration>,
}

/// v2 parses the env value as an integer in `[1, MAX_MCP_TIMEOUT_MS]` and
/// ignores anything else (configSection.ts:19-24).
fn parse_timeout_env(name: &str) -> Option<u64> {
    let raw = std::env::var(name).ok()?;
    let parsed: u64 = raw.trim().parse().ok()?;
    (1..=MAX_MCP_TIMEOUT_MS).contains(&parsed).then_some(parsed)
}

/// v2 precedence: per-server config → env → `[mcp]` section → 30s default
/// (connection-manager.ts:310-314). The env value is passed in so resolution
/// stays testable without mutating process state.
fn resolve_startup_timeout(per_server: Option<u64>, env: Option<u64>, global: Option<u64>) -> u64 {
    per_server
        .or(env)
        .or(global)
        .unwrap_or(DEFAULT_MCP_STARTUP_TIMEOUT_MS)
}

/// v2 precedence for tool calls; `None` keeps the client built-in default
/// (connection-manager.ts:389-390).
fn resolve_tool_timeout(
    per_server: Option<u64>,
    env: Option<u64>,
    global: Option<u64>,
) -> Option<u64> {
    per_server.or(env).or(global)
}

pub struct McpManager {
    clients: Arc<RwLock<HashMap<String, Arc<McpClient>>>>,
    cached_tools: Arc<RwLock<HashMap<String, (String, McpTool)>>>,
    /// v2 #4719 / #4733: active tool definitions for this session and the
    /// pending same-name replacements a mid-session MCP update registered.
    registry: Arc<RwLock<ToolRegistry>>,
    servers: Arc<RwLock<HashMap<String, ServerState>>>,
    filters: Arc<RwLock<HashMap<String, ToolFilter>>>,
    timeouts: Arc<RwLock<HashMap<String, McpTimeouts>>>,
    /// Global defaults from the `[mcp]` config section (v2
    /// `resolveDefaultTimeouts`).
    defaults: Arc<std::sync::RwLock<crate::config::McpConfig>>,
    /// In-flight reconnects (v2 `inFlightReconnects`): late callers join the
    /// running one via its Notify instead of double-spawning.
    in_flight: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
    /// OAuth credentials for remote servers (v2 `McpOAuthService`).
    oauth: std::sync::RwLock<Option<Arc<crate::mcp::oauth::McpOAuthService>>>,
    /// Where media an MCP server returns is preserved (v2
    /// `McpConnectionManagerOptions.attachmentStore`, upstream #3688). The
    /// default store is the daemon's own blob store, which is what the request
    /// media resolver reads a `kimi-file://` reference back from.
    attachments: Arc<RwLock<FileStore>>,
    /// Status-change listeners (v2 `listeners`, connection-manager.ts:360-378).
    status_listeners: Arc<Mutex<Vec<(McpStatusSubscription, McpStatusListener)>>>,
    next_status_id: std::sync::atomic::AtomicU64,
    /// Initial-load readiness (v2 `connectAll` / `waitForInitialLoad`,
    /// connection-manager.ts:170-181, :235-241). Starts `true` — "nothing to
    /// wait for" — because a manager built by the server/REPL path connects
    /// through [`Self::spawn_from_config`], which awaits its own connects, and
    /// one built by `add_client`/`configure` has no initial load at all.
    /// [`Self::connect_all`] clears it, then sets it once the connects settle.
    ///
    /// `watch` rather than `Notify`: a notify that lands before the wait would
    /// be lost, and the wait is usually entered after the load already ended.
    ready_tx: tokio::sync::watch::Sender<bool>,
    ready_rx: tokio::sync::watch::Receiver<bool>,
    /// Working directory for a stdio server whose config omits `cwd` (v2
    /// `McpConnectionManagerOptions.stdioCwd`, which the host sets to the
    /// workspace cwd). Without it a server resolving a relative path would
    /// run in whatever directory the engine process happened to start in.
    default_stdio_cwd: std::sync::RwLock<Option<std::path::PathBuf>>,
    /// Whether the startup-retry ticker has been started (one per manager).
    retry_ticker_started: std::sync::atomic::AtomicBool,
    /// Cleared by [`Self::shutdown`], which ends the ticker.
    retry_ticker_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Probe tasks started by weak session evidence (v2's per-entry
    /// `operationTails`). [`Self::shutdown`] waits for them so a probe cannot
    /// rewrite an entry after teardown.
    probe_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Startup tasks [`Self::connect_all`] spawned, by abort handle.
    /// [`Self::shutdown`] aborts them: an aborted connect drops the child it
    /// was handshaking with (`kill_on_drop`), instead of leaving the process —
    /// and its working directory — alive until the startup timeout.
    connect_tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    /// The identity every client announces in `initialize.clientInfo.name`
    /// (v2 `resolveClientName`, wired from the host identity in
    /// `workspaceMcpService.ts:42`). `None` announces the builtin
    /// `kimi-code`.
    client_name: Arc<std::sync::RwLock<Option<String>>>,
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

impl McpManager {
    pub fn new() -> Self {
        let (ready_tx, ready_rx) = tokio::sync::watch::channel(true);
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            cached_tools: Arc::new(RwLock::new(HashMap::new())),
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
            default_stdio_cwd: std::sync::RwLock::new(None),
            retry_ticker_started: std::sync::atomic::AtomicBool::new(false),
            retry_ticker_stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            probe_tasks: Arc::new(Mutex::new(Vec::new())),
            connect_tasks: Arc::new(Mutex::new(Vec::new())),
            client_name: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// Set the working directory for stdio servers whose config omits `cwd`
    /// (v2 `stdioCwd`, which the workspace-backed host sets to the workspace
    /// cwd). Call before connecting; a later change only affects servers that
    /// connect afterwards.
    pub fn set_default_stdio_cwd(&self, cwd: impl Into<std::path::PathBuf>) {
        *self
            .default_stdio_cwd
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(cwd.into());
    }

    /// The identity every client announces as `clientInfo.name` (v2
    /// `resolveClientName`). Call before connecting; `None` announces the
    /// builtin `kimi-code`.
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

    /// The cwd a stdio server runs in: its own `cwd`, else the manager's
    /// default. `None` leaves the child in the engine process's directory.
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

    /// Start connecting every server and return immediately (v2 `connectAll`,
    /// connection-manager.ts:170-181). The returned handle settles when every
    /// connect has, and flips [`Self::wait_for_initial_load`] back to ready.
    ///
    /// Session creation uses this instead of awaiting each `configure`: the
    /// tool table reads the manager live (`callbacks.rs::list_tools`), so a
    /// server that connects later still contributes its tools.
    pub fn connect_all(
        self: &Arc<Self>,
        servers: Vec<(String, McpServerRecipe, McpServerOptions)>,
    ) -> tokio::task::JoinHandle<()> {
        // Cleared before the spawn so a consumer that reads readiness between
        // the two cannot observe "ready" while the connects are still running.
        self.ready_tx.send_replace(false);
        // The same retry scheduler `spawn_from_config` starts: a failure that
        // lands `retry-waiting` needs something to act on its due time.
        self.start_retry_ticker();
        let manager = self.clone();
        tokio::spawn(async move {
            // One task per server, so [`Self::shutdown`] can abort a startup
            // that is still handshaking: an aborted future drops the spawned
            // child (`kill_on_drop`), which is what stops a server the
            // teardown never saw register itself (v2 `shutdown` cancels
            // in-flight startup, connection-manager.test.ts "serializes
            // replacement close and tracks disabled or removed clients
            // through shutdown").
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

    /// Await the initial load started by [`Self::connect_all`] (v2
    /// `waitForInitialLoad`, connection-manager.ts:235-241). Returns at once
    /// when the load already finished, and when none was ever started.
    pub async fn wait_for_initial_load(&self) {
        if *self.ready_rx.borrow() {
            return;
        }
        let mut rx = self.ready_rx.clone();
        let _ = rx.wait_for(|ready| *ready).await;
    }

    /// Register a status-change listener (v2 `onStatusChange`,
    /// connection-manager.ts:360-378). The listener is invoked with the public
    /// entry whenever a server's status changes; drop the returned token to
    /// unsubscribe.
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

    /// Remove a status-change listener by token.
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

    /// Fan the current public entry for `name` out to every status listener
    /// (v2 `emit`, connection-manager.ts:360-378). No-op when the server is
    /// unknown. Failed / needs-auth transitions are logged, and a panicking
    /// listener must not break the connection manager (v2 wraps listener
    /// calls in try/catch).
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

    /// Install the OAuth credential service used for remote servers
    /// (v2 `McpConnectionManagerOptions.oauthService`).
    pub fn set_oauth_service(&self, service: Arc<crate::mcp::oauth::McpOAuthService>) {
        *self
            .oauth
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(service);
    }

    /// Point attachment preservation at an explicit store (tests, or a host
    /// that keeps session blobs somewhere else).
    pub async fn set_attachment_store(&self, store: FileStore) {
        *self.attachments.write().await = store;
    }

    fn oauth_service(&self) -> Option<Arc<crate::mcp::oauth::McpOAuthService>> {
        self.oauth
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The clock status transitions compare against: the OAuth service's when
    /// one is installed, system time otherwise (v2
    /// `this.oauthService?.now() ?? Date.now()`, connection-manager.ts:382).
    async fn oauth_clock(&self) -> i64 {
        match self.oauth_service() {
            Some(service) => service.now(),
            None => chrono::Utc::now().timestamp_millis(),
        }
    }

    /// Flip a server to `needs-auth` after a *call* was rejected with 401 and
    /// report whether the entry moved (v2 `markNeedsAuth`,
    /// connection-manager.ts:307-347). A live entry only reaches this from
    /// `connected` — a call cannot be made against anything else — but
    /// `needs-auth` is accepted so a second rejected call is idempotent.
    ///
    /// `client` binds the report to the connection it came from: a call that
    /// failed on a client that has since been replaced must not flip the live
    /// entry (v2 `entry.client !== client`). A grant obtained within the
    /// concurrency window is treated as a login that landed alongside this
    /// connection, so it neither flips the entry nor gets invalidated.
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
        // Close the live client and drop its tools before flipping, so the
        // entry cannot keep serving a tool list the model can no longer call.
        if let Some(dead) = self.clients.write().await.remove(name) {
            dead.close().await;
        }
        self.drop_server_tools(name).await;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            state.status = "needs-auth".into();
            state.error = Some(needs_auth_hint(name));
            state.raw_tools.clear();
        }
        // The rejected credential must not be replayed on the next connect,
        // but only while it is still the one that failed: a login that landed
        // in between owns the store now. Cleanup failures are non-fatal — the
        // entry is already needs-auth either way.
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

    /// Inject `Authorization: Bearer <token>` from the OAuth store when the
    /// server carries no static token (v2 `resolveOAuthProvider`).
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

    /// Register (or replace) the tool-visibility filter of a server. The
    /// config path calls this from `spawn_from_config`; the napi binding path
    /// calls it directly before `add_client`.
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

    /// Set the global `[mcp]` defaults that per-server timeouts and the
    /// disclosure default fall back to. Sync on purpose: the napi path installs
    /// them while building the manager, before `connect_all` spawns its task.
    pub fn set_defaults(&self, defaults: crate::config::McpConfig) {
        *self.defaults.write().unwrap_or_else(|e| e.into_inner()) = defaults;
    }

    /// Register an initialized MCP client, indexing only the tools its filter
    /// allows (v2 `computeEnabledNames` → `resolved().tools`) while keeping
    /// the full advertised list for inspection (v2 `rawTools`). Discovery
    /// failures surface to the caller so the connect path can mark the entry
    /// `failed` instead of `connected` with zero tools.
    async fn register_client(&self, mut client: McpClient) -> Result<usize, McpError> {
        let name = client.server_name().to_string();
        if let Some(timeouts) = self.timeouts.read().await.get(&name).cloned() {
            client.set_tool_timeout(timeouts.tool);
        }
        let client_arc = Arc::new(client);
        let tools = client_arc.list_tools().await?;
        // A tool whose inputSchema is not a JSON object fails the whole
        // server (v2 `assertMcpInputSchema`, mcpCore/types.ts:44-55).
        for tool in &tools {
            if !tool.input_schema.is_object() {
                // A protocol violation, not a transport blip: v2 classifies it
                // `protocol`, which the startup retry treats as permanent.
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

        let enabled_count = {
            let mut cached = self.cached_tools.write().await;
            // Re-registering a server replaces its previous tool set.
            cached.retain(|_, (server, _)| *server != name);
            let mut count = 0;
            let mut seen_in_this_call = HashSet::new();
            let mut collisions: Vec<String> = Vec::new();
            for tool in &tools {
                if !filter.allows(&tool.name) {
                    continue;
                }
                let qualified_name = qualify_mcp_tool_name(&name, &tool.name);
                // v2 `registerMcpServer` collision detection
                // (mcpService.ts:186-215): a qualified name that duplicates
                // another tool of this server, or one already registered by
                // another server, is dropped and reported.
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
                // Only the qualified name is indexed: v2 registers the
                // `mcp__<server>__<tool>` form exclusively, and a plain-name
                // alias across servers was last-writer-wins — the model could
                // have its call routed to the wrong server. The model tool
                // table only ever advertises qualified names.
                cached.insert(qualified_name.clone(), (name.clone(), tool.clone()));
                // v2 #4719: register into the session's active/pending
                // registry. A changed same-name definition stays pending so
                // `list_tool_infos` keeps serving the pinned schema.
                let deferred = self
                    .servers
                    .read()
                    .await
                    .get(&name)
                    .is_some_and(|state| state.deferred);
                let definition = ToolDefinition {
                    name: qualified_name,
                    description: tool.description.clone().unwrap_or_default(),
                    input_schema: crate::mcp::schema_compaction::compact_mcp_input_schema(
                        &tool.input_schema,
                    ),
                    source: ToolSource::Mcp,
                    disclosure: if deferred {
                        ToolDisclosure::Deferred
                    } else {
                        ToolDisclosure::Inline
                    },
                };
                self.registry.write().await.register(definition);
            }
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
        }
        self.clients.write().await.insert(name, client_arc);
        Ok(enabled_count)
    }

    /// Register an initialized MCP client without failing the caller on a
    /// discovery error (direct callers: REST probes and tests).
    pub async fn add_client(&self, client: McpClient) {
        let _ = self.register_client(client).await;
    }

    /// Check if a tool name belongs to any registered MCP server.
    pub async fn handles(&self, tool_name: &str) -> bool {
        let cached = self.cached_tools.read().await;
        cached.contains_key(tool_name)
    }

    /// Whether a discovered tool is currently callable: its server's client is
    /// alive. [`Self::handles`] answers "was ever discovered" — a closed
    /// client still matches — so it cannot tell a live tool from one whose
    /// server went away. This applies the same rule [`Self::list_tool_infos`]
    /// uses when it withholds a dead server's tools, without building their
    /// definitions for a name-only answer.
    pub async fn is_tool_available(&self, tool_name: &str) -> bool {
        let clients = self.clients.read().await;
        let cached = self.cached_tools.read().await;
        match cached.get(tool_name) {
            Some((server, _)) => clients
                .get(server)
                .is_some_and(|client| !client.is_closed()),
            None => false,
        }
    }

    /// All discovered MCP tools as engine tool definitions (for the model).
    /// Tools of a server whose connection died are withheld (v2 `resolved()`
    /// returns `undefined` unless the entry is `connected`,
    /// connection-manager.ts:142-167).
    pub async fn list_tool_infos(&self) -> Vec<crate::turn_loop::types::ToolInfo> {
        let clients = self.clients.read().await;
        let cached = self.cached_tools.read().await;
        let live_servers: HashSet<String> = cached
            .values()
            .filter(|(server, _)| {
                clients
                    .get(server)
                    .is_some_and(|client| !client.is_closed())
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

    /// What a freshly constructed session would advertise: the active table
    /// with pending same-name replacements applied (v2 `listLive`).
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

    /// Whether a same-name MCP update is pinned as pending for this session.
    pub async fn has_pending_tool(&self, name: &str) -> bool {
        self.registry.read().await.has_pending(name)
    }

    /// The live tool definitions (active + pending) for the disclosure
    /// reconciler. `try_read` keeps the synchronous tool-table path from
    /// awaiting a contended registry write; a miss yields an empty snapshot,
    /// which the next turn's reconcile sees as a full re-announcement.
    pub fn live_definitions_blocking(&self) -> Vec<ToolDefinition> {
        self.registry
            .try_read()
            .map(|registry| registry.list_live())
            .unwrap_or_default()
    }

    /// The definitions this session keeps pinned (v2 `registry.list()`).
    pub fn active_definitions_blocking(&self) -> Vec<ToolDefinition> {
        self.registry
            .try_read()
            .map(|registry| registry.list())
            .unwrap_or_default()
    }

    /// The names of the tools whose server was registered as deferred (v2
    /// `disclosure: 'deferred'`). The top-level tool table drops these and
    /// `select_tools` offers them instead, so a deferred server's schemas stop
    /// occupying context until the model asks for them.
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

    /// Blocking variant of [`Self::deferred_tool_names`] for the tool-table
    /// shaping path, which runs both on and off the async runtime (tests,
    /// the sync `shape_tool_table` builder). `try_read` on a read-only
    /// cache snapshot: contended writes are rare and a miss degrades to
    /// "no deferred tools", the pre-disclosure behaviour.
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

    /// The deferred tools with the descriptions `select_tools`' capability
    /// search ranks (v2 `loadableEntries`): the same set
    /// [`Self::deferred_tool_names_blocking`] returns, plus what the model
    /// needs to choose between them. Sorted by name so a blank query lists a
    /// deterministic catalogue.
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

    /// Return the list of MCP servers, their transport types and discovered
    /// tools.
    ///
    /// The entry's state machine is the single source of truth (v2
    /// `resolved()`): a live client only contributes its transport and tool
    /// list. Deriving the status from the client map instead made an entry the
    /// state machine had already moved on from — `retry-waiting`, `needs-auth`
    /// — read back as `connected` just because the old client object was still
    /// in the map.
    pub async fn server_entries(&self) -> Vec<McpServerEntry> {
        let clients = self.clients.read().await;
        let cached = self.cached_tools.read().await;
        let servers = self.servers.read().await;

        let mut entries: Vec<McpServerEntry> = servers
            .iter()
            .map(|(name, state)| {
                let client = clients.get(name);
                let live = client.is_some_and(|client| !client.is_closed());
                // `toolCount` is 0 unless the entry is connected
                // (connection-manager.ts:507-518).
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
                // A closed client under a `connected` entry died mid-session;
                // `watch_unexpected_close` rewrites the state shortly after.
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
        // A client registered without `configure` (`add_client`, the REST
        // probe path and tests) has no state entry; it still belongs in the
        // roster, and its liveness is the only status available for it.
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

    /// The startup warnings this roster implies: a server the engine could not
    /// connect, or one waiting on the user's authorization. A healthy roster
    /// produces none, and a disabled server is not a warning — the user turned
    /// it off on purpose.
    ///
    /// This is what a session surfaces once at startup, so a user whose MCP
    /// tools are missing learns why instead of finding an empty tool list.
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

    /// Remove a registered or connected MCP server.
    pub async fn remove_server(&self, name: &str) -> bool {
        // Take everything out under the locks first, then close the client
        // without holding them: a kill must not freeze the whole manager while
        // other tasks wait on the client/server maps.
        let removed_server = self.servers.write().await.remove(name).is_some();
        let removed_client = self.clients.write().await.remove(name);
        self.drop_server_tools(name).await;
        self.filters.write().await.remove(name);
        self.timeouts.write().await.remove(name);
        let had_client = removed_client.is_some();
        if let Some(client) = removed_client {
            client.close().await;
        }
        removed_server || had_client
    }

    /// Drop one server's cached tools and the matching registry entries.
    /// v2 `unregisterMcpServer`: both halves must agree or a removed server
    /// would keep contributing a pinned definition to a later request.
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

    /// Close a server but keep its entry as `removed` (v2 `markRemoved`,
    /// connection-manager.ts:221-232): the tools disappear while the name
    /// stays visible so callers can report that the server was removed.
    pub async fn mark_removed(&self, name: &str) -> bool {
        let client = self.clients.write().await.remove(name);
        let known = client.is_some() || self.servers.read().await.contains_key(name);
        if !known {
            return false;
        }
        self.drop_server_tools(name).await;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            state.status = "removed".into();
            state.error = None;
            state.raw_tools.clear();
        }
        self.emit_status(name).await;
        // Close after the state flip and without holding any map lock.
        if let Some(client) = client {
            client.close().await;
        }
        true
    }

    /// Close every client and forget all entries (v2 `shutdown`).
    pub async fn shutdown(&self) {
        // Stop the startup-retry ticker: a retry that fires during teardown
        // would reconnect a server the caller is closing.
        self.retry_ticker_stop
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // Cancel startups that are still handshaking before anything else: an
        // in-flight connect is not in `clients` yet, so draining `clients`
        // below would miss its child — the process (and the plugin directory
        // it runs in) would outlive the teardown until the startup timeout.
        // Aborting the task drops the future, and `kill_on_drop` takes the
        // child with it (v2 cancels in-flight startup in `shutdown`).
        let startups: Vec<tokio::task::AbortHandle> = {
            let mut tasks = self.connect_tasks.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *tasks)
        };
        for task in startups {
            task.abort();
        }
        // Drain in-flight liveness probes before clearing the entries: v2's
        // `shutdown` awaits the per-entry operation tail, so a probe still
        // running during teardown is observed rather than orphaned. A probe for
        // a client this teardown replaces sees `isCurrent` fail and does
        // nothing.
        let probes: Vec<tokio::task::JoinHandle<()>> = {
            let mut tasks = self.probe_tasks.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *tasks)
        };
        for probe in probes {
            let _ = probe.await;
        }
        let clients: Vec<Arc<McpClient>> = {
            let mut guard = self.clients.write().await;
            guard.drain().map(|(_, client)| client).collect()
        };
        for client in clients {
            client.close().await;
        }
        {
            let servers: Vec<String> = self.servers.read().await.keys().cloned().collect();
            for server in servers {
                self.drop_server_tools(&server).await;
            }
            self.cached_tools.write().await.clear();
        }
        self.servers.write().await.clear();
        self.filters.write().await.clear();
        self.timeouts.write().await.clear();
    }

    /// Inspect tools provided by a specific MCP server. Prefers a live
    /// `tools/list` round-trip and falls back to the tools discovered at
    /// registration time, so a failed server still reports what it advertised.
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

    /// Call an MCP tool dynamically.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: &Value,
        cancelled: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Option<ExecutableToolResult> {
        let (server_name, mcp_tool) = {
            let cached = self.cached_tools.read().await;
            cached.get(tool_name).cloned()?
        };

        let client = {
            let clients = self.clients.read().await;
            clients.get(&server_name)?.clone()
        };

        match client.call_tool(&mcp_tool.name, arguments).await {
            Ok(res) => {
                // Media is preserved and handed to the model as a reference the
                // request resolver turns into whatever the active model can
                // take; an original the model never sees is still retrievable
                // (v2 `mcpResultToExecutableOutput`, upstream #3688).
                let attachments = self.attachments.read().await.clone();
                let converted = crate::mcp::output::mcp_result_to_output(
                    &res,
                    Some(&attachments),
                    None,
                    cancelled,
                );
                let mut text_parts = vec![converted.content];
                if res.structured_content.is_some() || res.meta.is_some() {
                    let mut extras = serde_json::Map::new();
                    if let Some(sc) = res.structured_content {
                        extras.insert("structuredContent".to_string(), sc);
                    }
                    if let Some(m) = res.meta {
                        extras.insert("_meta".to_string(), m);
                    }
                    let extras_json =
                        serde_json::to_string_pretty(&Value::Object(extras)).unwrap_or_default();
                    text_parts.push(format!(
                        "<mcp-result-extras>\n{}\n</mcp-result-extras>",
                        extras_json
                    ));
                }
                Some(ExecutableToolResult {
                    delivery: (!converted.delivery.is_empty()).then_some(ToolDelivery {
                        blocks: converted.delivery,
                        origin: None,
                    }),
                    stop_turn: false,
                    content: text_parts.join("\n"),
                    is_error: res.is_error,
                    note: None,
                    display: None,
                })
            }
            Err(e) => {
                // A 401 on a *call* means the stored credential was rejected,
                // not that the arguments were wrong: flip the server to
                // `needs-auth` and hand back an actionable message instead of
                // a bare execution error (v2 `throwIfUnauthorized`,
                // agent/mcp/tools/mcp.ts:78-91). Application-level tool
                // failures never reach this branch — the transport reports
                // them as `Ok` with `is_error` set — so v2's "ignore McpError"
                // sniff filter has no work to do here.
                let unauthorized = self.mark_needs_auth(&server_name, &e, Some(&client)).await;
                let content = if unauthorized {
                    format!(
                        "MCP server \"{server_name}\" rejected the call with 401 Unauthorized and \
                         is now marked needs-auth. Run /mcp-config login {server_name} to complete \
                         the OAuth login, then retry the original call."
                    )
                } else {
                    format!("MCP execution error: {e}")
                };
                Some(ExecutableToolResult {
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

    /// Automatically connect/spawn MCP servers defined in configuration.
    /// Servers that fail to spawn are remembered with a `failed` status and
    /// their error so `/mcp` views stay truthful (v2 marks failed entries,
    /// connection-manager.ts:366-372).
    /// Connect every server in `config.mcp_servers` in parallel, using
    /// `config.mcp` as the global timeout defaults (v2 `connectAll` +
    /// `resolveDefaultTimeouts`; per-server failures are isolated so one
    /// crashed entry never blocks the others).
    pub async fn spawn_from_config(self: &Arc<Self>, config: &crate::config::KimiConfig) {
        self.set_defaults(config.mcp.clone());
        // v2's workspace host enables the startup retry; every entry point
        // that connects from config gets the same ticker.
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
                    // The CLI config path has no model in hand, so the
                    // `tool_select` + `dynamically_loaded_tools` gate cannot be
                    // applied here. An undeclared value inherits `[mcp]
                    // defaultDeferred`; the napi path passes the host's
                    // per-server verdict instead.
                    deferred: conf.deferred,
                    oauth: conf.auth.as_deref().map(|auth| auth == "oauth"),
                };
                Some(self.configure(name, recipe, options))
            })
            .collect::<Vec<_>>();
        futures_util::future::join_all(tasks).await;
    }

    /// The current retry policy: the `[mcp]` knobs plus whether a scheduler is
    /// running to act on a due time.
    fn retry_policy(&self) -> RetryPolicy {
        let defaults = self.defaults.read().unwrap_or_else(|e| e.into_inner());
        RetryPolicy {
            enabled: defaults.reconnect_retry_enabled.unwrap_or(true),
            ticker: self
                .retry_ticker_started
                .load(std::sync::atomic::Ordering::SeqCst),
            initial_delay_ms: defaults
                .reconnect_initial_delay_ms
                .unwrap_or(DEFAULT_RECONNECT_INITIAL_DELAY_MS),
            max_delay_ms: defaults
                .reconnect_max_delay_ms
                .unwrap_or(DEFAULT_RECONNECT_MAX_DELAY_MS),
        }
    }

    /// Start the startup-retry ticker once per manager (v2's `retryDue`
    /// scheduling, driven by the state machine). The ticker only *drains* what
    /// [`Self::mark_connect_failure`] scheduled: an entry in `retry-waiting`
    /// whose due time has passed is reconnected, and a successful connect ends
    /// the ladder. It stops with [`Self::shutdown`].
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

    /// Recompute the due time of every `retry-waiting` entry from the current
    /// `[mcp]` knobs (v2 `rescheduleRetries`, connection-manager.ts:501-505):
    /// a host that changed the reconnect delays while a retry was pending must
    /// not wait out the old schedule. The streak is preserved — this moves the
    /// wait, it does not reset the ladder — and the 250 ms ticker picks the new
    /// due time up on its next pass.
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

    /// Reconnect every entry whose retry is due. The due timestamp is cleared
    /// under the same lock that checked it, so a reconfigure, removal, or
    /// manual reconnect that landed in between wins over this pass.
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
            // No status event for a due retry: v2 suppresses the `retryDue`
            // transition (`suppressRetryStatus`, connection-manager.ts:600-601)
            // so a recovering server does not flash `pending` between two
            // `failed` entries. Only the outcome of the attempt is reported.
            // `connect_one` records either `connected` (which clears the
            // streak) or the next retry-waiting step itself.
            let _ = self.connect_one(&name).await;
        }
    }

    /// Register a server from a host-supplied recipe and connect it unless it
    /// is disabled (v2 `connect`, connection-manager.ts:182-205). The napi
    /// binding path calls this directly; `spawn_from_config` wraps it.
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
        // v2 #4210: a server that declares no `deferred` inherits the `[mcp]
        // defaultDeferred` section value; an explicit per-server value wins.
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
        // v2 `upsert` (connection-manager.ts:334-353): a live entry whose
        // configuration is unchanged is returned as it is, so re-running the
        // same configuration never retires a working client, its tools or its
        // connection. Everything `configure` would apply is compared, not just
        // the recipe — otherwise a changed filter or timeout would be dropped
        // on the floor by the early return.
        {
            let unchanged = {
                let servers = self.servers.read().await;
                servers.get(name).is_some_and(|state| {
                    is_live_status(&state.status)
                        && state.recipe == recipe
                        && state.deferred == deferred
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
                // v2 lists a disabled entry as `disabled` without ever
                // connecting it (connection-manager.ts:193-204).
                status: if options.enabled {
                    "pending".into()
                } else {
                    "disabled".into()
                },
                error: None,
                raw_tools: Vec::new(),
                deferred,
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
        // connect_one marks the entry failed/needs-auth itself; its returned
        // error already carries any captured child stderr.
        self.connect_one(name).await
    }

    /// Mark a failed connect on the entry: `needs-auth` when the failure is a
    /// 401 on a server without a static credential, otherwise `failed`
    /// (v2 `connectOne` catch branch + `shouldMarkNeedsAuth`). `display` is
    /// the (possibly stderr-augmented) text stored on the entry.
    async fn mark_connect_failure(&self, name: &str, error: &McpError, display: &str) {
        let oauth_installed = self.oauth_service().is_some();
        let (recipe, explicit_oauth) = self
            .servers
            .read()
            .await
            .get(name)
            .map(|s| (s.recipe.clone(), s.oauth))
            .unwrap_or((McpServerRecipe::Mock, false));
        // Startup auth rejections cover 401 and 403 (v2
        // `isAuthRejectionStatus`); the runtime flip stays 401-only.
        let needs_auth = should_mark_needs_auth(
            &recipe,
            explicit_oauth,
            oauth_installed,
            error.is_auth_rejection(),
        );
        let policy = self.retry_policy();
        let now = chrono::Utc::now().timestamp_millis();
        // v2 `automaticRetry` (connection-manager.ts:511): an attempt that
        // already carries a failure streak is a retry, so landing back in
        // `retryWaiting` only moves the due time — the public entry is a
        // `failed` entry before and after, and v2 suppresses the event
        // (`suppressRetryStatus`, :600-601) so the UI does not flicker.
        let mut suppress = false;
        if let Some(state) = self.servers.write().await.get_mut(name) {
            let streak_before = state.retry_streak;
            state.error = Some(display.to_string());
            if needs_auth {
                // v2 replaces the startup error with the login hint
                // (`handshakeFailed` → `needsAuth` returns
                // `needsAuthHint(entry.name)`, connection-manager.ts:641): the
                // raw 401 tells the user nothing they can act on.
                state.error = Some(needs_auth_hint(name));
                state.status = "needs-auth".into();
                state.retry_at_ms = None;
                state.retry_streak = 0;
            } else if error.is_permanent() {
                state.status = "failed".into();
                state.retry_at_ms = None;
                state.retry_streak = 0;
            } else {
                // v2 `failureTarget`: a transient failure retries while the
                // host enabled the retry (the workspace sets it).
                let (status, retry_at, streak) =
                    next_failure_state(policy, state.retry_streak, now);
                suppress = status == "retry-waiting" && streak_before > 0;
                state.status = status.into();
                state.retry_streak = streak;
                state.retry_at_ms = retry_at;
            }
        }
        if !suppress {
            self.emit_status(name).await;
        }
    }

    /// Connect one configured server, marking the outcome on its entry.
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
        // Captured before the client moves into `register_client`, so a
        // discovery failure can still carry the child's diagnostics
        // (v2 `formatStartupError`, connection-manager.ts:545-574).
        let mut stderr_tail = String::new();
        let connect = async {
            let client = match &recipe {
                McpServerRecipe::Sse {
                    url,
                    headers,
                    bearer_token_env_var,
                } => {
                    // A missing bearer env var is a configuration failure
                    // (v2 `config`): retrying cannot make the variable exist.
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
                    // v2 `stdioCwd`: a server whose config omits `cwd` starts
                    // in the workspace, not wherever the engine process runs.
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
            // v2 `isCurrent`: a `markRemoved` that landed while the handshake
            // was in flight owns the entry — the attempt is discarded instead
            // of resurrecting a tombstoned server (connection-manager.test.ts
            // "markRemoved … in-flight connects invalidated").
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
            // Watch for the transport dying after the handshake (v2
            // `watchForUnexpectedClose`, connection-manager.ts:321-341).
            if let Some(client_arc) = self.clients.read().await.get(name).cloned() {
                self.watch_session_evidence(name, client_arc).await;
            }
            Ok(ConnectOutcome::Registered)
        };
        // v2 wraps connect + tool discovery in `withTimeout` and reports
        // `Timed out after <ms>ms` (connection-manager.ts:594-611).
        match tokio::time::timeout(startup, connect).await {
            Ok(Ok(ConnectOutcome::Removed)) => {
                // Not a failure: the tombstone is the outcome, so the entry
                // keeps its `removed` status and emits nothing.
                return Err(format!("MCP server '{name}' was removed during startup"));
            }
            Ok(Ok(_)) => {
                // The transport may have died in the gap between discovery
                // finishing and this status write; never overwrite that
                // failure with `connected`.
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
            // A successful connect ends the retry ladder (v2 resets the streak
            // on `connected`).
            state.retry_at_ms = None;
            state.retry_streak = 0;
        }
        self.emit_status(name).await;
        Ok(())
    }

    /// The handles a session-evidence callback needs. `watchSessionEvidence`
    /// builds one per registration, and the probe path reads the same fields —
    /// the callback has no manager handle, so everything is cloned out here.
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

    /// Register the session-evidence watch on a live client (v2
    /// `attachSessionEvidence` + `watchForUnexpectedClose`,
    /// connection-manager.ts:754-764, :321-341).
    ///
    /// * **Hard** evidence (the transport closed on its own, v2
    ///   `sessionEvidence` / `onclose`) fails the entry directly on the retry
    ///   ladder.
    /// * **Weak** evidence (a resumable HTTP stream gave up reconnecting, v2
    ///   `{ kind: 'weak' }`) only says the session *may* be dead: the entry
    ///   moves to `probing`, pings the server with a 5 s budget, and returns to
    ///   `connected` untouched when the ping answers.
    ///
    /// Both callbacks check the client is still the registered one before
    /// touching state, so a shutdown / reconnect that already moved on is never
    /// overwritten (v2 `isCurrent` + `entry.client !== client`).
    async fn watch_session_evidence(&self, name: &str, client: Arc<McpClient>) {
        // Captured now: the callbacks have no manager handle, and the policy is
        // what decides whether a dead connection retries or fails. A policy
        // change mid-session only applies to the next registration, exactly as
        // the previous hard-only watch behaved.
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
                // Keep the probe observable to `shutdown`, which waits for it
                // (v2 awaits the entry's operation tail). Finished handles are
                // pruned so a long-lived manager cannot accumulate them.
                let mut guard = tasks.lock().unwrap_or_else(|e| e.into_inner());
                guard.retain(|task| !task.is_finished());
                guard.push(handle);
            }))
            .await;
    }

    /// Reconnect a configured server on demand (v2 `reconnect`,
    /// connection-manager.ts:146-164): close the live client, drop its
    /// cached tools, re-derive from the stored recipe, and mark the entry
    /// pending while connecting. Concurrent reconnects for the same server
    /// join the in-flight one (v2 `inFlightReconnects`).
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
        // Remove the in-flight marker and wake any joiners (v2
        // `reconnectAndJoin` deletes the entry in a `finally`,
        // connection-manager.ts:297-301). Without the removal a later
        // `reconnect` would find the already-notified marker and return
        // without reconnecting.
        let mut in_flight = self.in_flight.lock().await;
        if let Some(notify) = in_flight.remove(name) {
            notify.notify_waiters();
        }
        drop(in_flight);
        result
    }

    /// Reconnect after any in-flight reconnect for the same server has
    /// settled (v2 `reconnectAfterCurrent`, connection-manager.ts:297-301):
    /// wait for the current one, then start a fresh reconnect.
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
        // v2's `reconnect` resolves through the *live* entry view, so a
        // tombstoned server answers "Unknown MCP server" instead of being
        // revived (connection-manager.test.ts "markRemoved tombstones the
        // entry … reconnect rejected"). A fresh `connect`/`configure` is what
        // revives it.
        if status == "removed" {
            return Err(format!("Unknown MCP server: {name}"));
        }
        // A disabled server must not be reconnected (v2 throws
        // `MCP_SERVER_DISABLED`, connection-manager.ts:146-164).
        let enabled = {
            let filters = self.filters.read().await;
            filters.get(name).map(|f| f.enabled).unwrap_or(true)
        };
        if !enabled {
            return Err(format!("MCP server is disabled: {name}"));
        }
        // Close the live client (dropping it kills the stdio child) and
        // clear its cached tools, mirroring the v2 close-then-pending order.
        // The close happens outside the map locks so it cannot block the
        // manager while the child is being killed.
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
        // connect_one marks the failure state itself.
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

/// Fan one public entry out to every status listener. Failed / needs-auth
/// transitions are logged first, and a panicking listener must not break the
/// connection manager (v2 wraps listener calls in try/catch). The single
/// shared fan-out keeps `emit_status` and the unexpected-close watcher
/// identical (logging + panic isolation included).
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

/// The public view of a server entry (v2 `toPublicEntry`,
/// connection-manager.ts:445-456): `toolCount` is the raw advertised count,
/// which is 0 unless the entry is `connected` (the caller zeroes `raw_tools`
/// on failure / removal).
fn to_public_entry(name: &str, state: &ServerState) -> McpServerEntry {
    McpServerEntry {
        name: name.to_string(),
        transport: state.recipe.transport_label(),
        status: public_status(&state.status).to_string(),
        tool_count: state.raw_tools.len(),
        error: state.error.clone(),
        // v2 `toPublicEntry`: the due time is only meaningful while a retry is
        // scheduled; every other state reports `undefined`.
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

/// v2 `DEFAULT_RECONNECT_INITIAL_DELAY_MS` (connection-manager.ts:100): the
/// first automatic startup retry waits this long.
const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 5_000;

/// v2 `DEFAULT_RECONNECT_MAX_DELAY_MS` (connection-manager.ts:101).
const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 300_000;

/// v2 `nextRetryDelayMs` (connection-manager.ts:690-696):
/// `min(initial * 2^streak, max)` times a 50–100 % jitter. `streak` is
/// zero-based (the first retry uses the initial delay).
fn retry_delay_ms(streak: u32, initial: u64, max: u64) -> u64 {
    let nominal = initial
        .saturating_mul(1u64 << streak.min(32))
        .min(max)
        .max(1);
    let jitter = 0.5 + fastrand::f64() * 0.5;
    ((nominal as f64) * jitter) as u64
}

/// The retry policy a failure consults: v2 `reconnectRetryEnabled` plus the
/// resolved delays. `ticker` is whether a scheduler exists to act on a due
/// time — without one a `retry-waiting` entry would wait forever, so the entry
/// fails instead.
#[derive(Clone, Copy)]
struct RetryPolicy {
    enabled: bool,
    ticker: bool,
    initial_delay_ms: u64,
    max_delay_ms: u64,
}

/// How one startup attempt ended inside `connect_one`. `Removed` is not a
/// failure — a `markRemoved` that landed mid-handshake owns the entry — so it
/// must not run the failure path that would overwrite the tombstone.
enum ConnectOutcome {
    Registered,
    Removed,
}

/// What a failure does to the entry's retry state (v2 `retryOrFailed` +
/// `nextRetryDelayMs`): the status, the due time, and the streak after this
/// failure. `needs-auth` and permanent failures never reach here.
fn next_failure_state(
    policy: RetryPolicy,
    streak: u32,
    now_ms: i64,
) -> (&'static str, Option<i64>, u32) {
    if !policy.enabled || !policy.ticker {
        return ("failed", None, 0);
    }
    let streak = streak.saturating_add(1);
    let delay = retry_delay_ms(streak - 1, policy.initial_delay_ms, policy.max_delay_ms);
    ("retry-waiting", Some(now_ms + delay as i64), streak)
}

/// The actionable message for an entry that needs OAuth (v2 `needsAuthHint`,
/// connection-manager.ts:641): both the startup 401 and the runtime 401 carry
/// it, so the user always sees what to run.
fn needs_auth_hint(name: &str) -> String {
    LocalizedText::with_params("engine.mcp.needsAuthHint", i18n_params!["name" => name]).render()
}

/// Whether a failure should flip the entry into `needs-auth` instead of
/// `failed` (v2 `shouldMarkNeedsAuth`, connection-manager.ts:383-393):
/// only remote servers without a static credential participate in the OAuth
/// flow, and only when the transport classified the failure as HTTP 401.
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
    // A pinned static credential means the 401 is a bad header, not a missing
    // OAuth token — the real error is more actionable than "run /mcp-config
    // login" for a server that doesn't speak OAuth.
    if bearer_token_env_var.is_some() {
        return false;
    }
    // v2 `config.headers !== undefined && config.auth !== 'oauth'`: extra
    // headers on a server that never asked for OAuth are the suspect, so the
    // entry fails with the header error. An explicit `auth = "oauth"` says the
    // headers are unrelated to the credential, so the 401 still means "log in".
    if !headers.is_empty() && !explicit_oauth {
        return false;
    }
    true
}

/// A configured server is remote when it has a URL, otherwise stdio when it
/// has a command; entries with neither are not MCP servers at all. An explicit
/// `transport` wins, and a bare `url` defaults to Streamable HTTP exactly like
/// the v2 config preprocess (`config-schema.ts:58-65`); legacy HTTP+SSE needs
/// `transport = "sse"`.
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

/// Merge `headers` with a bearer token read from `env_var`, mirroring v2
/// `buildMcpRemoteHeaders` (client-remote.ts:4-25): a missing or empty
/// variable is a configuration error, and any pre-existing Authorization
/// header is replaced by the resolved token.
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
