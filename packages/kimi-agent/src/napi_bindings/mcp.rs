//! MCP server management behind the napi boundary: the process-wide
//! `McpManager` registry, status event bridge, and OAuth flow exports.

use super::*;
use crate::mcp::manager::{McpServerOptions, McpServerRecipe};
use napi_derive::napi;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, Weak};

/// One resolved MCP server handed to [`shared_mcp_manager`]: its name, how to
/// spawn it, and its per-server options.
pub(crate) type McpServerSpec = (String, McpServerRecipe, McpServerOptions);

/// Whether host-supplied `transport: "mock"` configurations are accepted.
///
/// The mock transport answers every call with fabricated results, so it must
/// never be reachable from a production configuration — a typo in `transport`
/// otherwise silently registers a server that feeds invented tool output to
/// the model. Integration tests opt in with `KIMI_NATIVE_ALLOW_MOCK_MCP=1`.
pub(crate) fn allow_mock_mcp_transport() -> bool {
    std::env::var("KIMI_NATIVE_ALLOW_MOCK_MCP")
        .is_ok_and(|value| matches!(value.trim(), "1" | "true"))
}

/// The process-wide MCP managers, keyed by the resolved server set.
///
/// `Weak` on purpose: the cache must never be the reason a manager (and the
/// child processes it owns) stays alive. Sessions hold the strong references,
/// so the entry goes stale once the last session is disposed and the next
/// lookup drops it.
static MCP_MANAGERS: LazyLock<Mutex<HashMap<String, Weak<crate::mcp::McpManager>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The manager for `servers`, connecting it on first use.
///
/// Two sessions built from the same configuration share one manager, so `/new`
/// reuses the live connections instead of re-spawning every server (v2's
/// workspace-scoped manager, workspaceMcpService.ts:61-83). A changed
/// configuration hashes to a different key and gets its own manager; the old
/// one is dropped once its sessions are gone.
pub(crate) fn shared_mcp_manager(
    servers: Vec<McpServerSpec>,
    oauth_dir: Option<std::path::PathBuf>,
    defaults: Option<crate::config::McpConfig>,
    stdio_cwd: Option<std::path::PathBuf>,
    client_name: Option<String>,
) -> Arc<crate::mcp::McpManager> {
    let mut key = mcp_servers_key(&servers);
    if let Some(dir) = oauth_dir.as_deref() {
        key.push_str("|oauth:");
        key.push_str(&dir.display().to_string());
    }
    // A stdio server with no `cwd` of its own starts in the workspace (v2
    // `stdioCwd`), so two sessions from different workspaces must not share a
    // manager: the first one's working directory would decide for both.
    if let Some(dir) = stdio_cwd.as_deref() {
        key.push_str("|cwd:");
        key.push_str(&dir.display().to_string());
    }
    // The announced client name is observable by every server, so a manager
    // built for one identity must not be handed to another.
    if let Some(name) = client_name.as_deref() {
        key.push_str("|client:");
        key.push_str(name);
    }
    // `[mcp]` defaults change the resolved timeouts and the disclosure of
    // servers that declare neither, so they are part of the identity too.
    if let Some(defaults) = defaults.as_ref() {
        key.push_str(&format!(
            "|mcp:{}:{}:{:?}",
            defaults.startup_timeout_ms.unwrap_or(0),
            defaults.tool_timeout_ms.unwrap_or(0),
            defaults.default_deferred,
        ));
    }
    let mut cache = MCP_MANAGERS.lock().unwrap_or_else(|e| e.into_inner());
    // Drop the entries whose sessions are gone before deciding, so a stale
    // entry can never shadow a fresh manager for the same configuration.
    cache.retain(|_, weak| weak.strong_count() > 0);
    if let Some(existing) = cache.get(&key).and_then(Weak::upgrade) {
        // A reused manager still takes the caller's `[mcp]` defaults: the
        // section can change between sessions, and every session sharing this
        // manager must read the same values.
        if let Some(defaults) = defaults {
            existing.set_defaults(defaults);
        }
        existing.set_client_name(client_name);
        return existing;
    }
    let manager = Arc::new(crate::mcp::McpManager::new());
    if let Some(dir) = oauth_dir {
        manager.set_oauth_service(Arc::new(crate::mcp::oauth::McpOAuthService::new(Arc::new(
            crate::mcp::oauth::McpOAuthFileStore::new(dir),
        ))));
    }
    // Before `connect_all`: its spawned task reads the defaults while
    // configuring every server, including plugin-contributed ones.
    if let Some(defaults) = defaults {
        manager.set_defaults(defaults);
    }
    if let Some(dir) = stdio_cwd {
        manager.set_default_stdio_cwd(dir);
    }
    // Before `connect_all`: every initialize it sends announces this name.
    manager.set_client_name(client_name);
    manager.connect_all(servers);
    cache.insert(key, Arc::downgrade(&manager));
    manager
}

/// A stable fingerprint of a resolved server set.
///
/// `env` and `headers` are maps, so they are sorted before hashing: an
/// unordered walk would give one configuration two different keys and defeat
/// the cache.
fn mcp_servers_key(servers: &[McpServerSpec]) -> String {
    let mut parts: Vec<String> = servers
        .iter()
        .map(|(name, recipe, options)| {
            let body = match recipe {
                McpServerRecipe::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                } => format!(
                    "stdio|{command}|{}|{}|{}",
                    args.join("\u{1f}"),
                    sorted_pairs(env),
                    cwd.as_deref().unwrap_or("")
                ),
                McpServerRecipe::Sse {
                    url,
                    headers,
                    bearer_token_env_var,
                } => format!(
                    "sse|{url}|{}|{}",
                    sorted_pairs(headers),
                    bearer_token_env_var.as_deref().unwrap_or("")
                ),
                McpServerRecipe::Http {
                    url,
                    headers,
                    bearer_token_env_var,
                } => format!(
                    "http|{url}|{}|{}",
                    sorted_pairs(headers),
                    bearer_token_env_var.as_deref().unwrap_or("")
                ),
                McpServerRecipe::Mock => "mock".to_string(),
            };
            format!(
                "{name}\u{1e}{body}\u{1e}{}|{}|{}|{}|{}|{}",
                options.enabled,
                sorted_list(options.enabled_tools.as_deref()),
                sorted_list(options.disabled_tools.as_deref()),
                options.startup_timeout_ms.unwrap_or(0),
                options.tool_timeout_ms.unwrap_or(0),
                // The disclosure verdict is resolved against the `[mcp]`
                // default, so two sessions that differ here need different
                // managers: a reused manager is never reconfigured.
                options
                    .deferred
                    .map_or("auto", |value| if value { "on" } else { "off" }),
            )
        })
        .collect();
    parts.sort();
    parts.join("\u{1d}")
}

fn sorted_pairs(map: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = map.iter().collect();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn sorted_list(values: Option<&[String]>) -> String {
    let Some(values) = values else {
        return String::new();
    };
    let mut names: Vec<&String> = values.iter().collect();
    names.sort();
    names
        .iter()
        .map(|name| name.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Bridge the manager's status transitions to the host's event stream, so a
/// server that connects (or fails) *after* `createSession` returned still
/// reaches the UI (v2 `handleMcpServerStatusChange`, mcpService.ts:168-180).
///
/// The mapping and the replay live in [`crate::mcp::status`]; this wrapper is
/// only the napi sink (a JS callback). The bridge owns the subscription, so a
/// disposed session stops receiving transitions and stops holding the TSFN
/// alive (v2 `attachMcpTools`'s `_register` disposal, mcpService.ts:154-166).
pub(crate) use crate::mcp::status::McpStatusBridge;

pub(crate) fn attach_mcp_status(
    manager: &Option<Arc<crate::mcp::McpManager>>,
    callbacks: Arc<dyn crate::callbacks::HostCallbacks>,
) -> Option<Arc<McpStatusBridge>> {
    let manager = manager.as_ref()?;
    let emit: crate::mcp::status::McpEventSink = Arc::new(move |event| callbacks.emit_event(event));
    Some(crate::mcp::status::attach(manager, emit))
}

// ── MCP OAuth (browser auth-code + PKCE flow) ─────────────────────────────

fn mcp_oauth_store(dir: &str) -> napi::Result<Arc<crate::mcp::oauth::McpOAuthService>> {
    if dir.trim().is_empty() {
        return Err(napi::Error::from_reason(
            "MCP OAuth credential directory is empty",
        ));
    }
    Ok(Arc::new(crate::mcp::oauth::McpOAuthService::new(Arc::new(
        crate::mcp::oauth::McpOAuthFileStore::new(std::path::PathBuf::from(dir)),
    ))))
}

/// Persist tokens obtained by the host's browser flow. The engine owns the
/// encryption/at-rest format; the host hands over the OAuth token response in
/// `McpOAuthTokens` JSON shape. Returns the credential store key.
#[napi]
pub fn mcp_oauth_store_tokens(
    name: String,
    url: String,
    tokens_json: String,
    dir: String,
) -> napi::Result<String> {
    guard_sync_panic(|| {
        let tokens: crate::mcp::oauth::McpOAuthTokens = serde_json::from_str(&tokens_json)
            .map_err(|e| napi::Error::from_reason(format!("invalid MCP OAuth tokens: {e}")))?;
        let key = crate::mcp::oauth::mcp_oauth_store_key(&name, &url)
            .map_err(napi::Error::from_reason)?;
        mcp_oauth_store(&dir)?
            .store_tokens(&key, &tokens)
            .map_err(napi::Error::from_reason)?;
        Ok(key)
    })
}

/// Stored-credential state for the host's auth-status classifier. Reads the
/// same encrypted store the connection manager injects from; `accessToken` is
/// the stored value even when expired (the engine refreshes on connect).
#[napi]
pub async fn mcp_oauth_token_state(name: String, url: String, dir: String) -> napi::Result<String> {
    let key =
        crate::mcp::oauth::mcp_oauth_store_key(&name, &url).map_err(napi::Error::from_reason)?;
    let state = mcp_oauth_store(&dir)?.token_state(&key);
    Ok(serde_json::json!({
        "hasTokens": state.has_tokens,
        "hasRefreshToken": state.has_refresh_token,
        "expired": state.expired,
        "accessToken": state.access_token,
    })
    .to_string())
}

/// Drop a server's stored grant (`auth:reset`). `false` when nothing was
/// stored.
#[napi]
pub fn mcp_oauth_remove(name: String, url: String, dir: String) -> napi::Result<bool> {
    guard_sync_panic(|| {
        let key = crate::mcp::oauth::mcp_oauth_store_key(&name, &url)
            .map_err(napi::Error::from_reason)?;
        mcp_oauth_store(&dir)?
            .remove(&key)
            .map_err(napi::Error::from_reason)
    })
}

/// Reconnect every live manager that knows `name`, so a server marked
/// `needs-auth` retries with the grant the host just stored. Returns how many
/// managers reconnected.
#[napi]
pub async fn mcp_oauth_reconnect(name: String) -> napi::Result<u32> {
    let managers: Vec<Arc<crate::mcp::McpManager>> = {
        let mut cache = MCP_MANAGERS.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, weak| weak.strong_count() > 0);
        cache.values().filter_map(Weak::upgrade).collect()
    };
    let mut reconnected = 0u32;
    for manager in managers {
        if manager.reconnect(&name).await.is_ok() {
            reconnected += 1;
        }
    }
    Ok(reconnected)
}

/// Close every server the process-wide cache spawned and forget the managers,
/// returning how many were shut down.
///
/// Sessions share these managers on purpose — `/new` reuses live connections,
/// which is v2's workspace-scoped `WorkspaceMcpService` — so no per-session
/// teardown may close them: a workspace outlives its sessions on v2, and the
/// same connections serve the next one. v2 closes them with the workspace
/// instead: `WorkspaceMcpService.disposeAsync` awaits `manager.shutdown()`
/// (workspaceMcpService.ts:91-112). The fork has no workspace tier behind this
/// addon — the host *is* the workspace — so that dispose step is this export,
/// and a host that is done with the engine (a test deleting its temporary
/// home, an embedder shutting down) needs it explicitly: on Windows a stdio
/// server's working directory stays busy until that child exits, so
/// `remove_dir_all` on a plugin's own directory fails with `EBUSY` long after
/// the last session is gone.
#[napi]
pub async fn mcp_managers_dispose() -> napi::Result<u32> {
    let managers: Vec<Arc<crate::mcp::McpManager>> = {
        let mut cache = MCP_MANAGERS.lock().unwrap_or_else(|e| e.into_inner());
        let live: Vec<Arc<crate::mcp::McpManager>> =
            cache.values().filter_map(Weak::upgrade).collect();
        cache.clear();
        live
    };
    let count = u32::try_from(managers.len()).unwrap_or(u32::MAX);
    for manager in managers {
        manager.shutdown().await;
    }
    Ok(count)
}
