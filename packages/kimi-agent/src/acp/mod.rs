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
mod tests;
