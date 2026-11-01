//! Native ACP (Agent Client Protocol) dispatcher.
//!
//! `AcpServer::handle_message` speaks the JSON-RPC 2.0 envelope (parse error /
//! invalid request / method not found, notifications never answered) and
//! answers the ACP handshake with the spec shape: numeric `protocolVersion`,
//! `agentCapabilities`, `authMethods` (types.rs). `session/prompt` runs a real
//! turn through `ServerEngine` when one is attached and accepts both the
//! string and the ACP `ContentBlock[]` prompt form.
//!
//! Outbound traffic goes through [`AcpChannel`]: `session/update`
//! notifications while a turn runs, plus server-initiated requests
//! (`session/request_permission`, see `permission.rs`).
//!
//! Still missing: the auth gate.

pub mod builtin_commands;
pub mod channel;
pub mod events_map;
#[cfg(test)]
mod events_map_golden;
pub mod permission;
pub mod question;
pub mod types;

use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::acp::types::{
    AcpClientCapabilities, AcpInitializeParams, JsonRpcRequest, JsonRpcResponse, acp_modes,
    negotiate_protocol_version,
};
use crate::events::bus::{EventBus, Subscription};
use crate::mcp::manager::{McpServerOptions, McpServerRecipe};
use crate::session::sqlite_store::SqliteSessionStore;

pub use channel::{AcpChannel, AcpOutbound};

pub struct AcpServer {
    store: Arc<SqliteSessionStore>,
    engine: Option<Arc<crate::server::engine::ServerEngine>>,
    channel: AcpChannel,
    /// Current ACP mode per session (v2 `AcpSession.currentModeId`).
    modes: Mutex<HashMap<String, String>>,
    /// In-flight `session/prompt` requests, keyed by the JSON-RPC request id
    /// rendered as a string, valued by session id. A `$/cancel_request` names
    /// the request to abort, so this is how the notification finds the turn.
    in_flight_prompts: Mutex<HashMap<String, String>>,
    /// Client capabilities declared on `initialize` (drives which reverse
    /// RPCs — client fs / terminal — the agent may issue). Shared with the
    /// session host so an ACP-advertised fs can own Read/Write execution.
    client_capabilities: Arc<std::sync::Mutex<AcpClientCapabilities>>,
    /// Bypass the auth gate (v2 `disableAuth`, server.ts:110-114). The Rust
    /// engine has no runtime auth state, so the gate is "authed iff an engine
    /// (model) is attached"; tests and the no-engine dev path set this true.
    disable_auth: bool,
    /// Last title announced per session, so a `session_info_update` fires only
    /// when the stored title actually changed (external rename / generated
    /// title). `None` is a real value: a session with no title.
    last_titles: Mutex<HashMap<String, Option<String>>>,
}

impl AcpServer {
    /// Bypass the auth gate (v2 `disableAuth`). The no-engine dev/test path
    /// (canned prompt, no native LLM) calls this so sessions are not refused.
    pub fn set_disable_auth(&mut self, value: bool) {
        self.disable_auth = value;
    }

    pub fn new(store: Arc<SqliteSessionStore>) -> Self {
        Self {
            store,
            engine: None,
            channel: AcpChannel::new(),
            modes: Mutex::new(HashMap::new()),
            in_flight_prompts: Mutex::new(HashMap::new()),
            client_capabilities: Arc::new(std::sync::Mutex::new(AcpClientCapabilities::default())),
            disable_auth: false,
            last_titles: Mutex::new(HashMap::new()),
        }
    }

    /// Attach an engine shared with other servers without rebinding its
    /// host factory. [`with_engine`] installs a factory that routes turn
    /// permission prompts and client-fs execution to *this* server's
    /// channel — but the factory slot is engine-global, so a per-request
    /// server (e.g. the HTTP ACP bridge) must not overwrite it: that would
    /// steal permission/fs-bridge routing from concurrent users, including
    /// the dedicated stdio ACP server. Turns started through this handle
    /// fall back to the engine's interaction-manager host instead.
    pub fn with_shared_engine(
        store: Arc<SqliteSessionStore>,
        engine: Arc<crate::server::engine::ServerEngine>,
    ) -> Self {
        Self {
            store,
            engine: Some(engine),
            channel: AcpChannel::new(),
            modes: Mutex::new(HashMap::new()),
            in_flight_prompts: Mutex::new(HashMap::new()),
            client_capabilities: Arc::new(std::sync::Mutex::new(AcpClientCapabilities::default())),
            disable_auth: false,
            last_titles: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_engine(
        store: Arc<SqliteSessionStore>,
        engine: Arc<crate::server::engine::ServerEngine>,
    ) -> Self {
        // Every turn of this engine asks the ACP client before executing a
        // mutating tool (v2 `interaction-bridge.ts` + `approval.ts`), and an
        // ACP client that advertises fs capabilities owns Read/Write (their
        // execution is forwarded to the client, v2
        // `packages/acp-server/src/acp-fs/acpFsService.ts`).
        let channel = AcpChannel::new();
        let capabilities = Arc::new(std::sync::Mutex::new(AcpClientCapabilities::default()));
        let factory_channel = channel.clone();
        let factory_capabilities = capabilities.clone();
        // The reverse Bash path runs in the client's terminal, which needs the
        // session's cwd (v2 `AcpSessionRuntime` is constructed with it).
        let factory_store = store.clone();
        engine.set_host_factory(Arc::new(move |session_id: &str| {
            Arc::new(permission::AcpPermissionHost::new(
                Arc::new(crate::server::engine::ServerHost::standalone()),
                factory_channel.clone(),
                session_id.to_string(),
                factory_capabilities.clone(),
                session_cwd_of(&factory_store, session_id),
            ))
        }));
        Self {
            store,
            engine: Some(engine),
            channel,
            modes: Mutex::new(HashMap::new()),
            in_flight_prompts: Mutex::new(HashMap::new()),
            client_capabilities: capabilities,
            disable_auth: false,
            last_titles: Mutex::new(HashMap::new()),
        }
    }

    /// The session's current ACP mode, defaulting to `default`.
    fn current_mode(&self, session_id: &str) -> String {
        self.modes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| "default".to_string())
    }

    /// The model this session runs on: the `agent_config` override the
    /// per-session model switch writes, else the engine's own model.
    fn session_model(&self, session_id: &str) -> String {
        self.store
            .get_state("agent_config", session_id)
            .ok()
            .flatten()
            .and_then(|profile| {
                profile
                    .get("model")
                    .and_then(|value| value.as_str())
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
            })
            .or_else(|| {
                self.engine
                    .as_ref()
                    .map(|engine| engine.model_name().to_string())
            })
            .unwrap_or_default()
    }

    /// The merged slash palette: ACP builtins first (they always win), then
    /// the session's user-activatable skills (v2 `resolveCommands`).
    async fn available_commands(&self, session_id: &str) -> Vec<serde_json::Value> {
        let mut commands = builtin_commands::palette();
        if let Some(engine) = &self.engine {
            for command in
                builtin_commands::skill_commands(&engine.session_skills(session_id).await)
            {
                commands.push(json!({
                    "name": command.name,
                    "description": command.description,
                }));
            }
        }
        commands
    }

    /// Push the merged command palette (v2 `emitAvailableCommandsUpdate`).
    async fn emit_available_commands(&self, session_id: &str) {
        let commands = self.available_commands(session_id).await;
        self.channel.notify(
            "session/update",
            events_map::available_commands_notification(session_id, &commands),
        );
    }

    /// The skill a slash command activates for this session, resolved against
    /// the same catalog the palette advertises (v2 `resolveSkillCommand`:
    /// both the bare name and the `skill:`-prefixed spelling resolve).
    async fn resolve_skill_command(&self, session_id: &str, command_name: &str) -> Option<String> {
        let engine = self.engine.as_ref()?;
        let commands = builtin_commands::skill_commands(&engine.session_skills(session_id).await);
        let prefixed = format!("skill:{command_name}");
        commands
            .iter()
            .find(|command| command.name == command_name || command.name == prefixed)
            .map(|command| command.skill.clone())
    }

    /// Push `session_info_update` when the stored title changed. `force` sends
    /// it on attach (`session/new` / `load` / `resume`) even when it is `null`,
    /// matching v2's "clear the client-side title" semantics.
    fn emit_session_info(&self, session_id: &str, force: bool) {
        let title = self
            .store
            .get_session(session_id)
            .ok()
            .flatten()
            .and_then(|summary| summary.title);
        {
            let mut last = self.last_titles.lock().unwrap_or_else(|e| e.into_inner());
            if !force && last.get(session_id) == Some(&title) {
                return;
            }
            last.insert(session_id.to_string(), title.clone());
        }
        self.channel.notify(
            "session/update",
            events_map::session_info_update_notification(session_id, title.as_deref()),
        );
    }

    /// Deliver background-task completions as their own turns (v2
    /// `taskService.notifyAgentTask` → `loop.notify`): the ACP prompt path does
    /// not use the REST prompt queue, so this is what turns a queued completion
    /// into a model turn. One batch per turn; a task that finishes later rings
    /// the runner's bell for [`Self::task_wake_loop`].
    async fn run_task_notifications(
        &self,
        engine: &Arc<crate::server::engine::ServerEngine>,
        session_id: &str,
    ) {
        let key = format!("wake-{}", fastrand::u64(..));
        if !self.begin_prompt(&key, session_id) {
            return;
        }
        while let Some((text, origin)) = engine.take_task_notification_prompt(session_id).await {
            if let Ok(report) = self
                .run_model_turn(engine, session_id, &text, Vec::new(), Some(origin))
                .await
            {
                self.emit_turn_visibility(engine, session_id, &report).await;
            }
        }
        self.end_prompt(&key);
        self.emit_session_info(session_id, false);
    }

    /// Idle background-task wake for the stdio ACP server (v2 `loop.notify`): a
    /// completion that lands while no prompt is running starts a notification
    /// turn. Per-request bridge servers do not start it — their connection's
    /// prompts drain the queue after each turn.
    async fn task_wake_loop(&self) {
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let Some(runner) = engine.task_runner().await else {
            return;
        };
        let mut bell = runner.notification_bell();
        loop {
            if bell.changed().await.is_err() {
                return;
            }
            for session_id in runner.sessions_with_pending_notifications() {
                self.run_task_notifications(&engine, &session_id).await;
            }
        }
    }

    /// Execute one advertised builtin locally. Never starts a model turn.
    async fn run_builtin_command(&self, session_id: &str, name: &str, args: &str) -> String {
        match name {
            "help" => builtin_commands::help_text(),
            "status" => {
                let model = self.session_model(session_id);
                let profile = self
                    .store
                    .get_state("agent_config", session_id)
                    .ok()
                    .flatten()
                    .unwrap_or(serde_json::Value::Null);
                let thinking = profile
                    .get("thinking")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
                    .map(|value| !matches!(value, "off" | "none"))
                    .unwrap_or(false);
                let cwd = self.session_cwd(session_id);
                let title = self
                    .store
                    .get_session(session_id)
                    .ok()
                    .flatten()
                    .and_then(|summary| summary.title)
                    .filter(|title| !title.is_empty());
                let mut lines = vec![format!("Session: {session_id}")];
                if let Some(title) = title {
                    lines.push(format!("Title: {title}"));
                }
                lines.push(format!(
                    "Model: {} (thinking: {})",
                    if model.is_empty() {
                        "(unbound)"
                    } else {
                        &model
                    },
                    if thinking { "on" } else { "off" }
                ));
                lines.push(format!("Mode: {}", self.current_mode(session_id)));
                lines.push(format!(
                    "Working directory: {}",
                    if cwd.is_empty() { "(unknown)" } else { &cwd }
                ));
                lines.join("\n")
            }
            "usage" => {
                let model = self.session_model(session_id);
                let usage = crate::session::usage::status(&self.store, session_id, &model);
                // Measured+estimated when an engine holds the counting state;
                // a bare estimate is the fallback for an engine-less server.
                let (used, size) = match self.engine.as_ref() {
                    Some(engine) => (
                        engine.session_context_tokens(session_id),
                        engine.model_context_window(session_id).await,
                    ),
                    None => (
                        self.store
                            .load_session_history(session_id)
                            .unwrap_or_default()
                            .iter()
                            .map(|message| {
                                crate::compaction::estimate_tokens(&message.content) as u64
                            })
                            .sum(),
                        None,
                    ),
                };
                let mut lines = Vec::new();
                match size {
                    Some(size) if size > 0 => lines.push(format!(
                        "Context: {used} / {size} tokens ({}%)",
                        used.saturating_mul(100) / size as u64
                    )),
                    _ => lines.push(format!("Context: {used} tokens")),
                }
                let total = usage.total;
                if total.input_other == 0
                    && total.output == 0
                    && total.input_cache_read == 0
                    && total.input_cache_creation == 0
                {
                    lines.push("Session total: no LLM calls yet".to_string());
                } else {
                    let input =
                        total.input_other + total.input_cache_read + total.input_cache_creation;
                    lines.push(format!(
                        "Session total: {input} input, {} output",
                        total.output
                    ));
                }
                lines.join("\n")
            }
            "mcp" => {
                let Some(engine) = self.engine.as_ref() else {
                    return "MCP status is unavailable without an engine.".to_string();
                };
                let servers = engine.session_mcp_entries(session_id).await;
                if servers.is_empty() {
                    return "No MCP servers configured for this session.".to_string();
                }
                let mut lines = vec![format!("MCP servers ({}):", servers.len())];
                for server in servers {
                    let mut line = format!(
                        "- {} ({}): {}, {} tools",
                        server.name, server.transport, server.status, server.tool_count
                    );
                    if let Some(error) = server.error.filter(|error| !error.is_empty()) {
                        line.push_str(&format!(" — {error}"));
                    }
                    lines.push(line);
                }
                lines.join("\n")
            }
            "tasks" => {
                let Some(engine) = self.engine.as_ref() else {
                    return "Background tasks are unavailable without an engine.".to_string();
                };
                let tasks: Vec<serde_json::Value> = engine
                    .background_tasks()
                    .await
                    .into_iter()
                    .filter(|task| task.get("status").and_then(|v| v.as_str()) == Some("running"))
                    .collect();
                if tasks.is_empty() {
                    return "No background tasks.".to_string();
                }
                let mut lines = vec![format!("Background tasks ({}):", tasks.len())];
                for task in tasks {
                    lines.push(format!(
                        "- {}: {} ({})",
                        task.get("taskId").and_then(|v| v.as_str()).unwrap_or("?"),
                        task.get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                        task.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                    ));
                }
                lines.join("\n")
            }
            "compact" => {
                let Some(engine) = self.engine.as_ref() else {
                    return "Compaction is unavailable without an engine.".to_string();
                };
                if engine.is_busy(session_id) {
                    return "A turn is in flight; compaction refused.".to_string();
                }
                let instruction = (!args.is_empty()).then_some(args);
                match engine.compact_session(session_id, instruction).await {
                    Ok(report) if report.removed == 0 => "Nothing to compact.".to_string(),
                    Ok(report) => format!(
                        "Context compacted: {} → {} tokens ({} messages removed).",
                        report.tokens_before, report.tokens_after, report.removed
                    ),
                    Err(error) => format!("/compact failed: {error}"),
                }
            }
            _ => format!("Unknown ACP command: /{name}"),
        }
    }

    /// One model turn with the session's event stream forwarded as ACP
    /// `session/update`s, plus the goal continuations it may owe (v2
    /// `launchContinuationTurn`). The returned report is the first turn's; a
    /// continuation turn's events have already streamed to the client.
    async fn run_model_turn(
        &self,
        engine: &Arc<crate::server::engine::ServerEngine>,
        session_id: &str,
        prompt: &str,
        media: Vec<crate::rpc::types::ContentBlock>,
        origin: Option<serde_json::Value>,
    ) -> Result<crate::server::engine::TurnReport, crate::server::engine::EngineError> {
        let history = self
            .store
            .load_session_history(session_id)
            .unwrap_or_default();
        let turn_number = self.store.next_turn_number(session_id).unwrap_or(1);
        let bus = engine.hub().bus_for(session_id);
        let subscription = self.forward_session_events(session_id, &bus);
        let result = engine
            .run_turn_with_media(session_id, turn_number, history, prompt, media, origin)
            .await;
        // v2 `launchContinuationTurn` on this entry point too: a goal still
        // active after a cleanly completed turn (or a turn that hit its step
        // budget) owns the next one. The response reports the user's turn;
        // continuation turns stream as `session/update`s like any other.
        let mut owed = result.as_ref().ok().cloned();
        while let Some(report) = owed {
            if !crate::server::prompt_queue::turn_owes_goal_continuation(&report) {
                break;
            }
            let step_capped = crate::server::prompt_queue::turn_step_capped(&report);
            let Some(text) = engine
                .pending_goal_continuation(session_id, step_capped)
                .await
            else {
                break;
            };
            let history = self
                .store
                .load_session_history(session_id)
                .unwrap_or_default();
            let turn_number = self.store.next_turn_number(session_id).unwrap_or(1);
            owed = engine
                .run_turn_with_media(
                    session_id,
                    turn_number,
                    history,
                    &text,
                    Vec::new(),
                    Some(json!({ "kind": "system_trigger", "name": "goal_continuation" })),
                )
                .await
                .ok();
        }
        bus.unsubscribe(subscription);
        result
    }

    /// Post-turn visibility (v2's `usage_update` + `plan` builders): the
    /// context the session now carries against the model window, and the todo
    /// list the agent left behind.
    async fn emit_turn_visibility(
        &self,
        engine: &Arc<crate::server::engine::ServerEngine>,
        session_id: &str,
        report: &crate::server::engine::TurnReport,
    ) {
        if let Some(size) = report.context_window {
            let used = engine.session_context_tokens(session_id);
            self.channel.notify(
                "session/update",
                events_map::usage_update_notification(session_id, used, size as u64),
            );
        }
        if let Some(plan) =
            events_map::plan_notification(session_id, &engine.session_todos(session_id))
        {
            self.channel.notify("session/update", plan);
        }
    }

    /// The `session/prompt` response for a finished turn (or its failure), with
    /// the turn's post-visibility published first.
    async fn prompt_outcome(
        &self,
        engine: &Arc<crate::server::engine::ServerEngine>,
        session_id: &str,
        req_id: Option<serde_json::Value>,
        result: Result<crate::server::engine::TurnReport, crate::server::engine::EngineError>,
    ) -> JsonRpcResponse {
        match result {
            Ok(report) => {
                self.emit_turn_visibility(engine, session_id, &report).await;
                self.emit_session_info(session_id, false);
                JsonRpcResponse::success(
                    req_id,
                    json!({
                        "sessionId": session_id,
                        "turnId": report.turn_id,
                        "stopReason": events_map::turn_stop_reason_to_acp(&report.stop_reason),
                        "content": report.reply,
                        "steps": report.steps,
                        "usage": {
                            "inputTokens": report.usage.input_tokens,
                            "outputTokens": report.usage.output_tokens,
                            "totalTokens": report.usage.total_tokens,
                        }
                    }),
                )
            }
            Err(err) => {
                JsonRpcResponse::error(req_id, -32000, format!("Turn execution failed: {err}"))
            }
        }
    }

    /// Snapshot the ACP `SessionModeState` for a session.
    fn mode_state(&self, session_id: &str) -> serde_json::Value {
        json!({
            "currentModeId": self.current_mode(session_id),
            "availableModes": acp_modes(),
        })
    }

    /// Build the ACP `SessionConfigOption[]` surface advertised on
    /// `session/new` / `session/load` / `session/resume` / `session/fork`
    /// (v2 `buildSessionConfigOptions`, config-options.ts): the session's
    /// model row, the `thinking` select when the bound model declares a
    /// thinking axis, then the canonical four modes. Order is part of the
    /// contract — clients render options top-to-bottom.
    async fn config_options(&self, session_id: &str) -> serde_json::Value {
        let model_name = self.session_model(session_id);
        let model_option = json!({
            "type": "select",
            "id": "model",
            "name": "Model",
            "category": "model",
            "currentValue": model_name,
            "options": if model_name.is_empty() {
                json!([])
            } else {
                json!([{ "value": model_name, "name": model_name }])
            },
        });
        let thinking = match self.engine.as_ref() {
            Some(engine) => engine.model_thinking(&model_name).await,
            None => None,
        };
        let mode_option = json!({
            "type": "select",
            "id": "mode",
            "name": "Mode",
            "category": "mode",
            "currentValue": self.current_mode(session_id),
            "options": acp_modes()
                .as_array()
                .map(|modes| {
                    modes
                        .iter()
                        .map(|mode| {
                            json!({
                                "value": mode["id"],
                                "name": mode["name"],
                                "description": mode["description"],
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        });
        let mut options = vec![model_option];
        if let Some(row) = thinking {
            options.push(thinking_option(
                &current_thinking_level(self.store.as_ref(), session_id, &row),
                &row,
            ));
        }
        options.push(mode_option);
        json!(options)
    }

    /// Auth gate (v2 `ensureAuthed`, server.ts:625-646): throws `auth_required`
    /// (`-32000`) unless authed or `disable_auth`. The Rust engine has no
    /// runtime auth state, so "authed" is exactly "an engine (model) is
    /// attached" — the no-engine path cannot run turns and is refused up front.
    fn ensure_authed(&self) -> Result<(), (i64, String)> {
        if self.disable_auth || self.engine.is_some() {
            Ok(())
        } else {
            Err((-32000, "Authentication required".to_string()))
        }
    }

    /// Install the outbound channel used for notifications and back-channel
    /// requests.
    pub fn set_notification_sink(&self, sender: tokio::sync::mpsc::UnboundedSender<AcpOutbound>) {
        self.channel.set_sink(sender);
    }

    /// The outbound channel (tests drive it directly).
    pub fn channel(&self) -> AcpChannel {
        self.channel.clone()
    }

    /// Capabilities the client declared on `initialize`.
    pub fn client_capabilities(&self) -> AcpClientCapabilities {
        self.client_capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Connect the `mcpServers` a client passed on `session/new` as ephemeral
    /// per-session servers (v2 `sessions.create({ mcpServers })`): they are
    /// connected for this session only and never persisted, so they get a
    /// manager of their own rather than the process-wide one every session
    /// shares. Without an attached engine there is nowhere to register them,
    /// so this is a no-op.
    async fn register_session_mcp_servers(&self, session_id: &str, servers: &[serde_json::Value]) {
        let Some(engine) = self.engine.as_ref() else {
            return;
        };
        let manager = std::sync::Arc::new(crate::mcp::manager::McpManager::new());
        let mut registered = false;
        for server in servers {
            let Some(name) = server.get("name").and_then(|value| value.as_str()) else {
                continue;
            };
            let Some(recipe) = acp_mcp_recipe(server) else {
                continue;
            };
            // A server that fails to connect is still registered — it shows
            // as failed on this session's roster, exactly as v2's session
            // overlay does. Only the registration decides the scoping.
            registered = true;
            let _ = manager
                .configure(name, recipe, McpServerOptions::default())
                .await;
        }
        if registered {
            engine.set_session_mcp_manager(session_id, manager);
        }
    }

    /// Forward one session's engine events to ACP `session/update`
    /// notifications until the returned subscription is dropped
    /// (v2 `AcpSession` subscribes the agent event stream the same way).
    pub fn forward_session_events(&self, session_id: &str, bus: &Arc<EventBus>) -> Subscription {
        let channel = self.channel.clone();
        let session = session_id.to_string();
        bus.subscribe(move |event| {
            let Some(params) = events_map::engine_event_to_session_update(&session, event) else {
                return;
            };
            channel.notify("session/update", params);
        })
    }

    /// Dispatch one raw line. A response to a server-initiated request
    /// (`{"id":N,"result"…}`) resolves the waiting back-channel call instead
    /// of being treated as a client request.
    pub async fn handle_line(&self, raw: &str) -> Option<JsonRpcResponse> {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
            && let Some(id) = AcpChannel::response_id(&value)
        {
            self.channel.resolve(id, value).await;
            return None;
        }
        self.handle_message(raw).await
    }

    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let store = Arc::new(SqliteSessionStore::in_memory()?);
        let mut server = Self::new(store);
        server.disable_auth = true;
        Ok(server)
    }

    /// Run the ACP server reading from stdin and writing to stdout. Responses
    /// and server-initiated notifications share one writer so their JSON lines
    /// never interleave.
    pub async fn run_stdio(self: Arc<Self>) -> Result<(), std::io::Error> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        self.set_notification_sink(tx.clone());

        // Idle background-task wake: a task that finishes while no prompt is
        // running starts a notification turn (v2 `loop.notify`). Only the
        // dedicated stdio server owns this loop — bridge servers are attached
        // to a connection whose prompts drain the queue after each turn.
        {
            let wake = self.clone();
            tokio::spawn(async move { wake.task_wake_loop().await });
        }

        let writer = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(message) = rx.recv().await {
                let value = match message {
                    AcpOutbound::Response(response) => serde_json::to_value(response),
                    AcpOutbound::Notification(notification) => serde_json::to_value(notification),
                    AcpOutbound::Request(request) => serde_json::to_value(request),
                };
                let Ok(value) = value else { continue };
                if stdout
                    .write_all(value.to_string().as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
                if stdout.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
            }
        });

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        // Each line is dispatched on its own task: a `session/prompt` runs for
        // as long as the turn does, and a reader that awaited it inline could
        // not deliver the `session/cancel` / `$/cancel_request` that is
        // supposed to stop it. Responses are correlated by JSON-RPC id, so
        // out-of-order completion is fine.
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        while let Some(line) = reader.next_line().await? {
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            let server = self.clone();
            let tx = tx.clone();
            tasks.push(tokio::spawn(async move {
                if let Some(resp) = server.handle_line(&trimmed).await {
                    let _ = tx.send(AcpOutbound::Response(resp));
                }
            }));
            tasks.retain(|task| !task.is_finished());
        }
        // Drop the sink's clone too, otherwise the writer task never ends.
        self.channel.clear_sink();
        drop(tx);
        for task in tasks {
            let _ = task.await;
        }
        let _ = writer.await;
        Ok(())
    }

    /// ACP `SessionInfo.cwd`: the session's workspace root, else its recorded
    /// cwd, else the empty string (v2 `sessionSummaryToSessionInfo`).
    fn session_cwd(&self, session_id: &str) -> String {
        session_cwd_of(&self.store, session_id)
    }

    /// Reserve a session for an in-flight `session/prompt`, keyed by the
    /// request id. `false` when another prompt is already running for it
    /// (v2 `assertNoActiveTurn`, session.ts:632-639): the engine keeps one
    /// cancel token per session, so a second concurrent turn would overwrite
    /// the first one's and neither would settle correctly.
    fn begin_prompt(&self, request_key: &str, session_id: &str) -> bool {
        let mut in_flight = self
            .in_flight_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if in_flight.values().any(|active| active == session_id) {
            return false;
        }
        in_flight.insert(request_key.to_string(), session_id.to_string());
        true
    }

    fn end_prompt(&self, request_key: &str) {
        self.in_flight_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(request_key);
    }

    /// Cancel the turn behind a `$/cancel_request` notification: the
    /// notification names the *request* to abort, so the in-flight prompt's
    /// session is looked up and its turn cancelled. The prompt itself then
    /// settles with `stopReason: "cancelled"`, which is the response the ACP
    /// cancellation contract asks for.
    fn cancel_requested(&self, request_id: &serde_json::Value) -> bool {
        let key = request_key(request_id);
        let session_id = self
            .in_flight_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        match (session_id, self.engine.as_ref()) {
            (Some(session_id), Some(engine)) => engine.cancel_turn(&session_id),
            _ => false,
        }
    }

    /// Apply an ACP session mode (v2 `setSessionMode` + `setMode` +
    /// `acpModeToToggles`): validate the session and mode id, persist the
    /// permission mode the engine reads at turn start, and notify the client
    /// with `current_mode_update`.
    ///
    /// Both halves of v2 `acpModeToToggles` are applied: the plan toggle
    /// through the workspace `plan` domain (`tools/plan_mode.rs:107-127`)
    /// and the permission mode through session metadata.
    fn apply_session_mode(
        &self,
        session_id: Option<&str>,
        mode_id: Option<&str>,
    ) -> Result<String, (i64, String)> {
        let Some(session_id) = session_id else {
            return Err((-32602, "Invalid params: sessionId is required".to_string()));
        };
        let Some(mode_id) = mode_id else {
            return Err((-32602, "Invalid params: modeId is required".to_string()));
        };
        if self.store.get_session(session_id).ok().flatten().is_none() {
            return Err((-32602, format!("Unknown sessionId: {session_id}")));
        }
        if !is_acp_mode_id(mode_id) {
            return Err((-32602, format!("Unknown modeId: {mode_id}")));
        }

        let mut metadata = self
            .store
            .get_state("metadata", session_id)
            .ok()
            .flatten()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| json!({}));
        if let Some(object) = metadata.as_object_mut() {
            object.insert(
                "permission_mode".into(),
                json!(acp_mode_permission(mode_id)),
            );
        }
        let _ = self
            .store
            .put_session_state("metadata", session_id, session_id, &metadata);
        apply_plan_toggle(&self.store, session_id, acp_mode_plan(mode_id));
        self.modes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), mode_id.to_string());

        self.channel.notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "current_mode_update",
                    "currentModeId": mode_id,
                },
            }),
        );
        Ok(mode_id.to_string())
    }

    /// Apply an ACP per-session model switch (v2 `AcpSession.setModel` +
    /// `setSessionModel`): validate the session and model id, persist the
    /// override the engine reads at turn start, and push
    /// `config_option_update` so the client's picker follows.
    fn apply_session_model(
        &self,
        session_id: Option<&str>,
        model_id: Option<&str>,
    ) -> Result<(), (i64, String)> {
        let Some(session_id) = session_id else {
            return Err((-32602, "Invalid params: sessionId is required".to_string()));
        };
        let Some(model_id) = model_id.filter(|model| !model.is_empty()) else {
            return Err((-32602, "Invalid params: modelId is required".to_string()));
        };
        if self.store.get_session(session_id).ok().flatten().is_none() {
            return Err((-32602, format!("Unknown sessionId: {session_id}")));
        }

        let mut profile = self
            .store
            .get_state("agent_config", session_id)
            .ok()
            .flatten()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| json!({}));
        if let Some(object) = profile.as_object_mut() {
            object.insert("model".into(), json!(model_id));
        }
        let _ = self
            .store
            .put_session_state("agent_config", session_id, session_id, &profile);
        Ok(())
    }

    /// Push the refreshed `configOptions` after a `model` / `thinking` switch
    /// (v2 `config_option_update`).
    async fn notify_config_options(&self, session_id: &str) {
        let options = self.config_options(session_id).await;
        self.channel.notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": options,
                },
            }),
        );
    }

    /// Apply an ACP thinking-level switch (v2 `buildSessionConfigOptions`
    /// `thinking` arm + `setSessionConfigOption`): validate the level against
    /// the model's declared axis, persist the override the engine reads at
    /// turn start, and push the refreshed option set. An always-thinking model
    /// clamps `off` to `on` — v2 renders those locked-on.
    async fn apply_session_thinking(
        &self,
        session_id: Option<&str>,
        value: Option<&str>,
    ) -> Result<(), (i64, String)> {
        let Some(session_id) = session_id else {
            return Err((-32602, "Invalid params: sessionId is required".to_string()));
        };
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Err((-32602, "Invalid params: value is required".to_string()));
        };
        if self.store.get_session(session_id).ok().flatten().is_none() {
            return Err((-32602, format!("Unknown sessionId: {session_id}")));
        }
        let Some(engine) = self.engine.as_ref() else {
            return Err((-32602, "Unsupported configId: thinking".to_string()));
        };
        let model = self.session_model(session_id);
        let Some(row) = engine.model_thinking(&model).await else {
            return Err((-32602, format!("Model {model} does not support thinking")));
        };
        let level = if row.always_thinking && value == "off" {
            "on"
        } else {
            value
        };
        let allowed = level == "on"
            || level == "off"
            || row.support_efforts.iter().any(|effort| effort == level);
        if !allowed {
            return Err((-32602, format!("Invalid thinking level: {level}")));
        }
        let mut profile = self
            .store
            .get_state("agent_config", session_id)
            .ok()
            .flatten()
            .filter(|value| value.is_object())
            .unwrap_or_else(|| json!({}));
        if let Some(object) = profile.as_object_mut() {
            object.insert("thinking".into(), json!(level));
        }
        let _ = self
            .store
            .put_session_state("agent_config", session_id, session_id, &profile);
        self.notify_config_options(session_id).await;
        Ok(())
    }

    /// Process an incoming JSON-RPC 2.0 message and return an optional response.
    pub async fn handle_message(&self, raw: &str) -> Option<JsonRpcResponse> {
        let req: JsonRpcRequest = match serde_json::from_str(raw) {
            Ok(r) => r,
            Err(_) => {
                return Some(JsonRpcResponse::error(
                    None,
                    -32700,
                    "Parse error: invalid JSON",
                ));
            }
        };

        if req.jsonrpc != "2.0" {
            return Some(JsonRpcResponse::error(
                req.id,
                -32600,
                "Invalid Request: jsonrpc must be '2.0'",
            ));
        }

        // A message without an id is a JSON-RPC notification: run its side
        // effect, never write a response (v2 dispatches `session/cancel`
        // through `onNotification`, server.ts:717).
        let is_notification = req.id.is_none();

        let resp = match req.method.as_str() {
            "initialize" => {
                let params: AcpInitializeParams = req
                    .params
                    .as_ref()
                    .and_then(|raw| serde_json::from_value(raw.clone()).ok())
                    .unwrap_or_default();
                *self
                    .client_capabilities
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    params.client_capabilities.clone().unwrap_or_default();
                let protocol_version = negotiate_protocol_version(params.protocol_version);
                JsonRpcResponse::success(
                    req.id,
                    json!({
                        "protocolVersion": protocol_version,
                        "agentCapabilities": {
                            "loadSession": true,
                            // The engine accepts image blocks (native media
                            // injection); audio prompt blocks stay unsupported.
                            "promptCapabilities": {
                                "image": true,
                                "audio": false,
                                "embeddedContext": true,
                            },
                            // Only the methods this server actually answers.
                            "sessionCapabilities": {
                                "list": {},
                                "resume": {},
                                "close": {},
                                "delete": {},
                                "fork": {},
                                // Honored on `session/new` only — the engine
                                // merges additional roots at session create
                                // (v2 `warnIgnoredAdditionalDirs`).
                                "additionalDirectories": {},
                            },
                            "mcpCapabilities": { "http": true, "sse": true },
                            "auth": { "logout": {} },
                        },
                        "authMethods": [{
                            "id": "login",
                            "type": "terminal",
                            "name": "Login with Kimi account",
                            "description": "Open the device-code login flow in a terminal.",
                            "args": ["--login"],
                            "env": {},
                        }],
                        "agentInfo": {
                            "name": "Kimi Code CLI",
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                )
            }
            "session/new" => {
                if let Err((code, message)) = self.ensure_authed() {
                    return Some(JsonRpcResponse::error(req.id, code, message));
                }
                let session_id = format!("sess-{}", fastrand::u64(..));
                let title = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("title"))
                    .and_then(|v| v.as_str());
                let cwd = req
                    .params
                    .as_ref()
                    .and_then(|params| params.get("cwd"))
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let mcp_servers = req
                    .params
                    .as_ref()
                    .and_then(|params| params.get("mcpServers"))
                    .and_then(|value| value.as_array())
                    .cloned()
                    .unwrap_or_default();
                // Extra workspace roots the editor hands over. The engine
                // merges them into the session's authorized boundary at turn
                // start (v2 `sessions.create({ additionalDirs })`).
                let additional_dirs: Vec<String> = req
                    .params
                    .as_ref()
                    .and_then(|params| params.get("additionalDirectories"))
                    .and_then(|value| value.as_array())
                    .map(|dirs| {
                        dirs.iter()
                            .filter_map(|dir| dir.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();

                let created = match cwd.as_deref() {
                    Some(root) => match self.store.create_workspace(root, None) {
                        Ok(workspace) => self.store.create_session_with_workspace(
                            &session_id,
                            title,
                            Some(&workspace.id),
                        ),
                        Err(error) => Err(error),
                    },
                    None => self.store.create_session(&session_id, title),
                };

                match created {
                    Ok(_) => {
                        let mut metadata = json!({});
                        if let Some(root) = cwd.as_deref() {
                            metadata["cwd"] = json!(root);
                        }
                        if !additional_dirs.is_empty() {
                            metadata["additional_dirs"] = json!(additional_dirs);
                        }
                        if metadata.as_object().is_some_and(|map| !map.is_empty()) {
                            let _ = self.store.put_session_state(
                                "metadata",
                                &session_id,
                                &session_id,
                                &metadata,
                            );
                        }
                        if !mcp_servers.is_empty() {
                            self.register_session_mcp_servers(&session_id, &mcp_servers)
                                .await;
                        }
                        self.modes
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(session_id.clone(), "default".to_string());
                        self.emit_available_commands(&session_id).await;
                        self.emit_session_info(&session_id, true);
                        let config_options = self.config_options(&session_id).await;
                        JsonRpcResponse::success(
                            req.id,
                            json!({
                                "sessionId": session_id,
                                "configOptions": config_options,
                                "modes": self.mode_state(&session_id),
                            }),
                        )
                    }
                    Err(e) => {
                        JsonRpcResponse::error(req.id, -32000, format!("Database error: {e}"))
                    }
                }
            }
            "session/list" => match self.store.list_sessions() {
                Ok(list) => {
                    let requested_cwd = req
                        .params
                        .as_ref()
                        .and_then(|params| params.get("cwd"))
                        .and_then(|value| value.as_str());
                    let sessions: Vec<serde_json::Value> = list
                        .iter()
                        .filter_map(|summary| {
                            let cwd = self.session_cwd(&summary.session_id);
                            // v2 `filterSessionSummariesByCwd`: a session
                            // without a recorded cwd is always kept.
                            if let Some(requested) = requested_cwd
                                && !cwd.is_empty()
                                && cwd != requested
                            {
                                return None;
                            }
                            let title = summary.title.as_deref().filter(|title| !title.is_empty());
                            let updated_at = chrono::DateTime::from_timestamp_millis(
                                summary.updated_at,
                            )
                            .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
                            Some(json!({
                                "sessionId": summary.session_id,
                                "cwd": cwd,
                                "title": title,
                                "updatedAt": updated_at,
                            }))
                        })
                        .collect();
                    JsonRpcResponse::success(req.id, json!({ "sessions": sessions }))
                }
                Err(e) => JsonRpcResponse::error(req.id, -32000, format!("Database error: {e}")),
            },
            "session/prompt" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());
                let prompt = params
                    .and_then(|p| p.get("prompt"))
                    .and_then(acp_prompt_to_parts);

                match (session_id, prompt) {
                    (Some(sid), Some((p, media))) => {
                        if let Some(ref engine) = self.engine {
                            let request_key = req.id.as_ref().map(request_key).unwrap_or_default();
                            if !self.begin_prompt(&request_key, sid) {
                                JsonRpcResponse::error(
                                    req.id,
                                    -32600,
                                    "another turn is already in progress",
                                )
                            } else if let Some((name, args)) = builtin_commands::parse_slash(&p) {
                                // Slash input the host answers itself: an ACP
                                // builtin, an engine skill activation, or an
                                // unknown command (v2 `detectSlashIntent`).
                                // Unknown slash text is answered locally —
                                // sending it to the model would let it invent a
                                // command that does not exist.
                                if let Some((command, args)) = builtin_commands::parse(&p) {
                                    // ACP-owned commands answer locally and
                                    // never reach the model (v2
                                    // `runBuiltinSlashCommand`).
                                    let text =
                                        self.run_builtin_command(sid, command.name, &args).await;
                                    self.end_prompt(&request_key);
                                    self.channel
                                        .notify("session/update", local_command_update(sid, &text));
                                    // `/compact` and external renames can
                                    // change the title; keep the client's
                                    // header current.
                                    self.emit_session_info(sid, false);
                                    JsonRpcResponse::success(
                                        req.id,
                                        local_command_response(sid, &text),
                                    )
                                } else if let Some(skill_name) =
                                    self.resolve_skill_command(sid, name).await
                                {
                                    // v2 `driveSkillActivation`: the engine
                                    // renders the activation prompt and drives
                                    // it as a normal turn, so its events stream
                                    // like any other prompt.
                                    match engine
                                        .render_session_skill_prompt(sid, &skill_name, &args)
                                        .await
                                    {
                                        Ok(rendered) => {
                                            let mut origin = json!({
                                                "kind": "skill_activation",
                                                "activationId": format!(
                                                    "skill-{}",
                                                    fastrand::u64(..)
                                                ),
                                                "skillName": rendered.name,
                                                "trigger": "user-slash",
                                            });
                                            if let Some(path) = rendered.path.as_deref() {
                                                origin["skillPath"] = json!(path);
                                            }
                                            if let Some(source) = rendered.source.as_deref() {
                                                origin["skillSource"] = json!(source);
                                            }
                                            if let Some(kind) = rendered.skill_type.as_deref() {
                                                origin["skillType"] = json!(kind);
                                            }
                                            let result = self
                                                .run_model_turn(
                                                    engine,
                                                    sid,
                                                    &rendered.text,
                                                    Vec::new(),
                                                    Some(origin),
                                                )
                                                .await;
                                            self.end_prompt(&request_key);
                                            let response = self
                                                .prompt_outcome(engine, sid, req.id.clone(), result)
                                                .await;
                                            // Completions that landed while the
                                            // turn ran become their own turns
                                            // (v2 `loop.notify`).
                                            self.run_task_notifications(engine, sid).await;
                                            response
                                        }
                                        Err(error) => {
                                            self.end_prompt(&request_key);
                                            let text =
                                                builtin_commands::skill_error_text(name, &error);
                                            self.channel.notify(
                                                "session/update",
                                                local_command_update(sid, &text),
                                            );
                                            JsonRpcResponse::success(
                                                req.id,
                                                local_command_response(sid, &text),
                                            )
                                        }
                                    }
                                } else {
                                    self.end_prompt(&request_key);
                                    let text = format!(
                                        "Unknown ACP command: /{name}. Use /help to see available commands."
                                    );
                                    self.channel
                                        .notify("session/update", local_command_update(sid, &text));
                                    JsonRpcResponse::success(
                                        req.id,
                                        local_command_response(sid, &text),
                                    )
                                }
                            } else {
                                let result =
                                    self.run_model_turn(engine, sid, &p, media, None).await;
                                self.end_prompt(&request_key);
                                let response = self
                                    .prompt_outcome(engine, sid, req.id.clone(), result)
                                    .await;
                                // Completions that landed while the turn ran
                                // become their own turns (v2 `loop.notify`).
                                self.run_task_notifications(engine, sid).await;
                                response
                            }
                        } else {
                            // No engine is wired (e.g. no model could be
                            // resolved). The previous behaviour invented an
                            // assistant reply and *persisted* it, so the client
                            // showed a plausible answer that no model had
                            // produced and the fake exchange polluted the
                            // session's history for every later resume/export.
                            // Refuse instead of fabricating.
                            JsonRpcResponse::error(
                                req.id,
                                -32000,
                                "No LLM engine is available for this session, so no prompt can \
                                 be answered. Configure a model (or start the server with one \
                                 resolved) and retry.",
                            )
                        }
                    }
                    _ => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: sessionId and prompt are required",
                    ),
                }
            }
            "session/load" => {
                if let Err((code, message)) = self.ensure_authed() {
                    return Some(JsonRpcResponse::error(req.id, code, message));
                }
                let params = req.params.as_ref();
                warn_ignored_additional_dirs("session/load", params);
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());

                match session_id {
                    Some(sid) => match self.store.load_session_messages(sid) {
                        Ok(history) => {
                            // Replay the persisted history as an ordered batch
                            // of `session/update` chunks, then answer with the
                            // mode state (v2 `loadSession` + `replay.ts`).
                            //
                            // v2 `replay.ts`: an assistant message's tool calls
                            // replay as `tool_call` creates and the `tool`-role
                            // results as `tool_call_update` terminals. Both
                            // sides namespace the wire id with the turn the row
                            // was persisted under (`acp_tool_call_id`), so a
                            // replayed update lands on the card its create
                            // opened. A result whose create is not in this
                            // slice (a compacted session) is skipped rather
                            // than orphaning a card the client never saw.
                            let mut created_calls: std::collections::HashSet<String> =
                                std::collections::HashSet::new();
                            for stored in &history {
                                let message = &stored.message;
                                if message.role == "tool" {
                                    let Some(raw_id) = message.tool_call_id.as_deref() else {
                                        continue;
                                    };
                                    let tool_call_id =
                                        events_map::acp_tool_call_id(&stored.turn_id, raw_id);
                                    if !created_calls.contains(&tool_call_id) {
                                        continue;
                                    }
                                    self.channel.notify(
                                        "session/update",
                                        json!({
                                            "sessionId": sid,
                                            "update": {
                                                "sessionUpdate": "tool_call_update",
                                                "toolCallId": tool_call_id,
                                                "status": "completed",
                                                "content": [{
                                                    "type": "content",
                                                    "content": {
                                                        "type": "text",
                                                        "text": message.content,
                                                    },
                                                }],
                                                "rawOutput": message.content,
                                            },
                                        }),
                                    );
                                    continue;
                                }
                                // v2 `replay.ts` order within an assistant
                                // message: the text chunk first, then one
                                // `tool_call` create per requested call.
                                let update = match message.role.as_str() {
                                    "user" => "user_message_chunk",
                                    "assistant" => "agent_message_chunk",
                                    _ => "",
                                };
                                if !update.is_empty() && !message.content.is_empty() {
                                    self.channel.notify(
                                        "session/update",
                                        json!({
                                            "sessionId": sid,
                                            "update": {
                                                "sessionUpdate": update,
                                                "content": { "type": "text", "text": message.content },
                                            },
                                        }),
                                    );
                                }
                                for call in &message.tool_calls {
                                    let tool_call_id =
                                        events_map::acp_tool_call_id(&stored.turn_id, &call.id);
                                    created_calls.insert(tool_call_id.clone());
                                    self.channel.notify(
                                        "session/update",
                                        json!({
                                            "sessionId": sid,
                                            "update": {
                                                "sessionUpdate": "tool_call",
                                                "toolCallId": tool_call_id,
                                                "title": call.name,
                                                "kind": events_map::infer_tool_kind(&call.name),
                                                "status": "in_progress",
                                                "rawInput": call.arguments,
                                                "content": [{
                                                    "type": "content",
                                                    "content": {
                                                        "type": "text",
                                                        "text": serde_json::to_string(&call.arguments)
                                                            .unwrap_or_default(),
                                                    },
                                                }],
                                            },
                                        }),
                                    );
                                }
                            }
                            if let Some(engine) = &self.engine {
                                // A reattached client also gets the session's
                                // carried state (v2's `usage_update` + `plan`
                                // builders): the todo list and the context
                                // gauge the replayed transcript cannot show.
                                if let Some(plan) =
                                    events_map::plan_notification(sid, &engine.session_todos(sid))
                                {
                                    self.channel.notify("session/update", plan);
                                }
                                if let Some(size) = engine.model_context_window(sid).await {
                                    let used = engine.session_context_tokens(sid);
                                    self.channel.notify(
                                        "session/update",
                                        events_map::usage_update_notification(
                                            sid,
                                            used,
                                            size as u64,
                                        ),
                                    );
                                }
                            }
                            self.emit_available_commands(sid).await;
                            self.emit_session_info(sid, true);
                            let config_options = self.config_options(sid).await;
                            JsonRpcResponse::success(
                                req.id,
                                json!({
                                    "configOptions": config_options,
                                    "modes": self.mode_state(sid),
                                }),
                            )
                        }
                        Err(e) => {
                            JsonRpcResponse::error(req.id, -32000, format!("Database error: {e}"))
                        }
                    },
                    None => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: sessionId is required",
                    ),
                }
            }
            // `session/resume` re-attaches without replaying history — that is
            // the whole difference from `session/load` (v2 `resumeSession`,
            // server.ts:312-321).
            "session/resume" => {
                if let Err((code, message)) = self.ensure_authed() {
                    return Some(JsonRpcResponse::error(req.id, code, message));
                }
                let params = req.params.as_ref();
                warn_ignored_additional_dirs("session/resume", params);
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());
                match session_id {
                    Some(sid) => {
                        if self.store.get_session(sid).ok().flatten().is_none() {
                            JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Unknown sessionId: {sid}"),
                            )
                        } else {
                            self.emit_available_commands(sid).await;
                            self.emit_session_info(sid, true);
                            let config_options = self.config_options(sid).await;
                            JsonRpcResponse::success(
                                req.id,
                                json!({
                                    "configOptions": config_options,
                                    "modes": self.mode_state(sid),
                                }),
                            )
                        }
                    }
                    None => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: sessionId is required",
                    ),
                }
            }
            // `session/fork` (UNSTABLE in the ACP schema) copies the source
            // session's history into a new session (v2 `forkSession`,
            // server.ts:266-294).
            "session/fork" => {
                if let Err((code, message)) = self.ensure_authed() {
                    return Some(JsonRpcResponse::error(req.id, code, message));
                }
                let params = req.params.as_ref();
                warn_ignored_additional_dirs("session/fork", params);
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());
                match session_id {
                    Some(source) => match self.store.get_session(source).ok().flatten() {
                        None => JsonRpcResponse::error(
                            req.id,
                            -32602,
                            format!("Unknown sessionId: {source}"),
                        ),
                        Some(_) => {
                            let forked = format!("sess-{}", fastrand::u64(..));
                            // `None` is not a missing feature here: v2's ACP
                            // `session/fork` calls `klient.session(id).fork()`
                            // with no options at all (acp-server/src/server.ts:276),
                            // and `ForkSessionRequest` carries no turn index.
                            // Only the REST surface exposes the cut point.
                            match self.store.fork_session(source, &forked, None, None) {
                                Ok(true) => {
                                    self.modes
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .insert(forked.clone(), "default".to_string());
                                    let config_options = self.config_options(&forked).await;
                                    JsonRpcResponse::success(
                                        req.id,
                                        json!({
                                            "sessionId": forked,
                                            "configOptions": config_options,
                                            "modes": self.mode_state(&forked),
                                        }),
                                    )
                                }
                                Ok(false) => JsonRpcResponse::error(
                                    req.id,
                                    -32602,
                                    format!("Unknown sessionId: {source}"),
                                ),
                                Err(crate::session::sqlite_store::ForkError::Store(e)) => {
                                    JsonRpcResponse::error(
                                        req.id,
                                        -32000,
                                        format!("Database error: {e}"),
                                    )
                                }
                                Err(e) => JsonRpcResponse::error(req.id, -32602, e.to_string()),
                            }
                        }
                    },
                    None => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: sessionId is required",
                    ),
                }
            }
            "session/delete" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());

                match session_id {
                    Some(sid) => {
                        // v2 rejects an unknown id with invalid params instead
                        // of answering `{deleted:false}` (server.ts:350-377).
                        if self.store.get_session(sid).ok().flatten().is_none() {
                            JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Unknown sessionId: {sid}"),
                            )
                        } else {
                            match self.store.delete_session(sid) {
                                Ok(_) => JsonRpcResponse::success(req.id, json!({})),
                                Err(e) => JsonRpcResponse::error(
                                    req.id,
                                    -32000,
                                    format!("Database error: {e}"),
                                ),
                            }
                        }
                    }
                    None => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: sessionId is required",
                    ),
                }
            }
            "session/close" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());

                match session_id {
                    Some(sid) => {
                        if self.store.get_session(sid).ok().flatten().is_none() {
                            JsonRpcResponse::error(
                                req.id,
                                -32602,
                                format!("Unknown sessionId: {sid}"),
                            )
                        } else {
                            // Best-effort teardown: cancel any in-flight turn and
                            // drop the session's local mode (v2 `closeSession`,
                            // server.ts:332-348).
                            if let Some(ref engine) = self.engine {
                                engine.cancel_turn(sid);
                            }
                            self.modes
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(sid);
                            JsonRpcResponse::success(req.id, json!({}))
                        }
                    }
                    None => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: sessionId is required",
                    ),
                }
            }
            "session/cancel" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId").or_else(|| p.get("session_id")))
                    .and_then(|v| v.as_str());
                if let (Some(sid), Some(engine)) = (session_id, self.engine.as_ref()) {
                    engine.cancel_turn(sid);
                }
                JsonRpcResponse::success(req.id, json!({ "cancelled": true }))
            }
            // Protocol-level cancellation: the client names the *request* it
            // wants aborted, not the session. The prompt it names settles with
            // `stopReason: "cancelled"`, which is the response the ACP
            // cancellation contract requires.
            "$/cancel_request" => {
                let request_id = req
                    .params
                    .as_ref()
                    .and_then(|params| params.get("requestId"));
                let cancelled = request_id.is_some_and(|id| self.cancel_requested(id));
                JsonRpcResponse::success(req.id, json!({ "cancelled": cancelled }))
            }
            "session/set_mode" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str());
                let mode_id = params
                    .and_then(|p| p.get("modeId").or_else(|| p.get("mode")))
                    .and_then(|v| v.as_str());
                match self.apply_session_mode(session_id, mode_id) {
                    Ok(mode) => JsonRpcResponse::success(req.id, json!({ "modeId": mode })),
                    Err((code, message)) => JsonRpcResponse::error(req.id, code, message),
                }
            }
            // The custom `session/set_model` extension (dropped from the ACP
            // SDK's typed method set; v2 registers it by hand, server.ts:663-687).
            "session/set_model" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str());
                let model_id = params
                    .and_then(|p| p.get("modelId"))
                    .and_then(|v| v.as_str());
                match self.apply_session_model(session_id, model_id) {
                    Ok(()) => {
                        if let Some(session_id) = session_id {
                            self.notify_config_options(session_id).await;
                        }
                        JsonRpcResponse::success(req.id, json!({}))
                    }
                    Err((code, message)) => JsonRpcResponse::error(req.id, code, message),
                }
            }
            // The `mode` / `model` / `thinking` arms of
            // `session/set_config_option` funnel into the same paths (v2
            // `setSessionConfigOption`, server.ts:471-513).
            "session/set_config_option" => {
                let params = req.params.as_ref();
                let session_id = params
                    .and_then(|p| p.get("sessionId"))
                    .and_then(|v| v.as_str());
                let config_id = params
                    .and_then(|p| p.get("configId"))
                    .and_then(|v| v.as_str());
                let value = params.and_then(|p| p.get("value")).and_then(|v| v.as_str());
                match config_id {
                    Some("mode") => match self.apply_session_mode(session_id, value) {
                        Ok(_) => {
                            let session_id = session_id.unwrap_or_default();
                            let options = self.config_options(session_id).await;
                            JsonRpcResponse::success(req.id, json!({ "configOptions": options }))
                        }
                        Err((code, message)) => JsonRpcResponse::error(req.id, code, message),
                    },
                    Some("model") => match self.apply_session_model(session_id, value) {
                        Ok(()) => {
                            let session_id = session_id.unwrap_or_default();
                            self.notify_config_options(session_id).await;
                            let options = self.config_options(session_id).await;
                            JsonRpcResponse::success(req.id, json!({ "configOptions": options }))
                        }
                        Err((code, message)) => JsonRpcResponse::error(req.id, code, message),
                    },
                    Some("thinking") => {
                        match self.apply_session_thinking(session_id, value).await {
                            Ok(()) => {
                                let session_id = session_id.unwrap_or_default();
                                let options = self.config_options(session_id).await;
                                JsonRpcResponse::success(
                                    req.id,
                                    json!({ "configOptions": options }),
                                )
                            }
                            Err((code, message)) => JsonRpcResponse::error(req.id, code, message),
                        }
                    }
                    _ => JsonRpcResponse::error(
                        req.id,
                        -32602,
                        format!("Unsupported configId: {}", config_id.unwrap_or_default()),
                    ),
                }
            }
            // `authenticate` re-checks the gate after the client runs the
            // terminal-auth login flow (v2 `authenticate`, server.ts:380-390).
            // The Rust engine has no managed token to drop, so `logout` is a
            // no-op success (v2 `logout`, server.ts:392-399).
            "authenticate" => {
                let method_id = req
                    .params
                    .as_ref()
                    .and_then(|p| p.get("methodId"))
                    .and_then(|v| v.as_str());
                if method_id != Some("login") {
                    JsonRpcResponse::error(
                        req.id,
                        -32602,
                        "Invalid params: methodId must be 'login'",
                    )
                } else {
                    match self.ensure_authed() {
                        Ok(()) => JsonRpcResponse::success(req.id, json!({})),
                        Err((code, message)) => JsonRpcResponse::error(req.id, code, message),
                    }
                }
            }
            "logout" => JsonRpcResponse::success(req.id, json!({})),
            "ping" => JsonRpcResponse::success(req.id, json!("pong")),
            _ => {
                JsonRpcResponse::error(req.id, -32601, format!("Method not found: {}", req.method))
            }
        };

        if is_notification {
            return None;
        }
        Some(resp)
    }
}

/// ACP `SessionInfo.cwd`: the session's workspace root, else its recorded
/// cwd, else the empty string (v2 `sessionSummaryToSessionInfo`).
fn session_cwd_of(store: &SqliteSessionStore, session_id: &str) -> String {
    if let Ok(Some(session)) = store.get_session(session_id)
        && let Some(workspace_id) = session.workspace_id.as_deref()
        && let Ok(Some(workspace)) = store.get_workspace(workspace_id)
    {
        return workspace.root;
    }
    store
        .get_state("metadata", session_id)
        .ok()
        .flatten()
        .and_then(|metadata| {
            metadata
                .get("cwd")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// A JSON-RPC id as a map key. ACP `RequestId` is `string | number`, and the
/// two sides of a `$/cancel_request` round trip must normalize identically.
fn request_key(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The engine merges additional roots only at session create, so
/// load / resume / fork cannot honor `additionalDirectories` — say so instead
/// of dropping the field in silence (v2 `warnIgnoredAdditionalDirs`,
/// server.ts:654-659).
fn warn_ignored_additional_dirs(method: &str, params: Option<&serde_json::Value>) {
    let dirs = params
        .and_then(|params| params.get("additionalDirectories"))
        .and_then(|value| value.as_array())
        .filter(|dirs| !dirs.is_empty());
    if let Some(dirs) = dirs {
        tracing::warn!(
            method,
            dirs = ?dirs,
            "acp: ignoring additionalDirectories (the engine merges roots only on session create)"
        );
    }
}

/// The four wire-level mode ids this host understands (v2 `isAcpModeId`,
/// modes.ts).
fn is_acp_mode_id(mode: &str) -> bool {
    matches!(mode, "default" | "plan" | "auto" | "yolo")
}

/// v2 `acpModeToToggles`: the permission mode each ACP mode maps to.
fn acp_mode_permission(mode: &str) -> &'static str {
    match mode {
        "auto" => "auto",
        "yolo" => "yolo",
        _ => "manual",
    }
}

/// v2 `acpModeToToggles`: the plan toggle each ACP mode maps to. Only
/// `plan` enters plan mode; every other mode leaves it (v2 calls
/// `cancelPlan`, which has the identical state effect).
fn acp_mode_plan(mode: &str) -> bool {
    mode == "plan"
}

/// Apply the plan half of v2 `setMode` through the session state store (v2
/// agent scope — the same `plan` domain `tools/plan_mode.rs` reads and writes),
/// so a mode switch and a native `EnterPlanMode` cannot shadow each other.
/// Best effort: a domain that cannot be reached must not fail the mode
/// switch, because the permission half still has to land.
fn apply_plan_toggle(store: &SqliteSessionStore, session_id: &str, plan: bool) {
    let root = session_cwd_of(store, session_id);
    if root.is_empty() {
        return;
    }
    let root = std::path::Path::new(&root);
    let state = crate::storage::state_store::StateStore::for_session(root, session_id)
        .or_else(|_| crate::storage::state_store::StateStore::for_workspace(root));
    let Ok(state) = state else {
        return;
    };
    // `apply_write` is the domain's own semantics: entering plan mode mints
    // the plan id/path and the empty plan file, exactly like `EnterPlanMode`.
    let value = serde_json::json!({ "active": plan });
    match state.apply_write("plan", &value) {
        Ok(outcome) => {
            if let Err(error) = state.write_domain("plan", &outcome.stored) {
                tracing::warn!(session_id, plan, %error, "acp set_mode could not persist plan state");
            }
        }
        Err(error) => {
            tracing::warn!(session_id, plan, %error, "acp set_mode could not toggle plan state");
        }
    }
}

/// Accept both prompt forms: the legacy plain string and the ACP
/// `ContentBlock[]` array (v2 `acpBlocksToContentParts`, convert.ts:26-78).
fn acp_prompt_to_parts(
    value: &serde_json::Value,
) -> Option<(String, Vec<crate::rpc::types::ContentBlock>)> {
    match value {
        serde_json::Value::String(text) => Some((text.clone(), Vec::new())),
        serde_json::Value::Array(blocks) => {
            Some((acp_blocks_to_text(blocks), acp_blocks_to_media(blocks)))
        }
        _ => None,
    }
}

/// ACP `image` content blocks (`{type:"image", mimeType, data}`) become native
/// media blocks; every other block type is flattened to text by
/// [`acp_blocks_to_text`].
fn acp_blocks_to_media(blocks: &[serde_json::Value]) -> Vec<crate::rpc::types::ContentBlock> {
    use crate::rpc::types::ContentBlock;
    let mut media = Vec::new();
    for block in blocks {
        if block.get("type").and_then(|value| value.as_str()) != Some("image") {
            continue;
        }
        let media_type = block
            .get("mimeType")
            .and_then(|value| value.as_str())
            .unwrap_or("image/png");
        let name = block
            .get("name")
            .and_then(|value| value.as_str())
            .map(|s| s.to_string());
        if let Some(data) = block.get("data").and_then(|value| value.as_str()) {
            media.push(ContentBlock::Image {
                media_type: media_type.to_string(),
                data: data.to_string(),
                name,
            });
        }
    }
    media
}

/// Flatten ACP content blocks into the plain prompt text the engine takes.
/// Text blocks pass through, text resources keep their uri provenance,
/// resource links become inline references, and audio / blob / unknown blocks
/// are dropped — the engine prompt pipeline is text-only.
fn acp_blocks_to_text(blocks: &[serde_json::Value]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in blocks {
        match block
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
        {
            "text" => {
                if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                    parts.push(text.to_string());
                }
            }
            "resource" => {
                let resource = block
                    .get("resource")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if let Some(text) = resource.get("text").and_then(|value| value.as_str()) {
                    let uri = resource
                        .get("uri")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    parts.push(format!(
                        "<resource uri=\"{}\">{}</resource>",
                        escape_xml_attr(uri),
                        text
                    ));
                }
            }
            "resource_link" => {
                let uri = block
                    .get("uri")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                parts.push(format!(
                    "<resource_link uri=\"{}\" name=\"{}\" />",
                    escape_xml_attr(uri),
                    escape_xml_attr(name)
                ));
            }
            _ => {}
        }
    }
    parts.join("\n")
}

fn escape_xml_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Convert one ACP `McpServer` entry (`NewSessionRequest.mcpServers`) into an
/// engine recipe: a `command` entry is stdio, a `url` entry is http (or sse
/// when `type` says so).
fn acp_mcp_recipe(server: &serde_json::Value) -> Option<McpServerRecipe> {
    let string_map = |value: Option<&serde_json::Value>| {
        value
            .and_then(|value| value.as_object())
            .map(|object| {
                object
                    .iter()
                    .filter_map(|(key, value)| {
                        value.as_str().map(|text| (key.clone(), text.to_string()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    if let Some(url) = server.get("url").and_then(|value| value.as_str()) {
        let headers = string_map(server.get("headers"));
        let bearer_token_env_var = server
            .get("bearerTokenEnvVar")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        return Some(match server.get("type").and_then(|value| value.as_str()) {
            Some("sse") => McpServerRecipe::Sse {
                url: url.to_string(),
                headers,
                bearer_token_env_var,
            },
            _ => McpServerRecipe::Http {
                url: url.to_string(),
                headers,
                bearer_token_env_var,
            },
        });
    }

    let command = server.get("command").and_then(|value| value.as_str())?;
    let args = server
        .get("args")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some(McpServerRecipe::Stdio {
        command: command.to_string(),
        args,
        env: string_map(server.get("env")),
        cwd: server
            .get("cwd")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

/// One locally answered command's `agent_message_chunk` (v2
/// `driveBuiltinCommand` / `driveUnknownCommand` push the rendered text before
/// settling the prompt).
fn local_command_update(session_id: &str, text: &str) -> serde_json::Value {
    json!({
        "sessionId": session_id,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": text },
        },
    })
}

/// The `session/prompt` response for a locally answered command: no turn ran.
fn local_command_response(session_id: &str, text: &str) -> serde_json::Value {
    json!({
        "sessionId": session_id,
        "stopReason": "end_turn",
        "content": text,
        "steps": 0,
        "usage": { "inputTokens": 0, "outputTokens": 0, "totalTokens": 0 },
    })
}

/// The level a session's thinking toggle shows: the stored override, the
/// model's declared default, else `on` for an always-thinking model and `off`
/// otherwise. An always-thinking model never shows `off` (v2 clamps the same
/// way at render time).
fn current_thinking_level(
    store: &SqliteSessionStore,
    session_id: &str,
    row: &crate::server::engine::ModelThinking,
) -> String {
    let stored = store
        .get_state("agent_config", session_id)
        .ok()
        .flatten()
        .and_then(|profile| {
            profile
                .get("thinking")
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let level = stored
        .or_else(|| row.default_effort.clone())
        .unwrap_or_else(|| {
            if row.always_thinking {
                "on".to_string()
            } else {
                "off".to_string()
            }
        });
    if row.always_thinking && level == "off" {
        "on".to_string()
    } else {
        level
    }
}

/// The v2 `thinking` select (config-options.ts `buildThinkingOption`): `off`
/// plus every declared effort (the boolean off/on pair when none are), `off`
/// dropped for always-thinking models, and an undeclared current level appended
/// so `currentValue` is always selectable.
fn thinking_option(current: &str, row: &crate::server::engine::ModelThinking) -> serde_json::Value {
    let mut values: Vec<String> = if row.support_efforts.is_empty() {
        vec!["off".to_string(), "on".to_string()]
    } else {
        std::iter::once("off".to_string())
            .chain(row.support_efforts.iter().cloned())
            .collect()
    };
    if row.always_thinking {
        values.retain(|value| value != "off");
    }
    if !values.iter().any(|value| value == current) {
        values.push(current.to_string());
    }
    let options: Vec<serde_json::Value> = values
        .iter()
        .map(|value| {
            json!({
                "value": value,
                "name": thinking_option_name(value),
            })
        })
        .collect();
    json!({
        "type": "select",
        "id": "thinking",
        "name": "Thinking",
        "category": "thought_level",
        "currentValue": current,
        "options": options,
    })
}

/// Display name for a thinking select entry (`off` → `Thinking Off`).
fn thinking_option_name(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => format!("Thinking {}{}", first.to_uppercase(), chars.as_str()),
        None => "Thinking".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_acp_initialize() {
        let server = AcpServer::in_memory().unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true } },
                "clientInfo": { "name": "zed", "version": "1.0.0" }
            }
        });

        let resp = server.handle_message(&req.to_string()).await.unwrap();
        assert_eq!(resp.id, Some(json!(1)));
        assert!(resp.error.is_none());
        let res = resp.result.unwrap();
        // ACP spec shape: numeric protocolVersion + camelCase capabilities.
        assert_eq!(res["protocolVersion"], 1);
        assert!(res.get("protocol_version").is_none());
        assert_eq!(res["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            res["agentCapabilities"]["promptCapabilities"]["image"],
            true
        );
        assert_eq!(
            res["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            true
        );
        assert!(res["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
        assert!(res["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
        assert!(res["agentCapabilities"]["sessionCapabilities"]["fork"].is_object());
        assert_eq!(res["agentCapabilities"]["mcpCapabilities"]["http"], true);
        assert_eq!(res["authMethods"][0]["id"], "login");
        assert_eq!(res["authMethods"][0]["type"], "terminal");
        assert_eq!(res["authMethods"][0]["args"], json!(["--login"]));
        assert_eq!(res["agentInfo"]["name"], "Kimi Code CLI");
    }

    /// A client below the minimum revision still receives the server's current
    /// one (v2 `negotiateVersion`, acp-server/src/version.ts:38-50).
    #[tokio::test]
    async fn test_acp_initialize_negotiates_version() {
        let server = AcpServer::in_memory().unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": 0 }
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        assert_eq!(resp.result.unwrap()["protocolVersion"], 1);
    }

    /// Notifications carry no id and must never be answered
    /// (v2 registers `session/cancel` as `onNotification`, server.ts:717).
    #[tokio::test]
    async fn test_acp_notification_is_not_answered() {
        let server = AcpServer::in_memory().unwrap();
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": "sess-1" }
        });
        assert!(
            server
                .handle_message(&notification.to_string())
                .await
                .is_none()
        );

        // Unknown notifications are dropped silently too.
        let unknown = json!({
            "jsonrpc": "2.0",
            "method": "no/such/notification",
            "params": {}
        });
        assert!(server.handle_message(&unknown.to_string()).await.is_none());

        // The same cancel sent as a request still gets an answer.
        let request = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "session/cancel",
            "params": { "sessionId": "sess-1" }
        });
        let resp = server.handle_message(&request.to_string()).await.unwrap();
        assert_eq!(resp.id, Some(json!(9)));
    }

    /// The next session update that is not one of the two attach-time
    /// notifications (`available_commands_update` / `session_info_update`),
    /// which every new/load/resume now pushes.
    fn next_non_attach_update(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<AcpOutbound>,
    ) -> serde_json::Value {
        loop {
            match rx.try_recv().expect("a session update") {
                AcpOutbound::Notification(note) => {
                    let params = note.params.unwrap_or_default();
                    let kind = params["update"]["sessionUpdate"]
                        .as_str()
                        .unwrap_or_default();
                    if kind == "available_commands_update" || kind == "session_info_update" {
                        continue;
                    }
                    return params;
                }
                other => panic!("unexpected outbound message: {other:?}"),
            }
        }
    }

    /// Drop attach-time notifications from a drained batch.
    fn drop_attach_updates(mut updates: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
        updates.retain(|update| {
            let kind = update["update"]["sessionUpdate"]
                .as_str()
                .unwrap_or_default();
            kind != "available_commands_update" && kind != "session_info_update"
        });
        updates
    }

    /// `session/new` advertises the canonical four modes (`modes.ts:24-46`).
    #[tokio::test]
    async fn test_acp_new_session_advertises_modes() {
        let server = AcpServer::in_memory().unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": "/tmp/workspace" }
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let res = resp.result.unwrap();
        assert!(res["sessionId"].is_string());
        assert_eq!(res["modes"]["currentModeId"], "default");
        let available = res["modes"]["availableModes"].as_array().unwrap();
        assert_eq!(available.len(), 4);
        assert_eq!(available[0]["id"], "default");
        assert_eq!(available[3]["id"], "yolo");
    }

    /// `session/new` binds the requested `cwd` to a workspace and records it
    /// in metadata, so `session/list`'s cwd filter and the engine's workspace
    /// root both resolve it.
    #[tokio::test]
    async fn test_acp_new_session_records_cwd_workspace() {
        let server = AcpServer::in_memory().unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/new",
            "params": { "cwd": "/tmp/acp-work", "title": "ACP Work" }
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let session_id = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let summary = server.store.get_session(&session_id).unwrap().unwrap();
        assert!(
            summary.workspace_id.is_some(),
            "a cwd must associate a workspace"
        );
        assert_eq!(server.session_cwd(&session_id), "/tmp/acp-work");
        let workspace = server
            .store
            .list_workspaces()
            .unwrap()
            .into_iter()
            .find(|workspace| workspace.root == "/tmp/acp-work")
            .expect("the cwd workspace is registered");
        assert_eq!(summary.workspace_id.as_deref(), Some(workspace.id.as_str()));
    }

    #[test]
    fn test_acp_mcp_recipe_transports() {
        let stdio = acp_mcp_recipe(&json!({
            "name": "fs",
            "command": "npx",
            "args": ["-y", "server-files"],
            "env": { "TOKEN": "x" }
        }))
        .expect("stdio recipe");
        match stdio {
            McpServerRecipe::Stdio {
                command, args, env, ..
            } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "server-files"]);
                assert_eq!(env.get("TOKEN").map(String::as_str), Some("x"));
            }
            other => panic!("expected stdio, got {other:?}"),
        }

        let http = acp_mcp_recipe(&json!({
            "name": "web",
            "type": "http",
            "url": "https://example.test/mcp",
            "headers": { "X-Auth": "y" }
        }))
        .expect("http recipe");
        match http {
            McpServerRecipe::Http { url, headers, .. } => {
                assert_eq!(url, "https://example.test/mcp");
                assert_eq!(headers.get("X-Auth").map(String::as_str), Some("y"));
            }
            other => panic!("expected http, got {other:?}"),
        }

        let sse = acp_mcp_recipe(&json!({
            "name": "stream",
            "type": "sse",
            "url": "https://example.test/sse"
        }))
        .expect("sse recipe");
        assert!(matches!(sse, McpServerRecipe::Sse { .. }));
        assert!(acp_mcp_recipe(&json!({ "name": "empty" })).is_none());
    }

    /// `session/new` advertises `configOptions` = `[model, mode]` with the
    /// canonical four-mode `mode` arm and a single-row `model` arm (no model
    /// catalog, v2 `buildSessionConfigOptions`, config-options.ts). The
    /// `thinking` arm is omitted because no catalog row declares thinking
    /// support.
    #[tokio::test]
    async fn test_acp_new_session_advertises_config_options() {
        let server = AcpServer::in_memory().unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let res = resp.result.unwrap();
        let options = res["configOptions"].as_array().unwrap();
        assert_eq!(
            options.len(),
            2,
            "model + mode, no thinking arm without a catalog"
        );

        let model = &options[0];
        assert_eq!(model["type"], "select");
        assert_eq!(model["id"], "model");
        assert_eq!(model["category"], "model");
        // No engine attached → the model arm is honest: empty currentValue and
        // no rows (v2 keeps the unbound defaults).
        assert_eq!(model["currentValue"], "");
        assert_eq!(model["options"].as_array().unwrap().len(), 0);

        let mode = &options[1];
        assert_eq!(mode["id"], "mode");
        assert_eq!(mode["category"], "mode");
        assert_eq!(mode["currentValue"], "default");
        let mode_options = mode["options"].as_array().unwrap();
        assert_eq!(mode_options.len(), 4);
        assert_eq!(mode_options[0]["value"], "default");
        assert_eq!(mode_options[0]["name"], "Default");
        assert_eq!(mode_options[3]["value"], "yolo");
    }

    /// With an engine attached, the `model` arm carries the engine's single
    /// model as both `currentValue` and the one selectable row.
    #[tokio::test]
    async fn test_acp_config_options_model_arm_from_engine() {
        use crate::pipeline::PipelineSpec;
        use crate::server::engine::ServerEngine;

        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = ServerEngine::new(
            PipelineSpec {
                system_prompt: "sys".into(),
                model_name: "kimi-k2".into(),
                providers: Vec::new(),
                native_llm: None,
                workspace_root: None,
                native_tools: false,
                extra_roots: Vec::new(),
                rust_self_contained: false,
                shell_path: None,
                policy_snapshot: None,
                github_token: None,
                github_base_url: None,
                subagent_timeout_ms: None,
                agent_tool_veto: None,
                tools_veto: None,
                todo_tool_veto: None,
                tower_worktree_root: None,
                tower_enabled: false,
                tool_select: false,
                sandbox_mode: None,
                sandbox_policy: None,
                caller_agent_id: None,
                session_id: None,
                secondary_model: None,
                image_read_byte_budget: None,
                image_max_edge_px: None,
                model_capabilities: None,
                skill_dirs: Vec::new(),
                plugin_skill_dirs: Vec::new(),
                merge_all_available_skills: true,
                background: crate::storage::BackgroundLimits::default(),
            },
            Arc::new(crate::server::hub::EventHub::new()),
            store.clone(),
        );
        let server = AcpServer::with_engine(store, Arc::new(engine));
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let res = resp.result.unwrap();
        let model = &res["configOptions"][0];
        assert_eq!(model["currentValue"], "kimi-k2");
        let rows = model["options"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["value"], "kimi-k2");
        assert_eq!(rows[0]["name"], "kimi-k2");
    }

    /// The auth gate refuses `session/new` with `auth_required` (`-32000`)
    /// when no engine (model) is attached and auth is not disabled
    /// (v2 `ensureAuthed`, server.ts:625-646).
    #[tokio::test]
    async fn test_acp_auth_gate_blocks_session_new_without_engine() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let server = AcpServer::new(store); // disable_auth = false, no engine
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32000);
        assert!(err.message.contains("Authentication required"));
    }

    /// `authenticate` validates `methodId` and re-checks the gate
    /// (v2 `authenticate`, server.ts:380-390).
    #[tokio::test]
    async fn test_acp_authenticate_validates_method_and_gate() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let server = AcpServer::new(store);

        // Wrong methodId → invalid params.
        let bad = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "authenticate",
            "params": { "methodId": "oauth" }
        });
        let resp = server.handle_message(&bad.to_string()).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);

        // Correct methodId but no engine → auth_required.
        let good = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "authenticate",
            "params": { "methodId": "login" }
        });
        let resp = server.handle_message(&good.to_string()).await.unwrap();
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32000);
    }

    /// With an engine attached, `authenticate('login')` succeeds and `logout`
    /// is a no-op success (the Rust engine has no managed token to drop).
    #[tokio::test]
    async fn test_acp_authenticate_succeeds_with_engine() {
        use crate::pipeline::PipelineSpec;
        use crate::server::engine::ServerEngine;

        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = ServerEngine::new(
            PipelineSpec {
                system_prompt: "sys".into(),
                model_name: "kimi-k2".into(),
                providers: Vec::new(),
                native_llm: None,
                workspace_root: None,
                native_tools: false,
                extra_roots: Vec::new(),
                rust_self_contained: false,
                shell_path: None,
                policy_snapshot: None,
                github_token: None,
                github_base_url: None,
                subagent_timeout_ms: None,
                agent_tool_veto: None,
                tools_veto: None,
                todo_tool_veto: None,
                tower_worktree_root: None,
                tower_enabled: false,
                tool_select: false,
                sandbox_mode: None,
                sandbox_policy: None,
                caller_agent_id: None,
                session_id: None,
                secondary_model: None,
                image_read_byte_budget: None,
                image_max_edge_px: None,
                model_capabilities: None,
                skill_dirs: Vec::new(),
                plugin_skill_dirs: Vec::new(),
                merge_all_available_skills: true,
                background: crate::storage::BackgroundLimits::default(),
            },
            Arc::new(crate::server::hub::EventHub::new()),
            store.clone(),
        );
        let server = AcpServer::with_engine(store, Arc::new(engine));

        let auth = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "authenticate",
            "params": { "methodId": "login" }
        });
        let resp = server.handle_message(&auth.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "engine attached → authed");

        let logout = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "logout",
            "params": {}
        });
        let resp = server.handle_message(&logout.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "logout is a no-op success");
    }

    /// ACP clients send `prompt` as `ContentBlock[]`; text, text resources and
    /// resource links survive, audio/image blocks are dropped
    /// (v2 `acpBlocksToContentParts`, convert.ts:26-78).
    #[tokio::test]
    async fn test_acp_prompt_accepts_content_blocks() {
        let server = AcpServer::in_memory().unwrap();
        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let prompt_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/prompt",
            "params": {
                "sessionId": sid,
                "prompt": [
                    { "type": "text", "text": "look at this" },
                    {
                        "type": "resource",
                        "resource": { "uri": "file:///tmp/a.txt", "text": "body" }
                    },
                    { "type": "resource_link", "uri": "file:///tmp/b.txt", "name": "b" },
                    { "type": "image", "mimeType": "image/png", "data": "AAAA" }
                ]
            }
        });
        let resp = server
            .handle_message(&prompt_req.to_string())
            .await
            .unwrap();
        // No engine is wired: the prompt is refused instead of being answered
        // with an invented reply, so nothing is persisted for it. The
        // block→text projection therefore has to be asserted directly (below)
        // rather than through a fabricated history entry.
        let err = resp
            .error
            .expect("a prompt with no engine wired must be refused, not faked");
        assert_eq!(err.code, -32000);

        let prompt_value = json!([
            { "type": "text", "text": "look at this" },
            {
                "type": "resource",
                "resource": { "uri": "file:///tmp/a.txt", "text": "body" }
            },
            { "type": "resource_link", "uri": "file:///tmp/b.txt", "name": "b" },
            { "type": "image", "mimeType": "image/png", "data": "AAAA" }
        ]);
        let (text, media) =
            acp_prompt_to_parts(&prompt_value).expect("the prompt must project to text");
        assert_eq!(
            text,
            "look at this\n<resource uri=\"file:///tmp/a.txt\">body</resource>\n\
             <resource_link uri=\"file:///tmp/b.txt\" name=\"b\" />"
        );
        assert_eq!(media.len(), 1, "the image block becomes a media attachment");
    }

    /// ACP image blocks become native media blocks; the text projection keeps
    /// only the textual blocks.
    #[test]
    fn test_acp_blocks_to_media_extracts_images() {
        let blocks = vec![
            json!({ "type": "text", "text": "look" }),
            json!({ "type": "image", "mimeType": "image/jpeg", "data": "AAAB" }),
            json!({ "type": "resource_link", "uri": "file:///b", "name": "b" }),
            json!({ "type": "image", "mimeType": "image/png", "data": "CCCC" }),
        ];
        let media = acp_blocks_to_media(&blocks);
        assert_eq!(media.len(), 2);
        match &media[0] {
            crate::rpc::types::ContentBlock::Image {
                media_type, data, ..
            } => {
                assert_eq!(media_type, "image/jpeg");
                assert_eq!(data, "AAAB");
            }
            other => panic!("expected image, got {other:?}"),
        }
        let (text, media) = acp_prompt_to_parts(&json!([
            { "type": "text", "text": "look" },
            { "type": "image", "mimeType": "image/png", "data": "CCCC" },
        ]))
        .unwrap();
        assert_eq!(text, "look");
        assert_eq!(media.len(), 1);
        assert!(acp_prompt_to_parts(&json!("plain")).is_some());
        assert!(acp_prompt_to_parts(&json!(42)).is_none());
    }

    /// `initialize` records the client's reverse-RPC capabilities.
    #[tokio::test]
    async fn test_acp_initialize_records_client_capabilities() {
        let server = AcpServer::in_memory().unwrap();
        assert!(!server.client_capabilities().fs.read_text_file);
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": true,
                },
            }
        });
        server.handle_message(&req.to_string()).await.unwrap();
        let capabilities = server.client_capabilities();
        assert!(capabilities.fs.read_text_file);
        assert!(capabilities.fs.write_text_file);
        assert!(capabilities.terminal);
    }

    /// `session/set_mode` validates the mode id, persists the permission mode
    /// the engine reads at turn start, and pushes `current_mode_update`
    /// (v2 `setSessionMode` + `acpModeToToggles`).
    #[tokio::test]
    async fn test_acp_set_mode_persists_permission_and_notifies() {
        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let set_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": "yolo" }
        });
        let resp = server.handle_message(&set_req.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(resp.result.unwrap()["modeId"], "yolo");

        let metadata = server.store.get_state("metadata", &sid).unwrap().unwrap();
        assert_eq!(metadata["permission_mode"], "yolo");

        let params = next_non_attach_update(&mut rx);
        assert_eq!(params["sessionId"], sid);
        assert_eq!(params["update"]["sessionUpdate"], "current_mode_update");
        assert_eq!(params["update"]["currentModeId"], "yolo");
    }

    /// v2 `acpModeToToggles`: `plan` applies BOTH toggles — the plan domain
    /// goes active and the permission mode lands. Only the permission
    /// half used to reach the engine, so an ACP client selecting Plan
    /// got manual permissions and no plan mode.
    #[tokio::test]
    async fn test_acp_set_mode_plan_activates_plan_mode() {
        let workspace = tempfile::TempDir::new().unwrap();
        let root = workspace.path().to_string_lossy().to_string();
        let server = AcpServer::in_memory().unwrap();

        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": root }
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let set_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": "plan" }
        });
        let resp = server.handle_message(&set_req.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");

        // Permission half (pre-existing behavior).
        let metadata = server.store.get_state("metadata", &sid).unwrap().unwrap();
        assert_eq!(metadata["permission_mode"], "manual");

        // Plan half: the `plan` domain the engine reads is now active. The
        // domain is session-scoped (v2 agent scope), like the native
        // `EnterPlanMode` write it must not shadow.
        let state =
            crate::storage::state_store::StateStore::for_session(workspace.path(), &sid).unwrap();
        assert_eq!(
            state.read_domain("plan").unwrap()["active"],
            true,
            "plan mode must be active after session/set_mode plan"
        );

        // Leaving plan mode for another mode clears the domain.
        let back = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": "default" }
        });
        let resp = server.handle_message(&back.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(
            state.read_domain("plan").unwrap()["active"],
            false,
            "leaving plan mode must clear the plan domain"
        );
    }

    /// Unknown ids are rejected with the v2 messages (server.ts:452-469).
    #[tokio::test]
    async fn test_acp_set_mode_rejects_unknown_ids() {
        let server = AcpServer::in_memory().unwrap();
        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let unknown_mode = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": "turbo" }
        });
        let resp = server
            .handle_message(&unknown_mode.to_string())
            .await
            .unwrap();
        assert_eq!(resp.error.as_ref().unwrap().code, -32602);
        assert_eq!(resp.error.unwrap().message, "Unknown modeId: turbo");

        let unknown_session = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/set_mode",
            "params": { "sessionId": "sess-nope", "modeId": "auto" }
        });
        let resp = server
            .handle_message(&unknown_session.to_string())
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().message, "Unknown sessionId: sess-nope");
    }

    /// `session/set_config_option` routes the `mode` arm to the same handler.
    #[tokio::test]
    async fn test_acp_set_config_option_mode_arm() {
        let server = AcpServer::in_memory().unwrap();
        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let mode_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "mode", "value": "auto" }
        });
        let resp = server.handle_message(&mode_req.to_string()).await.unwrap();
        let options = resp.result.unwrap()["configOptions"].clone();
        let mode_option = options
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == "mode")
            .expect("the mode option is advertised");
        assert_eq!(mode_option["currentValue"], "auto");
        assert_eq!(
            server.store.get_state("metadata", &sid).unwrap().unwrap()["permission_mode"],
            "auto"
        );

        let model_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "model", "value": "kimi-k2" }
        });
        let resp = server.handle_message(&model_req.to_string()).await.unwrap();
        // The `model` arm switches the session's model and answers with the
        // refreshed option set (v2 `setSessionConfigOption`).
        let options = resp.result.unwrap()["configOptions"].clone();
        let model_option = options
            .as_array()
            .unwrap()
            .iter()
            .find(|option| option["id"] == "model")
            .expect("the model option is advertised");
        assert_eq!(model_option["currentValue"], "kimi-k2");

        let unknown_req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "nonsense", "value": "x" }
        });
        let resp = server
            .handle_message(&unknown_req.to_string())
            .await
            .unwrap();
        assert_eq!(
            resp.error.unwrap().message,
            "Unsupported configId: nonsense"
        );
    }

    /// The forwarding seam must carry the *raw* event shapes the turn loop
    /// publishes, not just the typed variants. `run_turn` emits `llm.delta`
    /// with no turn id and tool events that use `tool_name` + a string
    /// `turn_id`; those are the payloads a live turn puts on the bus.
    #[tokio::test]
    async fn test_raw_turn_events_are_forwarded_to_the_client() {
        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let bus = Arc::new(EventBus::new());
        let _subscription = server.forward_session_events("sess-1", &bus);

        bus.publish_json(json!({
            "type": "llm.delta",
            "part": { "type": "text", "text": "hi" },
        }));
        bus.publish_json(json!({
            "type": "tool.call.started",
            "turn_id": "turn-1",
            "tool_call_id": "call_1",
            "tool_name": "Read",
            "args": { "path": "a.txt" },
        }));
        bus.publish_json(json!({
            "type": "tool.native",
            "turn_id": "turn-1",
            "tool_call_id": "call_1",
            "tool_name": "Read",
            "arguments": { "path": "a.txt" },
            "content": "file body",
            "is_error": false,
            "note": null,
        }));

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        assert_eq!(
            updates.len(),
            3,
            "every raw turn event must produce a session/update: {updates:?}"
        );
        assert_eq!(updates[0]["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(updates[0]["update"]["content"]["text"], "hi");
        assert_eq!(updates[1]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(updates[1]["update"]["toolCallId"], "turn-1:call_1");
        assert_eq!(updates[2]["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[2]["update"]["status"], "completed");
        assert_eq!(
            updates[2]["update"]["content"][0]["content"]["text"],
            "file body"
        );
    }

    /// Engine events reach the client as `session/update` notifications while
    /// a turn runs, and stop once the subscription is dropped.
    #[tokio::test]
    async fn test_session_update_notifications_are_forwarded() {
        use crate::events::types::EngineEvent;

        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let bus = Arc::new(EventBus::new());
        let subscription = server.forward_session_events("sess-1", &bus);

        bus.publish(&EngineEvent::AssistantDelta {
            agent_id: "main".into(),
            turn_id: "turn-3".into(),
            delta: "hi".into(),
        });
        bus.publish(&EngineEvent::TurnStarted {
            agent_id: "main".into(),
            turn_id: "turn-3".into(),
            prompt: None,
        });

        match rx.try_recv().expect("one notification") {
            AcpOutbound::Notification(note) => {
                assert_eq!(note.method, "session/update");
                let params = note.params.expect("notification params");
                assert_eq!(params["sessionId"], "sess-1");
                assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
                assert_eq!(params["update"]["content"]["text"], "hi");
            }
            other => panic!("unexpected outbound message: {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "unmapped events must not notify");

        bus.unsubscribe(subscription);
        bus.publish(&EngineEvent::AssistantDelta {
            agent_id: "main".into(),
            turn_id: "turn-3".into(),
            delta: "again".into(),
        });
        assert!(
            rx.try_recv().is_err(),
            "a dropped subscription stops forwarding"
        );
    }

    #[tokio::test]
    async fn test_acp_session_flow() {
        let server = AcpServer::in_memory().unwrap();

        // 1. Create session
        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "title": "ACP Test Session" }
        });
        let resp = server.handle_message(&new_req.to_string()).await.unwrap();
        let sid = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        // 2. List sessions
        let list_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/list",
        });
        let resp = server.handle_message(&list_req.to_string()).await.unwrap();
        let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["sessionId"], sid);
        assert_eq!(sessions[0]["cwd"], "");
        assert_eq!(sessions[0]["title"], "ACP Test Session");
        assert!(sessions[0]["updatedAt"].is_string());

        // 3. Prompt session
        let prompt_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/prompt",
            "params": {
                "sessionId": sid,
                "prompt": "Hello ACP"
            }
        });
        let resp = server
            .handle_message(&prompt_req.to_string())
            .await
            .unwrap();
        // No engine is wired in this test, so the server refuses the prompt
        // instead of inventing an assistant reply (which it also used to persist
        // into the session's history).
        let err = resp
            .error
            .expect("a prompt with no engine wired must be refused, not faked");
        assert_eq!(err.code, -32000);
        assert!(
            err.message.contains("No LLM engine is available"),
            "unexpected error: {}",
            err.message
        );

        // 4. Load session history — replayed as `session/update` chunks, the
        // response carries the mode state (v2 `loadSession`).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);
        let load_req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/load",
            "params": { "sessionId": sid }
        });
        let resp = server.handle_message(&load_req.to_string()).await.unwrap();
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["modes"]["currentModeId"], "default");
        let mut replayed = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                replayed.push(note.params.unwrap());
            }
        }
        let replayed = drop_attach_updates(replayed);
        // Nothing is replayed: the refused prompt persists no turn at all, where
        // it used to leave a fabricated user+assistant pair in the history.
        assert_eq!(
            replayed.len(),
            0,
            "a refused prompt must not fabricate a turn"
        );

        // 5. Set mode (ACP wire name `modeId`; the legacy `mode` still parses)
        let mode_req = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": "yolo" }
        });
        let resp = server.handle_message(&mode_req.to_string()).await.unwrap();
        assert_eq!(resp.result.unwrap()["modeId"], "yolo");

        // 6. Delete session
        let del_req = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/delete",
            "params": { "sessionId": sid }
        });
        let resp = server.handle_message(&del_req.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(resp.result.unwrap(), json!({}));

        // 7. Deleting an unknown session is invalid params, not a false answer.
        let del_missing = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "session/delete",
            "params": { "sessionId": "sess-nope" }
        });
        let resp = server
            .handle_message(&del_missing.to_string())
            .await
            .unwrap();
        assert_eq!(resp.error.unwrap().message, "Unknown sessionId: sess-nope");
    }

    /// A stored tool result replays as `tool_call_update` (v2 `replay.ts`).
    #[tokio::test]
    async fn test_acp_load_replays_tool_results() {
        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);
        server.store.create_session("sess-tools", None).unwrap();
        let messages = vec![
            crate::turn_loop::types::LLMMessage::user("run it"),
            crate::turn_loop::types::LLMMessage {
                role: "assistant".into(),
                content: "on it".into(),
                tool_calls: vec![crate::turn_loop::types::ToolCall {
                    id: "call-1".into(),
                    name: "Read".into(),
                    arguments: json!({ "path": "a.txt" }),
                    extras: None,
                }],
                ..Default::default()
            },
            crate::turn_loop::types::LLMMessage {
                role: "tool".into(),
                content: "tool output".into(),
                tool_call_id: Some("call-1".into()),
                ..Default::default()
            },
        ];
        server
            .store
            .save_turn("sess-tools", "t1", 1, &messages, None, None)
            .unwrap();

        let load = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/load",
            "params": { "sessionId": "sess-tools" }
        });
        server.handle_message(&load.to_string()).await.unwrap();

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap());
            }
        }
        let updates = drop_attach_updates(updates);
        // user chunk, assistant chunk, tool_call create, tool_call_update.
        assert_eq!(updates.len(), 4, "unexpected replay: {updates:?}");
        assert_eq!(updates[0]["update"]["sessionUpdate"], "user_message_chunk");
        assert_eq!(updates[1]["update"]["sessionUpdate"], "agent_message_chunk");
        // The create carries the turn-namespaced id and the parsed args.
        assert_eq!(updates[2]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(updates[2]["update"]["toolCallId"], "t1:call-1");
        assert_eq!(updates[2]["update"]["title"], "Read");
        assert_eq!(updates[2]["update"]["kind"], "read");
        assert_eq!(updates[2]["update"]["status"], "in_progress");
        assert_eq!(updates[2]["update"]["rawInput"]["path"], "a.txt");
        // The terminal lands on the same wire id the create opened.
        assert_eq!(updates[3]["update"]["sessionUpdate"], "tool_call_update");
        assert_eq!(updates[3]["update"]["toolCallId"], "t1:call-1");
        assert_eq!(updates[3]["update"]["status"], "completed");
        assert_eq!(updates[3]["update"]["rawOutput"], "tool output");
        assert_eq!(
            updates[3]["update"]["content"][0]["type"],
            json!("content"),
            "tool content must carry the ACP wrapper"
        );
    }

    /// `session/list` projects storage rows into the ACP `SessionInfo` shape,
    /// resolves `cwd` from the session metadata, and keeps cwd-less sessions
    /// when the request filters by cwd (v2 `sessionSummaryToSessionInfo` +
    /// `filterSessionSummariesByCwd`).
    #[tokio::test]
    async fn test_acp_session_list_shape_and_cwd_filter() {
        let server = AcpServer::in_memory().unwrap();
        server.store.create_session("sess-a", Some("A")).unwrap();
        server.store.create_session("sess-b", Some("B")).unwrap();
        server
            .store
            .put_state("metadata", "sess-b", &json!({ "cwd": "/tmp/ws" }))
            .unwrap();

        let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {} });
        let resp = server.handle_message(&list.to_string()).await.unwrap();
        let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
        assert_eq!(sessions.len(), 2);
        let session_b = sessions
            .iter()
            .find(|session| session["sessionId"] == "sess-b")
            .expect("sess-b listed");
        assert_eq!(session_b["cwd"], "/tmp/ws");
        assert_eq!(session_b["title"], "B");
        assert!(session_b["updatedAt"].is_string());

        let matching = json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/list",
            "params": { "cwd": "/tmp/ws" }
        });
        let resp = server.handle_message(&matching.to_string()).await.unwrap();
        let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
        assert_eq!(sessions.len(), 2, "a cwd-less session is always kept");

        let other = json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/list",
            "params": { "cwd": "/tmp/other" }
        });
        let resp = server.handle_message(&other.to_string()).await.unwrap();
        let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["sessionId"], "sess-a");
    }

    /// `session/resume` re-attaches without replay; `session/fork` copies the
    /// source history into a new session (v2 `resumeSession` / `forkSession`).
    #[tokio::test]
    async fn test_acp_resume_and_fork() {
        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let prompt_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": "hi" }
        });
        let resp = server
            .handle_message(&prompt_req.to_string())
            .await
            .unwrap();
        // No engine is wired, so the prompt is refused instead of being answered
        // with an invented reply. Seed the history this test needs explicitly —
        // it used to come from that fabricated turn.
        assert!(
            resp.error.is_some(),
            "a prompt with no engine wired must be refused, not faked"
        );
        use crate::turn_loop::types::LLMMessage;
        server
            .store
            .save_turn(&sid, "turn-1", 1, &[LLMMessage::user("hi")], None, None)
            .expect("the seed turn must persist");

        // Drop the session/new attach notifications so the resume assertion
        // below measures only what resume itself sends.
        while rx.try_recv().is_ok() {}
        // resume: mode state only, no replayed chunks.
        let resume_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/resume",
            "params": { "sessionId": sid }
        });
        let resp = server
            .handle_message(&resume_req.to_string())
            .await
            .unwrap();
        assert_eq!(resp.result.unwrap()["modes"]["currentModeId"], "default");
        let mut resumed = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                resumed.push(note.params.unwrap_or_default());
            }
        }
        assert!(
            drop_attach_updates(resumed).is_empty(),
            "resume must not replay history (that is session/load)"
        );

        // fork: new id, copied history.
        let fork_req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/fork",
            "params": { "sessionId": sid }
        });
        let resp = server.handle_message(&fork_req.to_string()).await.unwrap();
        let forked = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(forked, sid);
        assert_eq!(
            server.store.load_session_history(&forked).unwrap()[0].content,
            "hi"
        );

        let unknown_fork = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "session/fork",
            "params": { "sessionId": "sess-nope" }
        });
        assert_eq!(
            server
                .handle_message(&unknown_fork.to_string())
                .await
                .unwrap()
                .error
                .unwrap()
                .message,
            "Unknown sessionId: sess-nope"
        );

        let unknown_resume = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/resume",
            "params": { "sessionId": "sess-nope" }
        });
        assert_eq!(
            server
                .handle_message(&unknown_resume.to_string())
                .await
                .unwrap()
                .error
                .unwrap()
                .message,
            "Unknown sessionId: sess-nope"
        );
    }

    /// `session/close` tears the session down best-effort and clears its local
    /// mode; an unknown session is rejected (v2 `closeSession`).
    #[tokio::test]
    async fn test_acp_close_session_clears_mode() {
        let server = AcpServer::in_memory().unwrap();
        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let set_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_mode",
            "params": { "sessionId": sid, "modeId": "plan" }
        });
        server.handle_message(&set_req.to_string()).await.unwrap();
        assert_eq!(server.current_mode(&sid), "plan");

        let close_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/close",
            "params": { "sessionId": sid }
        });
        let resp = server.handle_message(&close_req.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(resp.result.unwrap(), json!({}));
        assert_eq!(server.current_mode(&sid), "default");

        let unknown = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/close",
            "params": { "sessionId": "sess-nope" }
        });
        let resp = server.handle_message(&unknown.to_string()).await.unwrap();
        assert_eq!(resp.error.unwrap().message, "Unknown sessionId: sess-nope");
    }

    /// `session/new` records `additionalDirectories` on the session so the
    /// engine can merge them into the authorized boundary at turn start
    /// (v2 `sessions.create({ additionalDirs })`).
    #[tokio::test]
    async fn test_acp_new_session_records_additional_directories() {
        let server = AcpServer::in_memory().unwrap();
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {
                "cwd": "/tmp/ws",
                "additionalDirectories": ["/tmp/extra-a", "/tmp/extra-b"]
            }
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let sid = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let metadata = server.store.get_state("metadata", &sid).unwrap().unwrap();
        assert_eq!(metadata["cwd"], "/tmp/ws");
        assert_eq!(
            metadata["additional_dirs"],
            json!(["/tmp/extra-a", "/tmp/extra-b"])
        );

        // A session created without the field carries no empty list.
        let plain = json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {}
        });
        let resp = server.handle_message(&plain.to_string()).await.unwrap();
        let plain_sid = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            server
                .store
                .get_state("metadata", &plain_sid)
                .unwrap()
                .is_none()
        );
    }

    /// ACP `mcpServers` are ephemeral per-session servers (v2
    /// `sessions.create({ mcpServers })`): they must not land on the
    /// process-wide manager every other session shares.
    #[tokio::test]
    async fn test_acp_new_session_scopes_mcp_servers_to_the_session() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, "http://127.0.0.1:1/v1");
        let shared = Arc::new(crate::mcp::manager::McpManager::new());
        engine.set_mcp_manager(shared.clone());
        let server = AcpServer::with_engine(store, engine.clone());

        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {
                "mcpServers": [{
                    "name": "editor-tools",
                    "command": "node",
                    "args": ["server.js"]
                }]
            }
        });
        let resp = server.handle_message(&req.to_string()).await.unwrap();
        let sid = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let session_manager = engine
            .mcp_manager_for(&sid)
            .expect("the session has a manager");
        assert!(
            !Arc::ptr_eq(&session_manager, &shared),
            "the client's servers must not land on the shared manager"
        );
        let names = |entries: Vec<crate::mcp::manager::McpServerEntry>| {
            entries
                .into_iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>()
        };
        assert!(
            names(session_manager.server_entries().await).contains(&"editor-tools".to_string()),
            "the session's own manager carries the client's server"
        );
        assert!(
            !names(shared.server_entries().await).contains(&"editor-tools".to_string()),
            "the shared manager stays untouched"
        );

        // A session created without the field keeps the shared manager.
        let plain = json!({
            "jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {}
        });
        let resp = server.handle_message(&plain.to_string()).await.unwrap();
        let plain_sid = resp.result.unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(Arc::ptr_eq(
            &engine.mcp_manager_for(&plain_sid).unwrap(),
            &shared
        ));
    }

    /// `session/set_model` (the custom extension) and the `model` arm of
    /// `session/set_config_option` both persist the per-session override and
    /// push `config_option_update` (v2 `setSessionModel` /
    /// `setSessionConfigOption`).
    #[tokio::test]
    async fn test_acp_set_model_persists_and_notifies() {
        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let set_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_model",
            "params": { "sessionId": sid, "modelId": "kimi-k2" }
        });
        let resp = server.handle_message(&set_req.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(resp.result.unwrap(), json!({}));
        assert_eq!(
            server
                .store
                .get_state("agent_config", &sid)
                .unwrap()
                .unwrap()["model"],
            "kimi-k2"
        );
        assert_eq!(server.session_model(&sid), "kimi-k2");

        let params = next_non_attach_update(&mut rx);
        assert_eq!(params["update"]["sessionUpdate"], "config_option_update");
        assert_eq!(params["update"]["configOptions"][0]["id"], "model");
        assert_eq!(
            params["update"]["configOptions"][0]["currentValue"],
            "kimi-k2"
        );

        // The config-option route reaches the same state and answers with the
        // refreshed option surface.
        let config_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "model", "value": "kimi-k3" }
        });
        let resp = server
            .handle_message(&config_req.to_string())
            .await
            .unwrap();
        assert_eq!(
            resp.result.unwrap()["configOptions"][0]["currentValue"],
            "kimi-k3"
        );
        assert_eq!(server.session_model(&sid), "kimi-k3");

        // Unknown ids and unknown config ids stay invalid params.
        let unknown_session = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/set_model",
            "params": { "sessionId": "sess-nope", "modelId": "kimi-k2" }
        });
        assert_eq!(
            server
                .handle_message(&unknown_session.to_string())
                .await
                .unwrap()
                .error
                .unwrap()
                .message,
            "Unknown sessionId: sess-nope"
        );
        let unknown_config = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "thinking", "value": "on" }
        });
        assert_eq!(
            server
                .handle_message(&unknown_config.to_string())
                .await
                .unwrap()
                .error
                .unwrap()
                .message,
            "Unsupported configId: thinking"
        );
    }

    /// A second `session/prompt` for a session that already has one running is
    /// refused instead of racing the first turn's cancel token
    /// (v2 `assertNoActiveTurn`).
    #[tokio::test]
    async fn test_acp_second_prompt_for_a_busy_session_is_refused() {
        let server = AcpServer::in_memory().unwrap();
        assert!(server.begin_prompt("7", "sess-1"));
        assert!(
            !server.begin_prompt("8", "sess-1"),
            "one turn per session at a time"
        );
        assert!(
            server.begin_prompt("9", "sess-2"),
            "a different session is unaffected"
        );
        server.end_prompt("7");
        assert!(server.begin_prompt("10", "sess-1"));
    }

    /// `$/cancel_request` names the request to abort; the in-flight prompt's
    /// session is what actually gets cancelled, and the notification itself is
    /// never answered.
    #[tokio::test]
    async fn test_acp_cancel_request_targets_the_named_prompt() {
        let server = AcpServer::in_memory().unwrap();
        assert!(server.begin_prompt("7", "sess-1"));

        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": { "requestId": 7 }
        });
        assert!(
            server.handle_message(&cancel.to_string()).await.is_none(),
            "a notification must never be answered"
        );

        // The same message as a request answers, and an unknown request id is
        // reported as not cancelled rather than as a method-not-found.
        let unknown = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "$/cancel_request",
            "params": { "requestId": "no-such-request" }
        });
        let resp = server.handle_message(&unknown.to_string()).await.unwrap();
        assert_eq!(resp.result.unwrap()["cancelled"], false);
    }

    /// A local OpenAI-compatible SSE endpoint that streams one delta and then
    /// holds the connection open: only a cancellation can end the turn.
    async fn spawn_hanging_openai_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        (format!("http://{addr}/v1"), server)
    }

    /// A local OpenAI-compatible SSE endpoint that answers a complete turn.
    async fn spawn_completing_openai_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                        data: [DONE]\n\n";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
        });
        (format!("http://{addr}/v1"), server)
    }

    /// A local OpenAI-compatible SSE endpoint that answers `count` complete
    /// turns, recording each request body's prompt text.
    async fn spawn_repeating_openai_server(
        count: usize,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for _ in 0..count {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 65536];
                let read = sock.read(&mut buf).await.unwrap_or(0);
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..read]).into_owned());
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                            data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                            data: [DONE]\n\n";
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (format!("http://{addr}/v1"), requests, server)
    }

    /// An engine whose one model is the local mock endpoint.
    fn engine_with_native_llm(
        store: &Arc<SqliteSessionStore>,
        base_url: &str,
    ) -> Arc<crate::server::engine::ServerEngine> {
        engine_with_native_llm_and_skills(store, base_url, Vec::new())
    }

    /// [`engine_with_native_llm`] with extra skill roots (the engine's
    /// `extra_skill_dirs`, which the session catalog and the `Skill` tool scan).
    fn engine_with_native_llm_and_skills(
        store: &Arc<SqliteSessionStore>,
        base_url: &str,
        skill_dirs: Vec<std::path::PathBuf>,
    ) -> Arc<crate::server::engine::ServerEngine> {
        use crate::pipeline::PipelineSpec;
        use crate::server::engine::ServerEngine;

        Arc::new(ServerEngine::new(
            PipelineSpec {
                system_prompt: "sys".into(),
                model_name: "mock-model".into(),
                providers: Vec::new(),
                native_llm: Some(crate::rpc::types::NativeLlmConfig {
                    protocol: "openai".into(),
                    base_url: base_url.to_string(),
                    api_key: "test-key".into(),
                    model: "mock-model".into(),
                    ..Default::default()
                }),
                workspace_root: None,
                native_tools: false,
                extra_roots: Vec::new(),
                rust_self_contained: true,
                shell_path: None,
                policy_snapshot: None,
                github_token: None,
                github_base_url: None,
                subagent_timeout_ms: None,
                agent_tool_veto: None,
                tools_veto: None,
                todo_tool_veto: None,
                tower_worktree_root: None,
                tower_enabled: false,
                tool_select: false,
                sandbox_mode: None,
                sandbox_policy: None,
                caller_agent_id: None,
                session_id: None,
                secondary_model: None,
                image_read_byte_budget: None,
                image_max_edge_px: None,
                model_capabilities: None,
                skill_dirs,
                plugin_skill_dirs: Vec::new(),
                merge_all_available_skills: true,
                background: crate::storage::BackgroundLimits::default(),
            },
            Arc::new(crate::server::hub::EventHub::new()),
            store.clone(),
        ))
    }

    /// A completed turn answers with the ACP `end_turn`, not the engine's Rust
    /// enum name — a strict client parses `stopReason` against the ACP enum.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acp_prompt_reports_acp_stop_reason() {
        let (base_url, mock) = spawn_completing_openai_server().await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let server = AcpServer::with_engine(store, engine);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": "hello" }
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            server.handle_message(&prompt.to_string()),
        )
        .await
        .expect("the turn must settle")
        .unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(resp.result.unwrap()["stopReason"], "end_turn");

        mock.abort();
    }

    /// A real turn must stream its output to the client. This is the
    /// end-to-end counterpart of `test_raw_turn_events_are_forwarded_to_the_client`:
    /// it drives the actual engine through `session/prompt` and asserts the
    /// client received the model's text, so a mapper that silently drops every
    /// production event shape fails here instead of passing unit tests that
    /// hand-build their input.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_real_turn_streams_session_updates() {
        let (base_url, mock) = spawn_completing_openai_server().await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let server = Arc::new(AcpServer::with_engine(store, engine));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": "hello" }
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            server.handle_message(&prompt.to_string()),
        )
        .await
        .expect("the turn must settle")
        .unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        assert!(
            updates.iter().any(
                |update| update["update"]["sessionUpdate"] == "agent_message_chunk"
                    && update["update"]["content"]["text"] == "hi"
            ),
            "the model's text must reach the client as agent_message_chunk: {updates:?}"
        );

        mock.abort();
    }

    /// `session/new` advertises the builtin command palette and the initial
    /// title (v2 `emitAvailableCommandsUpdate` + `session_info_update`).
    #[tokio::test]
    async fn test_acp_new_session_emits_commands_and_info() {
        let server = AcpServer::in_memory().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "title": "My Session" }
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        let commands = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "available_commands_update")
            .unwrap_or_else(|| panic!("missing available_commands_update: {updates:?}"));
        let names: Vec<&str> = commands["update"]["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec!["compact", "status", "usage", "mcp", "tasks", "help"]
        );
        assert_eq!(commands["sessionId"], sid);

        let info = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "session_info_update")
            .unwrap_or_else(|| panic!("missing session_info_update: {updates:?}"));
        assert_eq!(info["update"]["title"], "My Session");
    }

    /// The advertised builtins answer locally: no model turn runs, the text
    /// comes back as one `agent_message_chunk`, and `/usage` reads the
    /// session's durable counters.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acp_builtin_commands_run_locally() {
        let (base_url, mock) = spawn_completing_openai_server().await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let server = AcpServer::with_engine(store.clone(), engine);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        crate::session::usage::record_turn(
            &store,
            &sid,
            "mock-model",
            &crate::rpc::types::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            },
        )
        .unwrap();

        let prompt = |id: u64, text: &str| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": { "sessionId": sid, "prompt": text }
            })
        };

        let help = server
            .handle_message(&prompt(2, "/help").to_string())
            .await
            .unwrap();
        let help = help.result.expect("help answers");
        assert_eq!(help["stopReason"], "end_turn");
        assert!(help["content"].as_str().unwrap().contains("/status"));

        let status = server
            .handle_message(&prompt(3, "/status").to_string())
            .await
            .unwrap()
            .result
            .expect("status answers");
        let status_text = status["content"].as_str().unwrap();
        assert!(
            status_text.contains(&format!("Session: {sid}")),
            "{status_text}"
        );
        assert!(status_text.contains("Model: mock-model"), "{status_text}");

        let usage = server
            .handle_message(&prompt(4, "/usage").to_string())
            .await
            .unwrap()
            .result
            .expect("usage answers");
        let usage_text = usage["content"].as_str().unwrap();
        assert!(
            usage_text.contains("Session total: 10 input, 5 output"),
            "{usage_text}"
        );

        mock.abort();
    }

    /// A title change outside ACP (a generated or host-renamed title) reaches
    /// the client as `session_info_update` at the next prompt.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acp_session_info_update_follows_title_change() {
        let (base_url, mock) = spawn_completing_openai_server().await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let server = AcpServer::with_engine(store.clone(), engine);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "title": "First" }
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        while rx.try_recv().is_ok() {}

        server
            .store
            .update_session_title(&sid, Some("Second"))
            .unwrap();
        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": "/status" }
        });
        server.handle_message(&prompt.to_string()).await.unwrap();

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        let info = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "session_info_update")
            .unwrap_or_else(|| panic!("missing title update: {updates:?}"));
        assert_eq!(info["update"]["title"], "Second");

        mock.abort();
    }

    /// After a turn the client gets the v2 `usage_update` (context used against
    /// the configured model window) and `plan` (the todo list the agent left)
    /// notifications, not just the final response.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_real_turn_emits_usage_and_plan_updates() {
        let (base_url, mock) = spawn_completing_openai_server().await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let mut config = crate::config::KimiConfig::default();
        config.models.insert(
            "mock-model".into(),
            crate::config::ModelAliasConfig {
                max_context_size: Some(42_000),
                ..Default::default()
            },
        );
        engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
        let server = Arc::new(AcpServer::with_engine(store, engine));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let workspace = tempfile::tempdir().unwrap();
        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": workspace.path().to_str().unwrap() }
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        // The todo tools write this domain; the ACP server reads the same one.
        let state = crate::storage::StateStore::for_workspace(workspace.path()).unwrap();
        state
            .write_domain(
                "todo",
                &json!([
                    { "id": "T1", "title": "Read the code", "status": "pending" },
                    { "id": "T2", "title": "Apply the fix", "status": "done" }
                ]),
            )
            .unwrap();

        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": "hello" }
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            server.handle_message(&prompt.to_string()),
        )
        .await
        .expect("the turn must settle")
        .unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        let usage = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "usage_update")
            .unwrap_or_else(|| panic!("missing usage_update: {updates:?}"));
        assert_eq!(usage["sessionId"], sid);
        assert_eq!(usage["update"]["size"], 42_000);
        assert!(
            usage["update"]["used"].as_u64().unwrap_or(0) > 0,
            "used must reflect the persisted conversation: {usage:?}"
        );

        let plan = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "plan")
            .unwrap_or_else(|| panic!("missing plan: {updates:?}"));
        assert_eq!(
            plan["update"]["entries"],
            json!([
                { "content": "Read the code", "priority": "medium", "status": "pending" },
                { "content": "Apply the fix", "priority": "medium", "status": "completed" },
            ])
        );

        mock.abort();
    }

    /// `$/cancel_request` reaches a turn that is still streaming and settles
    /// the prompt with the ACP `cancelled` stop reason — the response the ACP
    /// cancellation contract requires.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acp_cancel_request_stops_a_running_turn() {
        let (base_url, mock) = spawn_hanging_openai_server().await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let server = Arc::new(AcpServer::with_engine(store, engine.clone()));

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        // The prompt runs on its own task, exactly as `run_stdio` dispatches
        // it: the reader has to stay free to deliver the cancellation.
        let prompt_server = server.clone();
        let prompt_sid = sid.clone();
        let prompt = tokio::spawn(async move {
            prompt_server
                .handle_message(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 7,
                        "method": "session/prompt",
                        "params": { "sessionId": prompt_sid, "prompt": "hello" }
                    })
                    .to_string(),
                )
                .await
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !engine.is_turn_active(&sid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the turn never started"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "$/cancel_request",
            "params": { "requestId": 7 }
        });
        assert!(
            server.handle_message(&cancel.to_string()).await.is_none(),
            "a notification must never be answered"
        );

        let resp = tokio::time::timeout(std::time::Duration::from_secs(20), prompt)
            .await
            .expect("a cancelled prompt must settle promptly")
            .unwrap()
            .unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");
        assert_eq!(resp.result.unwrap()["stopReason"], "cancelled");

        mock.abort();
    }

    /// v2 `driveSkillActivation` + `driveUnknownCommand`: a `/skill:` command
    /// renders through the engine's own scan and drives a normal turn with the
    /// activation origin, while an unknown slash command is answered locally
    /// and never reaches the model.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acp_skill_commands_activate_and_unknown_commands_answer_locally() {
        let (base_url, mock) = spawn_completing_openai_server().await;
        let skills_root = tempfile::tempdir().unwrap();
        let skill_dir = skills_root.path().join("demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill for ACP\n---\n\nDo the demo: $ARGUMENTS\n",
        )
        .unwrap();

        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm_and_skills(
            &store,
            &base_url,
            vec![skills_root.path().to_path_buf()],
        );
        let server = AcpServer::with_engine(store.clone(), engine);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        let commands = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "available_commands_update")
            .expect("session/new advertises commands");
        let names: Vec<&str> = commands["update"]["availableCommands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert!(
            names.contains(&"skill:demo"),
            "the project skill must be advertised: {names:?}"
        );

        let prompt = |id: u64, text: &str| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/prompt",
                "params": { "sessionId": sid, "prompt": text }
            })
        };

        // Unknown slash commands never reach the model.
        let unknown = server
            .handle_message(&prompt(2, "/nope now").to_string())
            .await
            .unwrap()
            .result
            .expect("an unknown command answers");
        assert_eq!(unknown["stopReason"], "end_turn");
        assert!(
            unknown["content"]
                .as_str()
                .unwrap()
                .contains("Unknown ACP command: /nope"),
            "{unknown}"
        );

        // The skill activation runs a normal turn against the engine.
        let activated = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            server.handle_message(&prompt(3, "/skill:demo hello world").to_string()),
        )
        .await
        .expect("the skill turn must settle")
        .unwrap()
        .result
        .expect("the skill activation answers");
        assert_eq!(activated["stopReason"], "end_turn");

        let history = store.load_session_history(&sid).unwrap_or_default();
        let activation = history
            .iter()
            .find(|message| message.content.contains("<skill-loaded"))
            .expect("the rendered skill prompt is part of the turn");
        assert!(
            activation.content.contains("Do the demo: hello world"),
            "the activation must expand $ARGUMENTS: {}",
            activation.content
        );
        let turns = store.list_turns(&sid).unwrap();
        let origin = turns
            .iter()
            .find_map(|turn| turn.origin.as_ref())
            .expect("the turn carries an origin");
        assert_eq!(
            origin.get("kind").and_then(|kind| kind.as_str()),
            Some("skill_activation"),
            "the turn carries the v2 skill activation origin"
        );

        mock.abort();
    }

    /// The `thinking` config option (v2 `buildThinkingOption`): it appears only
    /// for a model that declares a thinking axis, follows the model's declared
    /// efforts, clamps always-thinking models to `on`, and disappears when the
    /// session switches to a model without the axis.
    #[tokio::test]
    async fn test_acp_thinking_option_follows_the_model_catalog() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, "http://127.0.0.1:1/v1");
        let config: crate::config::KimiConfig = r#"
[providers.acme]
type = "openai"
api_key = "k"

[models.thinky]
provider = "acme"
model = "thinky"
capabilities = ["thinking", "tool_use"]
support_efforts = ["low", "high"]
default_effort = "high"

[models.always]
provider = "acme"
model = "always"
capabilities = ["always_thinking"]
support_efforts = ["low", "high"]

[models.plain]
provider = "acme"
model = "plain"
"#
        .parse()
        .expect("parse config");
        engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
        let server = AcpServer::with_engine(store.clone(), engine);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let options = |response: &JsonRpcResponse| -> Vec<serde_json::Value> {
            response.result.clone().unwrap()["configOptions"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        };
        let thinking = |options: &[serde_json::Value]| -> Option<serde_json::Value> {
            options
                .iter()
                .find(|option| option["id"] == "thinking")
                .cloned()
        };

        // The engine's default model has no catalog row: no thinking option.
        let set_model = |id: u64, model: &str| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/set_config_option",
                "params": { "sessionId": sid, "configId": "model", "value": model }
            })
        };
        let initial = server
            .handle_message(&set_model(2, "thinky").to_string())
            .await
            .unwrap();
        let options_now = options(&initial);
        let option =
            thinking(&options_now).expect("a thinking-capable model advertises the toggle");
        assert_eq!(option["category"], "thought_level");
        assert_eq!(option["currentValue"], "high", "the declared default wins");
        let values: Vec<&str> = option["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, vec!["off", "low", "high"]);

        let set_thinking = |id: u64, value: &str| {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/set_config_option",
                "params": { "sessionId": sid, "configId": "thinking", "value": value }
            })
        };
        let chosen = server
            .handle_message(&set_thinking(3, "low").to_string())
            .await
            .unwrap();
        let option = thinking(&options(&chosen)).expect("the toggle stays advertised");
        assert_eq!(option["currentValue"], "low");
        assert_eq!(
            store.get_state("agent_config", &sid).unwrap().unwrap()["thinking"],
            "low"
        );

        let invalid = server
            .handle_message(&set_thinking(4, "turbo").to_string())
            .await
            .unwrap();
        assert!(
            invalid
                .error
                .unwrap()
                .message
                .contains("Invalid thinking level"),
            "unknown levels are refused"
        );

        // Always-thinking: `off` is not offered and a request for it clamps.
        // The stored effort level carries over while the new model accepts it.
        let always = server
            .handle_message(&set_model(5, "always").to_string())
            .await
            .unwrap();
        let option = thinking(&options(&always)).expect("always-thinking still advertises");
        assert_eq!(option["currentValue"], "low");
        let values: Vec<&str> = option["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["value"].as_str().unwrap())
            .collect();
        assert!(!values.contains(&"off"), "{values:?}");
        let clamped = server
            .handle_message(&set_thinking(6, "off").to_string())
            .await
            .unwrap();
        assert_eq!(
            thinking(&options(&clamped)).unwrap()["currentValue"],
            "on",
            "an always-thinking model clamps off to on"
        );

        // A model without the axis drops the option again.
        let plain = server
            .handle_message(&set_model(7, "plain").to_string())
            .await
            .unwrap();
        assert!(
            thinking(&options(&plain)).is_none(),
            "a non-thinking model advertises no toggle"
        );
    }

    /// `session/load` replays the transcript and the session's carried state:
    /// the todo list behind the ACP `plan` notification and the context usage
    /// the reattached client cannot derive from messages alone.
    #[tokio::test]
    async fn test_acp_session_load_replays_plan_and_usage() {
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let workspace = tempfile::tempdir().unwrap();
        let engine = engine_with_native_llm(&store, "http://127.0.0.1:1/v1");
        let config: crate::config::KimiConfig = r#"
[providers.acme]
type = "openai"
api_key = "k"

[models.mock-model]
provider = "acme"
model = "mock-model"
max_context_size = 100000
"#
        .parse()
        .expect("parse config");
        engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
        let server = AcpServer::with_engine(store.clone(), engine);

        let new_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/new",
            "params": { "cwd": workspace.path().to_str().unwrap() }
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        let state = crate::storage::StateStore::for_session(workspace.path(), &sid).unwrap();
        state
            .write_domain(
                "todo",
                &json!([{ "id": "T1", "title": "Replay me", "status": "in_progress" }]),
            )
            .unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
        server.set_notification_sink(tx);
        let load_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/load",
            "params": { "sessionId": sid }
        });
        let resp = server.handle_message(&load_req.to_string()).await.unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");

        let mut updates = Vec::new();
        while let Ok(message) = rx.try_recv() {
            if let AcpOutbound::Notification(note) = message {
                updates.push(note.params.unwrap_or_default());
            }
        }
        let plan = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "plan")
            .unwrap_or_else(|| panic!("missing replayed plan: {updates:?}"));
        assert_eq!(plan["update"]["entries"][0]["content"], "Replay me");
        let usage = updates
            .iter()
            .find(|update| update["update"]["sessionUpdate"] == "usage_update")
            .unwrap_or_else(|| panic!("missing replayed usage: {updates:?}"));
        assert_eq!(usage["update"]["size"], 100000);
    }
    /// v2 `loop.notify` on the ACP entry point: a background completion that
    /// is waiting when a prompt runs becomes its own model turn after the
    /// prompt response, carrying the rendered notification and task origin.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_acp_prompt_drains_background_task_notifications() {
        let (base_url, requests, mock) = spawn_repeating_openai_server(2).await;
        let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
        let engine = engine_with_native_llm(&store, &base_url);
        let runner = Arc::new(crate::storage::TaskRunner::new(None));
        engine
            .subagent_manager()
            .set_task_runner(runner.clone())
            .await;
        let server = AcpServer::with_engine(store.clone(), engine);

        let new_req = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
        });
        let sid = server
            .handle_message(&new_req.to_string())
            .await
            .unwrap()
            .result
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

        runner
            .spawn_task_with_meta(
                crate::storage::TaskSpawnMeta {
                    session_id: Some(sid.as_str()),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                },
                "t-wake".into(),
                "wake job".into(),
                async { crate::storage::TaskOutcome::Completed("wake output".to_string()) },
            )
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while runner.pending_notification_count(Some(&sid)) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the task never queued its notification"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": "hello" }
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            server.handle_message(&prompt.to_string()),
        )
        .await
        .expect("the prompt must settle")
        .unwrap();
        assert!(resp.error.is_none(), "unexpected error: {resp:?}");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while requests.lock().unwrap().len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the notification turn never reached the model"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let bodies = requests.lock().unwrap();
        assert!(
            bodies[1].contains("Background task t-wake (completed) finished: wake job"),
            "the wake turn must carry the rendered notification: {}",
            bodies[1]
        );
        assert!(
            bodies[1].contains("wake output"),
            "the wake turn must carry the output preview: {}",
            bodies[1]
        );
        drop(bodies);

        let turns = store.list_turns(&sid).unwrap();
        assert!(
            turns.iter().any(|turn| {
                turn.origin
                    .as_ref()
                    .and_then(|origin| origin.get("kind"))
                    .and_then(|kind| kind.as_str())
                    == Some("task")
            }),
            "the wake turn records the v2 task origin: {turns:?}"
        );

        mock.abort();
    }
}
