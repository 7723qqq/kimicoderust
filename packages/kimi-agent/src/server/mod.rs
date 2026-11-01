//! Native HTTP REST request dispatcher for the Kimi Agent API surface.
//!
//! ## Locking convention
//!
//! The server's registries (event hub lanes, the active-turn table, pending
//! questions and approvals, activity state) are shared by every session and
//! connection. A `std::sync::Mutex` stays poisoned for the rest of the process
//! once a thread panics while holding it, and every later `.lock().unwrap()`
//! panics too — so one panic in one session's callback would take the whole
//! server down. Those locks are therefore taken with
//! `.lock().unwrap_or_else(|poisoned| poisoned.into_inner())`, which recovers
//! the guard instead of propagating the panic.
//!
//! `HttpServer::handle_request` maps a handful of paths (`/health`,
//! `/api/v1/sessions`, `POST /api/v1/sessions/:id/prompt`) onto
//! `SqliteSessionStore`, and [`http::serve`] binds them to a real TCP listener.
//! The one product entry is `kimi-agent --serve <ADDR>`, which builds the store,
//! the engine and the [`ServerAuth`] credential and hands them to
//! [`http::serve`].
//!
//! What is still missing before this fully replaces `packages/kap-server`'s
//! `/api/v1`:
//!
//! - the `subscribe_v2` transcript stream carries the baseline reset plus live
//!   ops projected from engine events and filtered by the subscribed grade
//!   (`off`/`turn`/`block`/`delta`); the REST catch-up
//!   (`GET .../transcript/ops`) serves an authoritative `reset` batch;
//! - the prompt-queue surface is real: `/sessions/{id}/prompts` admits one
//!   active prompt per session with the rest queued FIFO, `:steer` moves queued
//!   prompts into the running turn, and `:abort` settles the active prompt;
//! - the `/api/v1/debug/*` reflection surface (`channels`, `tree`, `graph`,
//!   `subscriptions`, `cascade`) is backed by the live store / hub / engine —
//!   the scope and DI shapes are mapped from those, not a v2 DI ledger.
//!
//! `kimi-agent --serve` is the only `/api/v1` surface: `packages/kap-server`
//! has been retired and removed, and `--legacy-server` / `KIMI_LEGACY_SERVER=1`
//! now fail loudly instead of falling back.

pub mod activity;
pub mod auth;
pub mod custom_registry;
pub mod debug;
pub mod engine;
pub mod envelope;
pub mod file_launch;
pub mod files;
pub mod fs_routes;
pub mod host_guard;
pub mod http;
pub mod hub;
pub mod interaction;
pub mod media;
pub mod message_events;
pub mod model_catalog;
pub mod models_dev;
pub mod oauth;
pub mod plugin_archive;
pub mod plugins;
pub mod prompt_queue;
pub mod provider_refresh;
pub mod provider_write;
pub mod remote_control;
pub mod router;
pub mod static_files;
pub mod terminal;
pub mod transcript;
pub mod web_events;
pub mod ws;
pub mod ws_protocol;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::cron::scheduler::{CronEntry, CronScheduler};
use crate::server::auth::ServerAuth;
use crate::server::engine::ServerEngine;
use crate::server::host_guard::HostGuard;
use crate::server::hub::EventHub;
use crate::server::router::{HttpRequest, HttpResponse};
use crate::session::sqlite_store::SqliteSessionStore;
use crate::storage::task_runner::TaskRunner;

/// True when `host` names this machine only.
///
/// `--serve` uses it to decide whether the loopback-only routes are mounted at
/// all, and [`http::serve`] uses it to refuse an unauthenticated non-loopback
/// bind. A wildcard bind (`0.0.0.0`, `::`) is deliberately *not* loopback: it
/// is reachable from the network, which is the whole point of the distinction.
pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]") || host.starts_with("127.")
}

pub struct HttpServer {
    store: Arc<SqliteSessionStore>,
    hub: Arc<EventHub>,
    engine: Option<Arc<ServerEngine>>,
    auth: ServerAuth,
    heartbeat: Duration,
    cron_scheduler: Arc<Mutex<CronScheduler>>,
    task_runner: Arc<TaskRunner>,
    /// Whether the background-task wake service was spawned for this server.
    /// `with_engine` starts it once; it turns a completed background task into
    /// a notification turn through the prompt queue (v2
    /// `taskService.notifyAgentTask`).
    task_wake_started: bool,
    server_id: String,
    started_at: String,
    web_assets_dir: Option<PathBuf>,
    mcp_manager: Arc<crate::mcp::manager::McpManager>,
    /// MCP OAuth credentials (device-code login surface).
    mcp_oauth: Arc<tokio::sync::Mutex<Option<Arc<crate::mcp::oauth::McpOAuthService>>>>,
    /// Pending browser authorization-code flows (the standalone server has no
    /// JS host to hold them): keyed by callback state and by flow id.
    mcp_auth_code: Arc<crate::mcp::oauth::auth_code::AuthCodeRegistry>,
    interaction_manager: Arc<interaction::InteractionManager>,
    plugin_manager: Arc<plugins::PluginManager>,
    oauth_manager: Arc<oauth::OAuthManager>,
    /// The per-session active/queued prompt state machine behind
    /// `/sessions/{id}/prompts`.
    prompt_queue: Arc<prompt_queue::PromptQueue>,
    config_override: Arc<Mutex<Option<crate::config::KimiConfig>>>,
    /// The models.dev catalog proxy cache (v2 `getModelsDevCatalog`): TTL,
    /// in-flight dedup, stale fallback; the built-in list ends the chain.
    models_dev_cache: Arc<models_dev::CatalogCache>,
    /// The `config.toml` the write routes mutate: the file the server loaded,
    /// or discovery when unset (unset in tests via
    /// [`HttpServer::with_config_write_path`]).
    config_write_path: std::sync::Mutex<Option<PathBuf>>,
    /// The `/api/v1/files` upload store.
    file_store: files::FileStore,
    terminal_manager: Arc<terminal::TerminalManager>,
    subagent_manager: Arc<crate::subagent::SubagentManager>,
    /// Remote Control status state (#3594).
    remote_control_state: Arc<Mutex<RemoteControlStatusWire>>,
    /// The live remote-control runtime (#3594), started by
    /// `POST /api/v1/remote-control`. `None` until then; the status routes
    /// read whichever of the two is live.
    remote_control_runtime:
        Arc<tokio::sync::Mutex<Option<crate::server::remote_control::RemoteControlHandle>>>,
    /// Cancelled by `POST /api/v1/shutdown`; the `http::serve` accept loop
    /// selects on it so the request actually stops the server.
    shutdown: tokio_util::sync::CancellationToken,
    /// Whether the listener is on a loopback address. An in-process server
    /// (`HttpServer::new`, tests) is loopback by definition; `--serve` sets
    /// this from the address it was given.
    loopback_bind: bool,
    /// `--debug-endpoints`: mount the `/api/v1/debug/*` reflection surface.
    /// Off unless asked for — it is a test-introspection tool, not a product
    /// route.
    debug_endpoints: bool,
    /// `--allow-remote-shutdown`: keep `POST /api/v1/shutdown` registered on a
    /// non-loopback bind.
    allow_remote_shutdown: bool,
    /// The DNS-rebinding guard. `None` disables the check: an in-process server
    /// has no bind address to compare against and no browser to rebind, and the
    /// product entry (`--serve`) always installs one.
    host_guard: Option<HostGuard>,
    /// Server-side usage telemetry (v2 #3897): the sink v2 injects as
    /// `ITelemetryService` into its route hosts. `None` on every current entry
    /// point — the transitions are recorded where they happen, and wiring a
    /// real sink lands with the next host integration.
    telemetry_sink: Option<crate::tools::external_hooks::TelemetrySink>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RemoteControlStatusWire {
    pub enabled: bool,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RemoteControlStatusWire {
    pub fn off() -> Self {
        Self {
            enabled: false,
            state: "off".to_string(),
            url: None,
            device_id: None,
            device_name: None,
            error: None,
        }
    }
}

mod builder;
mod catalog_routes;
mod dispatch;

/// A kap-server validation error for malformed provider payloads.
fn provider_validation_error(message: String) -> HttpResponse {
    HttpResponse::json(
        400,
        &json!({
            "code": crate::server::envelope::error_codes::VALIDATION_FAILED,
            "msg": message,
        }),
    )
}

fn extract_session_action<'a>(path: &'a str, action: &str) -> Option<&'a str> {
    let suffix_slash = format!("/{action}");
    let suffix_colon = format!(":{action}");
    let prefix = "/api/v1/sessions/";
    if !path.starts_with(prefix) {
        return None;
    }
    let rest = &path[prefix.len()..];
    if let Some(id) = rest.strip_suffix(&suffix_slash)
        && !id.is_empty()
        && !id.contains('/')
        && !id.contains(':')
    {
        return Some(id);
    }
    if let Some(id) = rest.strip_suffix(&suffix_colon)
        && !id.is_empty()
        && !id.contains('/')
        && !id.contains(':')
    {
        return Some(id);
    }
    None
}

fn extract_session_subaction<'a>(path: &'a str, action: &str, sub: &str) -> Option<&'a str> {
    let suffix = format!("/{action}/{sub}");
    let rest = path.strip_prefix("/api/v1/sessions/")?;
    let id = rest.strip_suffix(&suffix)?;
    if id.is_empty() || id.contains('/') || id.contains(':') {
        return None;
    }
    Some(id)
}

pub(crate) fn infer_media_type(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// The turn origin a prompt submission's `metadata` builds (v2 #3764 /
/// #3832). The request wraps the client's object as the one-element array
/// v2's `clientMetadata` is. A `origin` field inside it is a prompt origin
/// in the transcript contract's shape — the `skill_activation` variant a
/// user-slash activation carries, which the shipped web client nests there
/// (`metadata.origin`, bundle `activateSkill`) — and is used as-is, so the
/// v3 history projection can read the activation off the turn origin.
/// Everything else keeps the opaque-client-metadata wrapping.
fn prompt_origin_from_metadata(client_metadata: Option<&Value>) -> Option<Value> {
    let metadata = client_metadata?;
    let inner = &metadata[0];
    inner
        .get("origin")
        .filter(|origin| origin.get("kind").and_then(Value::as_str).is_some())
        .cloned()
        .or_else(|| Some(json!({ "kind": "user", "clientMetadata": metadata })))
}

/// The media family a part's `type` names. Anything that is not video or
/// audio is treated as an image, matching the intake's historical default.
fn media_kind_of(kind: &str) -> crate::rpc::types::MediaKind {
    match kind {
        "video" => crate::rpc::types::MediaKind::Video,
        "audio" => crate::rpc::types::MediaKind::Audio,
        _ => crate::rpc::types::MediaKind::Image,
    }
}

/// The media family a MIME type names, or `None` for a type the engine has no
/// media block for (the caller keeps its text placeholder).
fn media_kind_for_type(media_type: &str) -> Option<crate::rpc::types::MediaKind> {
    use crate::rpc::types::MediaKind;
    if media_type.starts_with("image/") {
        Some(MediaKind::Image)
    } else if media_type.starts_with("video/") {
        Some(MediaKind::Video)
    } else if media_type.starts_with("audio/") {
        Some(MediaKind::Audio)
    } else {
        None
    }
}

/// A reference to a file already in the store. The bytes stay where they are:
/// the engine's resolver reads them at request time, once it knows which model
/// the turn runs on.
fn file_ref_from_store(
    store: &crate::server::files::FileStore,
    file_id: &str,
    kind_hint: &str,
) -> Result<crate::rpc::types::ContentBlock, String> {
    let (meta, path) = store.get(file_id).map_err(|error| error.2.to_string())?;
    let media_type = if meta.media_type.is_empty() {
        infer_media_type(&path)
    } else {
        meta.media_type.clone()
    };
    let kind = match media_kind_for_type(&media_type) {
        Some(kind) => kind,
        // A part that names its own family still gets a reference: the store
        // may hold a type the MIME prefix cannot place.
        None if !kind_hint.is_empty() => media_kind_of(kind_hint),
        // A plain attachment has no media block; the model reads it by name.
        None => {
            return Ok(crate::rpc::types::ContentBlock::Text {
                text: format!(
                    "[Attached file: {} ({media_type}, {} bytes)]",
                    meta.name, meta.size
                ),
            });
        }
    };
    Ok(crate::rpc::types::ContentBlock::MediaRef {
        file_id: file_id.to_string(),
        kind,
    })
}

/// Convert a protocol `MessageContent[]` prompt submission into the engine's
/// `(text, media blocks)` pair. Media parts are resolved from the local file
/// store (uploaded `f_` blobs) or read in place from a server-local path.
/// Persist a prompt submission's profile options into the session's
/// `agent_config` and `metadata` — the state the turn loop reads (`metadata`
/// for permission mode, `agent_config` for model / thinking / disabled tools).
/// Rejects an unknown `permission_mode`.
fn apply_prompt_submission_options(
    server: &HttpServer,
    session_id: &str,
    body: &Value,
) -> Result<(), String> {
    let mut agent_config = server
        .store()
        .get_state("agent_config", session_id)
        .ok()
        .flatten()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    // Transition baselines are read before the mutable borrow below overwrites
    // them (v2 #3897 emits on the transition, not on every write).
    let swarm_was_on = agent_config
        .get("swarm_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    {
        let config = agent_config.as_object_mut().unwrap();
        for key in [
            "model",
            "thinking",
            "profile",
            "goal_objective",
            "goal_control",
        ] {
            if let Some(value) = body
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|value| !value.is_empty())
            {
                config.insert(key.to_string(), json!(value));
            }
        }
        if let Some(plan_mode) = body.get("plan_mode").and_then(|v| v.as_bool()) {
            config.insert("plan_mode".to_string(), json!(plan_mode));
        }
        if let Some(swarm_mode) = body.get("swarm_mode").and_then(|v| v.as_bool()) {
            if swarm_was_on != swarm_mode {
                server.emit_session_telemetry_named(if swarm_mode {
                    "swarm_mode_entered"
                } else {
                    "swarm_mode_exited"
                });
            }
            config.insert("swarm_mode".to_string(), json!(swarm_mode));
        }
        if let Some(disabled) = body.get("disabled_tools").and_then(|v| v.as_array()) {
            config.insert("disabled_tools".to_string(), Value::Array(disabled.clone()));
        }
    }
    // A dropped write here means the turn runs with the previous
    // model/thinking/profile/permission settings while the client believes the
    // submission was accepted. Surface it instead.
    if let Err(e) =
        server
            .store()
            .put_session_state("agent_config", session_id, session_id, &agent_config)
    {
        return Err(format!("failed to persist the agent config: {e}"));
    }

    let mut metadata = server
        .store()
        .get_state("metadata", session_id)
        .ok()
        .flatten()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    {
        let meta = metadata.as_object_mut().unwrap();
        if let Some(extra) = body.get("metadata").and_then(|v| v.as_object()) {
            for (key, value) in extra {
                meta.insert(key.clone(), value.clone());
            }
        }
        if let Some(mode) = body.get("permission_mode").and_then(|v| v.as_str()) {
            if !matches!(mode, "manual" | "yolo" | "auto") {
                return Err(format!("invalid permission_mode: {mode}"));
            }
            meta.insert("permission_mode".to_string(), json!(mode));
        }
    }
    if let Err(e) = server
        .store()
        .put_session_state("metadata", session_id, session_id, &metadata)
    {
        return Err(format!("failed to persist session metadata: {e}"));
    }

    // Plan mode is workspace-scoped state the plan guard reads; activate or
    // clear it so the submitted `plan_mode` takes effect on the turn. A caller
    // that asked for a plan-mode change must not have it silently dropped.
    if let Some(plan_mode) = body.get("plan_mode").and_then(|v| v.as_bool()) {
        let work_dir =
            fs_routes::resolve_session_workdir(server.store(), session_id).ok_or_else(|| {
                "plan_mode was submitted but this session has no resolvable working directory"
                    .to_string()
            })?;
        let state = crate::storage::StateStore::for_workspace(&work_dir)
            .map_err(|e| format!("failed to open the workspace state store for plan mode: {e}"))?;
        state
            .write_domain("plan", &json!({ "active": plan_mode }))
            .map_err(|e| format!("failed to persist plan mode: {e}"))?;
    }

    // Swarm mode is agent state the `swarm_mode` injection reads; mirror the
    // submitted flag into the registry the way plan mode is mirrored into the
    // workspace `plan` domain. Without this the `/swarm` command only flips a
    // status flag and the mode never reaches the turn loop, so the reminder
    // and the auto-exit never fire for a `manual` / `task` swarm.
    if let Some(swarm_mode) = body.get("swarm_mode").and_then(|v| v.as_bool()) {
        let agent_id = crate::callbacks::MAIN_AGENT_ID;
        if swarm_mode {
            let trigger =
                swarm_trigger_from(body.get("swarm_mode_trigger").and_then(|v| v.as_str()));
            crate::swarm::mode::swarm_mode_registry().enter(agent_id, trigger);
        } else {
            crate::swarm::mode::swarm_mode_registry().exit(agent_id);
        }
    }
    Ok(())
}

/// The wire spelling of a swarm trigger. v2 has exactly one writer besides
/// the tool — kap-server's `swarm.enter('manual')` on the profile route — so
/// `manual` is the default an unlabelled write means.
fn swarm_trigger_from(raw: Option<&str>) -> crate::swarm::mode::SwarmModeTrigger {
    use crate::swarm::mode::SwarmModeTrigger;
    match raw {
        Some("task") => SwarmModeTrigger::Task,
        Some("tool") => SwarmModeTrigger::Tool,
        _ => SwarmModeTrigger::Manual,
    }
}

/// Explains why `/api/v1/remote-control` cannot enable anything.
///
/// Kept as a shared constant so GET and POST cannot drift into telling different
/// stories about the same missing capability.
const REMOTE_CONTROL_UNAVAILABLE: &str = "remote-control is not running: the runtime exists (device registration, relay channel, heartbeat) but no session started it — POST /api/v1/remote-control with enabled=true and the Kimi login refresh_token to start one";

/// Generate a stable device id for remote control (TS `createKimiDeviceId`
/// shape: a ULID).
fn new_device_id() -> String {
    ulid::Ulid::new().to_string()
}

/// The relay origin the Remote Control toggle must use.
///
/// v2 #3969: a global (`.ai`) login must reach `code-rc.kimi.ai`; the mainland
/// login keeps `code-rc.kimi.com`. The region comes from the same persisted
/// OAuth ref upstream's server hands to the Remote Control manager (the managed
/// provider's `oauth` table in `config.toml`), with the install-channel marker
/// deciding only before the first login. An operator's
/// `KIMI_CODE_REMOTE_CONTROL_RELAY_URL` still overrides both — no wire field
/// was added for this, upstream's request schema stayed `{ enabled }`.
fn remote_control_relay_origin(config: &crate::config::KimiConfig) -> String {
    let configured_oauth = crate::region::ConfiguredOAuthRef::from_config(config);
    let region_relay = crate::region::resolve_kimi_remote_control_relay_origin(
        &configured_oauth,
        crate::workflow::kimi_home().as_deref(),
    );
    crate::server::remote_control::resolve_remote_control_relay_origin(&region_relay)
}

/// The engine's built-in capabilities, served by `GET /api/v1/capabilities` and
/// `GET /api/v1/capabilities/{id}`. A single source so the list route and the
/// detail route cannot drift apart.
const NATIVE_CAPABILITY_IDS: &[&str] = &[
    "bash",
    "file_history",
    "read",
    "write",
    "edit",
    "grep",
    "glob",
    "tools",
    "native_tools",
    "websocket_events",
    "interaction_questions",
    "interaction_approvals",
    "task_runner",
    "cron_scheduler",
    "gui_store",
    "mcp",
    "plugins",
    "terminals",
    "subagents",
];

/// Parses a terminal dimension from a JSON body.
///
/// `as u32` silently wrapped values above `u32::MAX`, and a missing field was
/// accepted as a valid size, so `{"cols": 4294967297}` and `{"cols": 0}` both
/// produced a nonsensical pty. `Ok(None)` means "not supplied".
fn terminal_dimension(value: Option<&Value>, label: &str) -> Result<Option<u32>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let Some(number) = raw.as_u64() else {
        return Err(format!("Field '{label}' must be a non-negative integer"));
    };
    let dimension = u32::try_from(number)
        .map_err(|_| format!("Field '{label}' must be at most {}", u32::MAX))?;
    if dimension == 0 {
        return Err(format!("Field '{label}' must be greater than 0"));
    }
    Ok(Some(dimension))
}

fn prompt_content_to_blocks(
    content: &[Value],
    files: &crate::server::files::FileStore,
) -> Result<(String, Vec<crate::rpc::types::ContentBlock>), String> {
    use crate::rpc::types::ContentBlock;
    let mut texts: Vec<String> = Vec::new();
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for part in content {
        match part
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("")
        {
            "text" => {
                if let Some(text) = part.get("text").and_then(|value| value.as_str()) {
                    texts.push(text.to_string());
                }
            }
            "image" | "video" | "audio" => {
                let kind = part
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("image");
                let Some(source) = part.get("source") else {
                    continue;
                };
                let source_kind = source
                    .get("kind")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let name = part
                    .get("name")
                    .and_then(|value| value.as_str())
                    .map(|s| s.to_string());
                match source_kind {
                    "url" => {
                        if let Some(url) = source.get("url").and_then(|value| value.as_str()) {
                            // A `kimi-file://` URL is a daemon reference, not a
                            // URL a provider could fetch.
                            if let Some(file_id) =
                                crate::llm::media_resolver::parse_daemon_file_url(url)
                            {
                                blocks.push(ContentBlock::MediaRef {
                                    file_id: file_id.to_string(),
                                    kind: media_kind_of(kind),
                                });
                            } else {
                                blocks.push(match kind {
                                    "image" => ContentBlock::ImageUrl {
                                        url: url.to_string(),
                                        id: None,
                                        name,
                                    },
                                    "audio" => ContentBlock::AudioUrl {
                                        url: url.to_string(),
                                        id: None,
                                        name,
                                    },
                                    _ => ContentBlock::VideoUrl {
                                        url: url.to_string(),
                                        id: None,
                                        name,
                                    },
                                });
                            }
                        }
                    }
                    "base64" => {
                        let media_type = source
                            .get("media_type")
                            .and_then(|value| value.as_str())
                            .unwrap_or("application/octet-stream");
                        let data = source
                            .get("data")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        if kind == "image" {
                            // v2 `resolvePromptMediaFiles`: an inline image
                            // over the model's pixel/byte budget is compressed
                            // at intake, the original is persisted, and a
                            // caption naming both precedes the image (ROADMAP
                            // #3747b — the TUI path does this host-side; this
                            // is the HTTP path's half).
                            let (caption, block) = crate::llm::prompt_media::prepare_inline_image(
                                files,
                                media_type,
                                data,
                                name.as_deref(),
                            );
                            if let Some(caption) = caption {
                                blocks.push(ContentBlock::Text { text: caption });
                            }
                            blocks.push(block);
                        } else {
                            let url = format!("data:{media_type};base64,{data}");
                            blocks.push(if kind == "audio" {
                                ContentBlock::AudioUrl {
                                    url,
                                    id: None,
                                    name,
                                }
                            } else {
                                ContentBlock::VideoUrl {
                                    url,
                                    id: None,
                                    name,
                                }
                            });
                        }
                    }
                    "file" | "session_media" => {
                        let file_id = source
                            .get("file_id")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        blocks.push(file_ref_from_store(files, file_id, kind)?);
                    }
                    "path" => {
                        let path = source
                            .get("path")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        let path = Path::new(path);
                        // Materialize the file into the store so the reference
                        // has an identity the resolver and the budget can key
                        // on (v2's prompt intake does the same).
                        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
                        let name = path
                            .file_name()
                            .and_then(|value| value.to_str())
                            .unwrap_or("attachment");
                        let media_type = infer_media_type(path);
                        let meta = files
                            .save(name, &media_type, None, &bytes)
                            .map_err(|error| error.2)?;
                        blocks.push(ContentBlock::MediaRef {
                            file_id: meta.id,
                            kind: media_kind_of(kind),
                        });
                    }
                    _ => {}
                }
            }
            "file" => {
                let name = part.get("name").and_then(|value| value.as_str());
                let media_type = part
                    .get("media_type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if let Some(file_id) = part.get("file_id").and_then(|value| value.as_str()) {
                    match file_ref_from_store(files, file_id, "") {
                        Ok(block) => blocks.push(block),
                        Err(error) => {
                            if let Some(name) = name {
                                blocks.push(ContentBlock::Text {
                                    text: format!("[Attached file: {name} ({media_type})]"),
                                });
                            } else {
                                return Err(error);
                            }
                        }
                    }
                } else if let Some(path) = part.get("path").and_then(|value| value.as_str()) {
                    let path = Path::new(path);
                    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
                    let file_name = name.unwrap_or_else(|| {
                        path.file_name()
                            .and_then(|value| value.to_str())
                            .unwrap_or("attachment")
                    });
                    let resolved_type = if media_type.is_empty() {
                        infer_media_type(path)
                    } else {
                        media_type.to_string()
                    };
                    match media_kind_for_type(&resolved_type) {
                        Some(kind) => {
                            let meta = files
                                .save(file_name, &resolved_type, None, &bytes)
                                .map_err(|error| error.2)?;
                            blocks.push(ContentBlock::MediaRef {
                                file_id: meta.id,
                                kind,
                            });
                        }
                        None => blocks.push(ContentBlock::Text {
                            text: format!(
                                "[Attached file: {file_name} ({resolved_type}, {} bytes)]",
                                bytes.len()
                            ),
                        }),
                    }
                }
            }
            _ => {}
        }
    }
    Ok((texts.join(""), blocks))
}

fn extract_session_fs_action(path: &str) -> Option<(&str, &str)> {
    let rest = path
        .strip_prefix("/api/v1/sessions/")
        .or_else(|| path.strip_prefix("/sessions/"))?;
    if let Some((sess, act)) = rest.split_once("/fs:")
        && !sess.is_empty()
        && !sess.contains('/')
    {
        return Some((sess, act));
    }
    if let Some((sess, act)) = rest.split_once(":fs:")
        && !sess.is_empty()
        && !sess.contains('/')
    {
        return Some((sess, act));
    }
    if let Some((sess, act)) = rest.split_once("/fs/")
        && !sess.is_empty()
        && !sess.contains('/')
    {
        return Some((sess, act));
    }
    None
}

fn format_config_response(cfg: &crate::config::KimiConfig) -> Value {
    let default_model = cfg
        .default_model
        .clone()
        .unwrap_or_else(|| "kimi-latest".into());
    let yolo = cfg.agent.yolo.unwrap_or(false)
        || cfg.yolo.unwrap_or(false)
        || cfg
            .permission
            .as_ref()
            .and_then(|p| p.mode.as_deref())
            .map(|m| m.eq_ignore_ascii_case("yolo"))
            .unwrap_or(false);

    let mut providers_json = serde_json::Map::new();
    for (name, p) in &cfg.providers {
        let has_api_key = p
            .api_key
            .as_ref()
            .map(|k| !k.trim().is_empty())
            .unwrap_or(false);
        providers_json.insert(
            name.clone(),
            json!({
                "type": p.provider_type.as_deref().unwrap_or(""),
                "base_url": p.base_url,
                "default_model": p.default_model,
                "has_api_key": has_api_key,
            }),
        );
    }

    let mut models_json = serde_json::Map::new();
    for (name, m) in &cfg.models {
        models_json.insert(
            name.clone(),
            json!({
                "provider": m.provider,
                "model": m.model,
                "has_api_key": true,
            }),
        );
    }

    json!({
        "default_model": default_model,
        "default_provider": cfg.default_provider,
        "secondary_model": cfg.secondary_model,
        "thinking": cfg.thinking,
        "yolo": yolo,
        "plan_mode": cfg.plan_mode.or(cfg.agent.plan_mode).unwrap_or(false),
        "default_plan_mode": cfg.default_plan_mode.unwrap_or(false),
        "default_permission_mode": cfg.default_permission_mode,
        "permission": cfg.permission,
        "hooks": cfg.hooks,
        "services": cfg.services,
        "merge_all_available_skills": cfg.merge_all_available_skills,
        "extra_skill_dirs": cfg.extra_skill_dirs,
        "extra_agent_dirs": cfg.extra_agent_dirs,
        "loop_control": cfg.loop_control,
        "background": cfg.background,
        "experimental": cfg.experimental,
        "telemetry": cfg.telemetry.unwrap_or(false),
        "model_catalog": {
            "refresh_interval_ms": cfg
                .model_catalog
                .as_ref()
                .and_then(|m| m.refresh_interval_ms)
                .unwrap_or(0),
            "refresh_on_start": cfg
                .model_catalog
                .as_ref()
                .and_then(|m| m.refresh_on_start)
                .unwrap_or(false),
        },
        "providers": providers_json,
        "models": models_json,
        // v2's `toConfigResponse` walks every resolved domain and emits it
        // snake_cased (kap-server/src/routes/config.ts:88-103), so `subagent`
        // belongs here too; `raw` is the fork's round-trip convenience.
        "subagent": cfg.subagent,
        "raw": serde_json::to_value(cfg).unwrap_or(Value::Null),
    })
}

/// Project one stored `LLMMessage` onto v2's `messageSchema`
/// (kap-server/src/protocol/message.ts:108-117): `id` / `session_id` / `role` /
/// `content[]` blocks / `created_at`, with `prompt_id` and `parent_message_id`
/// optional.
///
/// The fork stores flat messages (`role` + text `content`, plus structural
/// `blocks` / `tool_calls`), so this restores the block array v2 clients read.
/// `created_at` is not persisted per message — the schema requires a string, so
/// an empty one is a visible marker rather than a fabricated timestamp.
fn project_wire_message(
    session_id: &str,
    index: usize,
    message: &crate::turn_loop::types::LLMMessage,
) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    if !message.content.is_empty() {
        blocks.push(json!({ "type": "text", "text": message.content }));
    }
    for block in &message.blocks {
        blocks.push(serde_json::to_value(block).unwrap_or(Value::Null));
    }
    for call in &message.tool_calls {
        blocks.push(json!({
            "type": "tool_use",
            "tool_call_id": call.id,
            "tool_name": call.name,
            "input": call.arguments,
        }));
    }
    if message.role == "tool"
        && let Some(call_id) = message.tool_call_id.as_deref()
    {
        blocks.push(json!({
            "type": "tool_result",
            "tool_call_id": call_id,
            "output": message.content,
        }));
    }
    json!({
        "id": format!("{session_id}-{index}"),
        "session_id": session_id,
        "role": message.role,
        "content": blocks,
        "created_at": "",
    })
}

/// What this route may put in the archive.
///
/// This is the **web client's** exporter, not the CLI's: `/export-debug-zip`
/// calls the host's `exportSession`, which walks the on-disk session directory
/// (`session-meta.json`, `history.jsonl`, `agents/`, …) — a host-owned
/// artifact this engine never sees. The two archives are therefore complementary
/// rather than comparable: this one ships what SQLite holds, plus the same
/// `export-manifest.json` head the host writes and, on request, the
/// home-scoped global log.
#[derive(Debug, Clone, Default)]
pub struct SessionExportOptions {
    /// Mirror the host's `includeGlobalLog`. Off unless asked for: the global
    /// log and its rotations are home-scoped and can run to tens of megabytes,
    /// which is why the host defaults to excluding them too.
    pub include_global_log: bool,
}

/// The home-scoped global log and its `.N` rotations, under the member names
/// the host's exporter and `apps/vis` both expect (`logs/kimi-code.log`, …).
///
/// Same selection the host makes (`readdir`, keep `kimi-code.log*`, sort
/// lexicographically), so the two exporters agree on what "the global log" is.
fn collect_global_log_members() -> Vec<(String, Vec<u8>)> {
    let Some(home) = crate::workflow::kimi_home() else {
        return Vec::new();
    };
    global_log_members_in(&home.join("logs"))
}

/// The same collection against an explicit directory, so the selection and the
/// member naming are testable without an environment.
fn global_log_members_in(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let entries = std::fs::read_dir(dir);
    let Ok(entries) = entries else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("kimi-code.log"))
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            std::fs::read(dir.join(&name))
                .ok()
                .map(|body| (format!("logs/{name}"), body))
        })
        .collect()
}

/// Pack a session export into the ZIP the Web client's `exportSession` asks for.
///
/// The bundle posts to `…/export` through a bespoke transport that hard-fails
/// unless the response is `application/zip`; answering JSON made the whole
/// action report a parse error. `session.json` (the full export document) and
/// `transcript.md` (a readable rendering) are this engine's own value; the
/// `export-manifest.json` head and the optional log members are what the host's
/// exporter sends, so a bundle from either path describes itself the same way.
fn build_session_export_zip(
    export: &crate::session::sqlite_store::SessionExport,
    options: &SessionExportOptions,
) -> Result<Vec<u8>, String> {
    let global_logs = if options.include_global_log {
        collect_global_log_members()
    } else {
        Vec::new()
    };
    build_session_export_zip_with(export, global_logs)
}

/// The archive layout with the log members already resolved, so it can be
/// tested without a home directory or an environment.
fn build_session_export_zip_with(
    export: &crate::session::sqlite_store::SessionExport,
    global_logs: Vec<(String, Vec<u8>)>,
) -> Result<Vec<u8>, String> {
    use std::io::Write;

    let document = serde_json::to_vec_pretty(export).map_err(|e| e.to_string())?;
    let mut markdown = String::new();
    let title = export.session.title.as_deref().unwrap_or("Session");
    markdown.push_str(&format!("# {title}\n\n"));
    markdown.push_str(&format!(
        "- Session: `{}`\n- Turns: {}\n- Exported: {}\n\n",
        export.session.session_id, export.turns_count, export.exported_at
    ));
    for message in &export.messages {
        let (heading, content) = match message.role.as_str() {
            "user" => ("User", &message.content),
            "assistant" => ("Assistant", &message.content),
            "system" => ("System", &message.content),
            other => (other, &message.content),
        };
        markdown.push_str(&format!("## {heading}\n\n{content}\n\n"));
    }

    let mut manifest = serde_json::Map::new();
    manifest.insert("sessionId".into(), json!(export.session.session_id));
    manifest.insert("exportedAt".into(), json!(export.exported_at));
    manifest.insert("kimiCodeVersion".into(), json!(env!("CARGO_PKG_VERSION")));
    manifest.insert("os".into(), json!(std::env::consts::OS));
    // The host names the primary entry and omits the key entirely when the log
    // is not bundled — its e2e pins that absence, so never write a null here.
    if let Some(primary) = global_logs
        .iter()
        .map(|(name, _)| name.clone())
        .find(|name| *name == "logs/kimi-code.log")
        .or_else(|| global_logs.first().map(|(name, _)| name.clone()))
    {
        manifest.insert("globalLogPath".into(), json!(primary));
    }
    let manifest =
        serde_json::to_vec_pretty(&Value::Object(manifest)).map_err(|e| e.to_string())?;

    let mut buffer = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let zip_options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer
            .start_file("export-manifest.json", zip_options)
            .map_err(|e| e.to_string())?;
        writer.write_all(&manifest).map_err(|e| e.to_string())?;
        writer
            .start_file("session.json", zip_options)
            .map_err(|e| e.to_string())?;
        writer.write_all(&document).map_err(|e| e.to_string())?;
        writer
            .start_file("transcript.md", zip_options)
            .map_err(|e| e.to_string())?;
        writer
            .write_all(markdown.as_bytes())
            .map_err(|e| e.to_string())?;
        for (name, body) in &global_logs {
            writer
                .start_file(name, zip_options)
                .map_err(|e| e.to_string())?;
            writer.write_all(body).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(buffer)
}

/// One built-in capability in the shape the Web client reads. Shared by the
/// list and detail routes so the two cannot drift into serving different
/// element types again.
fn native_capability_wire(id: &str) -> Value {
    json!({
        "id": id,
        "displayName": id,
        "description": format!("Built-in {id} capability of the native engine."),
        "supported": true,
        "state": "ready",
        "version": env!("CARGO_PKG_VERSION"),
        "steps": [],
        "install": { "progress": 100 }
    })
}

/// One OAuth flow, shaped after v2's `oauthFlowSnapshotSchema` /
/// `oauthFlowStartSchema` (`agent-core-v2/src/app/auth/oauthProtocol.ts:5-53`):
/// snake_case throughout, `status` from
/// `pending | authenticated | denied | expired | cancelled`, and `resolved_at`
/// present only once the flow has resolved.
///
/// The Rust `OAuthFlowSnapshot` is camelCase and names the terminal success
/// `"success"`, so answering it verbatim left `flow_id` undefined and the
/// completion branch (`status === "authenticated"`) unreachable — a finished
/// sign-in never registered. `provider` is the flow's key in the manager, which
/// is the handle callers poll and cancel with.
fn oauth_flow_wire(
    flow: Option<&crate::server::oauth::OAuthFlowSnapshot>,
    provider: &str,
) -> Value {
    let Some(flow) = flow else {
        // v2's GET answers `oauthFlowSnapshotOrNullSchema`; "nothing running"
        // is `null`, which is how the client reads it.
        return Value::Null;
    };
    let status = if flow.status == "success" {
        "authenticated"
    } else if flow.status == "error" {
        // v2's `oauthFlowStatusEnum` has no `error` member
        // (agent-core-v2/src/app/auth/oauthProtocol.ts:5-11): a failed exchange
        // surfaces as `denied` with `error_message` carrying the reason, which
        // is what the client's terminal-state branch matches on.
        "denied"
    } else {
        flow.status.as_str()
    };
    let expires_at = flow.expires_in.map(|secs| {
        chrono::DateTime::from_timestamp_millis(
            chrono::Utc::now().timestamp_millis() + secs as i64 * 1000,
        )
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
    });
    let mut body = json!({
        "flow_id": provider,
        "provider": flow.provider,
        "status": status,
        "user_code": flow.user_code,
        "verification_uri": flow.verification_uri,
        "verification_uri_complete": flow.verification_uri_complete,
        "expires_in": flow.expires_in,
        "expires_at": expires_at,
    });
    // v2's successful *start* is a two-field variant (`flow_id` + `provider` +
    // `authenticated`); the pending variant and the snapshot both carry the
    // device-code details, and only the snapshot carries `resolved_at`.
    match body.as_object_mut() {
        Some(object) if status == "authenticated" => {
            object.retain(|key, _| matches!(key.as_str(), "flow_id" | "provider" | "status"));
        }
        Some(object) => {
            if matches!(status, "denied" | "expired" | "cancelled") {
                let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                object.insert("resolved_at".into(), json!(now));
            }
            if let Some(error) = &flow.error_message {
                object.insert("error_message".into(), json!(error));
            }
        }
        None => {}
    }
    body
}

/// The static directory the catalog routes advertise when the models.dev
/// fetch fails: the built-in snapshot (v2 `BUILT_IN_MODELS_DEV_JSON`), mapped
/// through the same item projection a fetched catalog rides, so an entry
/// cannot exist in one and be missing (or fabricated) in the other.
fn catalog_provider_items() -> Value {
    models_dev::builtin_items()
}

mod route_interaction;
mod route_models_providers;
mod route_plugins_oauth;
mod route_prompts;
mod route_prompts_files;
mod route_session_io;
mod route_session_ops;
mod route_sessions;
mod route_sessions_v2;
mod route_subagents_gui;
mod route_system;
mod route_tools_mcp;
mod route_workspaces;
mod route_workspaces_fs;

/// Apply the client-requested JSON envelope to a response a route domain produced.
/// Early returns inside an arm never reach here — the giant match behaved the same way.
pub(super) fn finalize_response(req: &HttpRequest, resp: HttpResponse) -> HttpResponse {
    if req.wants_envelope() && resp.header("content-type") == Some("application/json") {
        let req_id = req.request_id();
        if (resp.status == 200 || resp.status == 201)
            && let Ok(val) = serde_json::from_slice::<Value>(&resp.body)
            && val.get("code").is_none()
        {
            return HttpResponse::envelope_ok(&val, &req_id);
        } else if resp.status >= 400
            && let Ok(val) = serde_json::from_slice::<Value>(&resp.body)
            && val.get("code").is_none()
        {
            let err_msg = val.get("error").and_then(|e| e.as_str()).unwrap_or("Error");
            let err_code = match resp.status {
                400 => envelope::error_codes::VALIDATION_FAILED,
                404 => envelope::error_codes::SESSION_NOT_FOUND,
                _ => envelope::error_codes::INTERNAL_ERROR,
            };
            return HttpResponse::envelope_err(resp.status, err_code, err_msg, &req_id);
        }
    }

    resp
}

#[cfg(test)]
mod tests;
