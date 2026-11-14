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
pub mod idle_sessions;
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

pub fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]") || host.starts_with("127.")
}

pub type SessionCleanupHook = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

pub struct HttpServer {
    store: Arc<SqliteSessionStore>,
    hub: Arc<EventHub>,
    wire_journal: Option<Arc<crate::session::wire_journal::WireJournal>>,
    engine: Option<Arc<ServerEngine>>,
    auth: ServerAuth,
    heartbeat: Duration,
    cron_scheduler: Arc<Mutex<CronScheduler>>,
    task_runner: Arc<TaskRunner>,

    task_wake_started: bool,
    server_id: String,
    started_at: String,
    web_assets_dir: Option<PathBuf>,
    mcp_manager: Arc<crate::mcp::manager::McpManager>,

    mcp_oauth: Arc<tokio::sync::Mutex<Option<Arc<crate::mcp::oauth::McpOAuthService>>>>,

    mcp_auth_code: Arc<crate::mcp::oauth::auth_code::AuthCodeRegistry>,
    interaction_manager: Arc<interaction::InteractionManager>,
    plugin_manager: Arc<plugins::PluginManager>,
    oauth_manager: Arc<oauth::OAuthManager>,

    prompt_queue: Arc<prompt_queue::PromptQueue>,
    config_override: Arc<Mutex<Option<crate::config::KimiConfig>>>,

    model_refresh_chain: tokio::sync::Mutex<()>,

    hooks_source: crate::tools::external_hooks::HooksSource,

    models_dev_cache: Arc<models_dev::CatalogCache>,

    config_write_path: std::sync::Mutex<Option<PathBuf>>,

    file_store: files::FileStore,
    terminal_manager: Arc<terminal::TerminalManager>,
    subagent_manager: Arc<crate::subagent::SubagentManager>,

    remote_control_state: Arc<Mutex<RemoteControlStatusWire>>,

    remote_control_runtime:
        Arc<tokio::sync::Mutex<Option<crate::server::remote_control::RemoteControlHandle>>>,

    remote_control_devices: Arc<remote_control::RemoteControlDeviceClient>,

    remote_control_logout_gate: Arc<remote_control::RemoteControlLogoutGate>,

    shutdown: tokio_util::sync::CancellationToken,

    loopback_bind: bool,

    debug_endpoints: bool,

    allow_remote_shutdown: bool,

    host_guard: Option<HostGuard>,

    telemetry_sink: Option<crate::tools::external_hooks::TelemetrySink>,

    reclaimed: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,

    resume_slots:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,

    session_cleanup_hooks: Arc<std::sync::Mutex<Vec<SessionCleanupHook>>>,

    idle_reaper: std::sync::OnceLock<
        Arc<crate::session::idle_reclaim::SessionIdleReaper<idle_sessions::ServerIdleControl>>,
    >,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopSnapshot {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_turn_id: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_prompt_id: Option<String>,

    pub queued_prompt_ids: Vec<String>,

    pub notification_count: usize,
    pub paused: bool,

    pub has_pending_requests: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<LoopTurnSnapshot>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopTurnSnapshot {
    pub turn_id: u32,

    pub phase: &'static str,
    pub step: u32,
    pub ending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ending_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<LoopRetrySnapshot>,
    pub active_tool_calls: Vec<LoopToolCallSnapshot>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopRetrySnapshot {
    pub failed_attempt: u32,
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u32>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoopToolCallSnapshot {
    pub tool_call_id: String,
    pub name: String,
}

impl LoopTurnSnapshot {
    fn from_phase(
        turn_id: Option<u64>,
        phase: &activity::AgentPhase,
        step: u32,
        ending: bool,
        since: Option<u64>,
    ) -> Option<Self> {
        let turn_id = u32::try_from(turn_id?).ok()?;
        let (phase, retry, active_tool_calls) = match phase {
            activity::AgentPhase::ToolCall {
                tool_call_id, name, ..
            } => (
                "tool_call",
                None,
                vec![LoopToolCallSnapshot {
                    tool_call_id: tool_call_id.clone(),
                    name: name.clone(),
                }],
            ),
            activity::AgentPhase::Retrying {
                failed_attempt,
                next_attempt,
                max_attempts,
                delay_ms,
                error_name,
                status_code,
                ..
            } => (
                "retrying",
                Some(LoopRetrySnapshot {
                    failed_attempt: *failed_attempt,
                    next_attempt: *next_attempt,
                    max_attempts: *max_attempts,
                    delay_ms: *delay_ms,
                    error_name: error_name.clone(),
                    status_code: *status_code,
                }),
                Vec::new(),
            ),

            _ => ("running", None, Vec::new()),
        };
        Some(Self {
            turn_id,
            phase,
            step,
            ending,
            ending_reason: ending.then_some("aborted"),
            retry,
            active_tool_calls,
            since,
        })
    }
}

impl HttpServer {
    pub fn loop_snapshot(&self, session_id: &str) -> LoopSnapshot {
        let active_prompt_id = self.prompt_queue.active_tracked_prompt_id(session_id);
        let queued_prompt_ids = self.prompt_queue.queued_tracked_prompt_ids(session_id);
        let notification_count = self.prompt_queue.queued_notification_count(session_id);
        let Some(engine) = self.engine.as_ref() else {
            return LoopSnapshot {
                state: "idle",
                active_turn_id: None,
                active_prompt_id,
                queued_prompt_ids,
                notification_count,
                paused: false,
                has_pending_requests: self
                    .interaction_manager
                    .pending_interaction_kind(session_id)
                    .is_some(),
                turn: None,
            };
        };
        let phase = engine.activity_phase(session_id);
        let running = engine.is_turn_active(session_id);
        let active_turn_id = engine.active_turn_number(session_id);
        let turn = running
            .then(|| {
                let tracker = engine.activity_tracker(session_id);
                LoopTurnSnapshot::from_phase(
                    active_turn_id
                        .map(u64::from)
                        .or_else(|| activity::phase_turn_id(&phase)),
                    &phase,
                    tracker.turn_step(),
                    engine.is_turn_ending(session_id),
                    tracker.turn_started_ms(),
                )
            })
            .flatten();
        LoopSnapshot {
            state: if running { "running" } else { "idle" },
            active_turn_id,
            active_prompt_id,
            queued_prompt_ids,
            notification_count,
            paused: false,
            has_pending_requests: self
                .interaction_manager
                .pending_interaction_kind(session_id)
                .is_some()
                || activity::phase_has_pending_requests(&phase),
            turn,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoteControlConflictWire {
    pub pid: u32,
    pub local_origin: String,
    pub device_id: String,
    pub url: String,
    pub started_at: u64,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<RemoteControlConflictWire>,
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
            conflict: None,
        }
    }
}

mod builder;

pub use builder::{HttpServerWakeDriver, WakeEnqueueError};
mod catalog_routes;
mod dispatch;

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

fn prompt_origin_from_metadata(client_metadata: Option<&Value>) -> Option<Value> {
    let metadata = client_metadata?;
    let inner = &metadata[0];
    inner
        .get("origin")
        .filter(|origin| origin.get("kind").and_then(Value::as_str).is_some())
        .cloned()
        .or_else(|| Some(json!({ "kind": "user", "clientMetadata": metadata })))
}

fn media_kind_of(kind: &str) -> crate::rpc::types::MediaKind {
    match kind {
        "video" => crate::rpc::types::MediaKind::Video,
        "audio" => crate::rpc::types::MediaKind::Audio,
        _ => crate::rpc::types::MediaKind::Image,
    }
}

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

        None if !kind_hint.is_empty() => media_kind_of(kind_hint),

        None => {
            return Ok(crate::rpc::types::ContentBlock::Text {
                text: format!(
                    "[Attached file: {} ({media_type}, {} bytes)]",
                    meta.name, meta.size
                ),
                meta: None,
            });
        }
    };
    Ok(crate::rpc::types::ContentBlock::MediaRef {
        file_id: file_id.to_string(),
        kind,
    })
}

async fn apply_prompt_submission_options(
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

    let swarm_was_on = agent_config
        .get("swarm_mode")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let hello_world_was_on = agent_config
        .get("hello_world_mode")
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
        if let Some(hello_world_mode) = body.get("hello_world_mode").and_then(|v| v.as_bool()) {
            if hello_world_was_on != hello_world_mode && server.engine.is_none() {
                server.emit_session_telemetry_named(if hello_world_mode {
                    "hello_world_mode_entered"
                } else {
                    "hello_world_mode_exited"
                });
            }
            config.insert("hello_world_mode".to_string(), json!(hello_world_mode));
        }
        if let Some(disabled) = body.get("disabled_tools").and_then(|v| v.as_array()) {
            let mut seen = std::collections::HashSet::new();
            let names: Vec<Value> = disabled
                .iter()
                .filter(|value| {
                    value
                        .as_str()
                        .is_some_and(|name| seen.insert(name.to_string()))
                })
                .cloned()
                .collect();
            config.insert("disabled_tools".to_string(), Value::Array(names));
        }
    }

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
    let mut recorded_permission_mode = None;
    {
        let meta = metadata.as_object_mut().unwrap();
        if let Some(extra) = body.get("metadata").and_then(|v| v.as_object()) {
            for (key, value) in extra {
                meta.insert(key.clone(), value.clone());
            }
        }
        if let Some(mode) = body.get("permission_mode").and_then(|v| v.as_str()) {
            let Some(parsed) = crate::agent::permission_mode::permission_mode_ops::parse_mode(mode)
            else {
                return Err(format!("invalid permission_mode: {mode}"));
            };
            recorded_permission_mode = Some(parsed);
            meta.insert("permission_mode".to_string(), json!(mode));
        }
    }
    if let Err(e) = server.store().put_session_metadata(session_id, &metadata) {
        return Err(format!("failed to persist session metadata: {e}"));
    }
    if let Some(mode) = recorded_permission_mode {
        let now = chrono::Utc::now().timestamp_millis();
        match crate::agent::permission_mode::permission_mode_ops::record_mode(
            server.store(),
            session_id,
            crate::callbacks::MAIN_AGENT_ID,
            mode,
            now,
        ) {
            Ok(true) => {
                if let Some(journal) = server.wire_journal.as_ref() {
                    journal.append(
                        session_id,
                        &crate::events::EngineEvent::Custom(
                            crate::agent::permission_mode::permission_mode_ops::permission_set_mode_event(
                                crate::callbacks::MAIN_AGENT_ID,
                                mode,
                            ),
                        ),
                        now,
                    );
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    %session_id,
                    %error,
                    "failed to persist the durable permission.set_mode record"
                );
            }
        }
    }

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

    if let Some(hello_world_mode) = body.get("hello_world_mode").and_then(|v| v.as_bool()) {
        let agent_id = crate::callbacks::MAIN_AGENT_ID;
        if let Some(engine) = server.engine.as_ref() {
            let callbacks = engine.session_callbacks_for(session_id);
            if hello_world_mode {
                let workdir = fs_routes::resolve_session_workdir(server.store(), session_id)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::enter_scoped(
                    callbacks.as_ref(),
                    crate::features::helloWorld::agent::helloWorldService::HelloWorldScope {
                        agent_id,
                        session_id,
                        workspace_root: &workdir,
                    },
                    crate::features::helloWorld::agent::helloWorld::HelloWorldModeTrigger::Manual,
                    None,
                )
                .await;
            } else {
                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::exit(
                    callbacks.as_ref(),
                    agent_id,
                );
            }
        } else {
            let events = if hello_world_mode {
                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::enter_events(
                    agent_id,
                    crate::features::helloWorld::agent::helloWorld::HelloWorldModeTrigger::Manual,
                    None,
                )
            } else {
                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::exit_events(
                    agent_id,
                )
                .map(|(_, events)| events)
            };
            for event in events.into_iter().flatten() {
                server
                    .hub
                    .bus_for(session_id)
                    .publish(&crate::events::EngineEvent::Custom(event));
            }
        }
    }
    Ok(())
}

fn swarm_trigger_from(raw: Option<&str>) -> crate::swarm::mode::SwarmModeTrigger {
    use crate::swarm::mode::SwarmModeTrigger;
    match raw {
        Some("task") => SwarmModeTrigger::Task,
        Some("tool") => SwarmModeTrigger::Tool,
        _ => SwarmModeTrigger::Manual,
    }
}

const REMOTE_CONTROL_UNAVAILABLE: &str = "remote-control is not running: the runtime exists (device registration, relay channel, heartbeat) but no session started it — POST /api/v1/remote-control with enabled=true and the Kimi login refresh_token to start one";

fn new_device_id() -> String {
    ulid::Ulid::new().to_string()
}

fn remote_control_relay_origin(config: &crate::config::KimiConfig) -> String {
    let configured_oauth = crate::region::ConfiguredOAuthRef::from_config(config);
    let region_relay = crate::region::resolve_kimi_remote_control_relay_origin(
        &configured_oauth,
        crate::workflow::kimi_home().as_deref(),
    );
    crate::server::remote_control::resolve_remote_control_relay_origin(&region_relay)
}

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
    provider: Option<&str>,
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
                            if let Some(file_id) =
                                crate::llm::media_resolver::parse_daemon_file_url(url)
                            {
                                blocks.push(ContentBlock::MediaRef {
                                    file_id: file_id.to_string(),
                                    kind: media_kind_of(kind),
                                });
                            } else if kind == "image" {
                                blocks.extend(crate::llm::prompt_media::image_blocks_from_url(
                                    files,
                                    provider,
                                    url,
                                    name.as_deref(),
                                ));
                            } else {
                                blocks.push(match kind {
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
                            blocks.extend(crate::llm::prompt_media::image_blocks_from_base64(
                                files,
                                provider,
                                media_type,
                                data,
                                name.as_deref(),
                            ));
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
                                    meta: None,
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
                            meta: None,
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



        "subagent": cfg.subagent,
        "raw": serde_json::to_value(cfg).unwrap_or(Value::Null),
    })
}

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

#[derive(Debug, Clone, Default)]
pub struct SessionExportOptions {
    pub include_global_log: bool,
}

fn collect_global_log_members() -> Vec<(String, Vec<u8>)> {
    let Some(home) = crate::workflow::kimi_home() else {
        return Vec::new();
    };
    global_log_members_in(&home.join("logs"))
}

fn global_log_members_in(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    log_members_in(dir, "logs/global")
}

fn collect_session_log_members(session_log: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let Some(dir) = session_log.parent() else {
        return Vec::new();
    };
    log_members_in(dir, "logs")
}

fn log_members_in(dir: &std::path::Path, member_prefix: &str) -> Vec<(String, Vec<u8>)> {
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
                .map(|body| (format!("{member_prefix}/{name}"), body))
        })
        .collect()
}

fn build_session_export_zip(
    export: &crate::session::sqlite_store::SessionExport,
    options: &SessionExportOptions,
    session_log: Option<&std::path::Path>,
) -> Result<Vec<u8>, String> {
    let session_logs = session_log
        .map(collect_session_log_members)
        .unwrap_or_default();
    let global_logs = if options.include_global_log {
        collect_global_log_members()
    } else {
        Vec::new()
    };
    build_session_export_zip_with(export, session_logs, global_logs)
}

fn build_session_export_zip_with(
    export: &crate::session::sqlite_store::SessionExport,
    session_logs: Vec<(String, Vec<u8>)>,
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

    manifest.insert(
        "wireProtocolVersion".into(),
        json!(crate::native::event_store::WIRE_PROTOCOL_VERSION),
    );
    manifest.insert(
        "os".into(),
        json!(format!(
            "{} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    );
    if let Some(title) = &export.session.title {
        manifest.insert("title".into(), json!(title));
    }
    if let Some(dir) = &export.session.cwd {
        manifest.insert("workspaceDir".into(), json!(dir));
    }
    if let Some(first) = &export.first_activity_at {
        manifest.insert("sessionFirstActivity".into(), json!(first));
    }
    if let Some(last) = &export.last_activity_at {
        manifest.insert("sessionLastActivity".into(), json!(last));
    }

    if session_logs
        .iter()
        .any(|(name, _)| name == "logs/kimi-code.log")
    {
        manifest.insert("sessionLogPath".into(), json!("logs/kimi-code.log"));
    }
    if let Some(primary) = global_logs
        .iter()
        .map(|(name, _)| name.clone())
        .find(|name| *name == "logs/global/kimi-code.log")
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

        writer
            .start_file("agents/main/wire.jsonl", zip_options)
            .map_err(|e| e.to_string())?;
        writer
            .write_all(export.wire_jsonl.join("\n").as_bytes())
            .map_err(|e| e.to_string())?;
        if !export.wire_jsonl.is_empty() {
            writer.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        for (agent_id, lines) in &export.agent_wires {
            if agent_id == "main" || lines.is_empty() {
                continue;
            }
            writer
                .start_file(format!("agents/{agent_id}/wire.jsonl"), zip_options)
                .map_err(|e| e.to_string())?;
            writer
                .write_all(lines.join("\n").as_bytes())
                .map_err(|e| e.to_string())?;
            writer.write_all(b"\n").map_err(|e| e.to_string())?;
        }
        for (name, body) in session_logs.iter().chain(global_logs.iter()) {
            writer
                .start_file(name, zip_options)
                .map_err(|e| e.to_string())?;
            writer.write_all(body).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(buffer)
}

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

fn oauth_flow_wire(
    flow: Option<&crate::server::oauth::OAuthFlowSnapshot>,
    provider: &str,
) -> Value {
    let Some(flow) = flow else {
        return Value::Null;
    };
    let status = if flow.status == "success" {
        "authenticated"
    } else if flow.status == "error" {
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
