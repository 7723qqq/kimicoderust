use super::*;

/// The host executable recorded for plugin stdio servers declared with
/// `command: "node"`: the packaged Bun binary can re-execute the entry itself
/// (`<runner> __plugin_run_node <entry>`), so a Bun-only install needs no
/// system Node.js. Unset keeps spawning `node` as declared.
fn plugin_node_runner() -> Option<std::path::PathBuf> {
    std::env::var_os("KIMI_NODE_RUNNER")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

fn plugin_manager_with_env(store: Arc<SqliteSessionStore>) -> plugins::PluginManager {
    plugins::PluginManager::new(store).with_node_runner(plugin_node_runner())
}

/// Why a rich wake could not be admitted. Callers can handle a missing session
/// differently from a server without a turn engine without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeEnqueueError {
    SessionNotFound,
    EngineUnavailable,
}

/// Tool-facing wake driver backed by the standalone server's prompt queue.
/// A weak reference lets the engine retain per-session runtimes without forming
/// an HttpServer → ServerEngine → runtime → HttpServer ownership cycle.
#[derive(Clone)]
pub struct HttpServerWakeDriver {
    server: std::sync::Weak<HttpServer>,
}

impl HttpServerWakeDriver {
    #[must_use]
    pub fn new(server: Arc<HttpServer>) -> Self {
        Self {
            server: Arc::downgrade(&server),
        }
    }
}

impl crate::session::wake::TowerWakeDriver for HttpServerWakeDriver {
    fn enqueue(
        &self,
        session_id: &str,
        payload: crate::session::wake::WakePayload,
    ) -> Result<(), String> {
        let Some(server) = self.server.upgrade() else {
            return Err(format!("{:?}", WakeEnqueueError::EngineUnavailable));
        };
        server
            .enqueue_wake(session_id, payload)
            .map(|_| ())
            .map_err(|error| format!("{error:?}"))
    }

    fn drop_wakes(&self, session_id: &str, variant: &str) -> bool {
        self.server
            .upgrade()
            .is_some_and(|server| server.drop_wakes(session_id, variant))
    }

    fn set_replay_handler(
        &self,
        session_id: &str,
        variant: &str,
        handler: crate::session::wake::WakeReplayHandler,
    ) {
        if let Some(server) = self.server.upgrade() {
            server.set_wake_replay_handler(session_id, variant, handler);
        }
    }

    fn wake_replay_status(
        &self,
        session_id: &str,
        variant: &str,
    ) -> Option<crate::session::wake::WakeReplayStatus> {
        self.server
            .upgrade()
            .and_then(|server| server.wake_replay_status(session_id, variant))
    }
}

impl HttpServer {
    /// A server over its own event hub.
    pub fn new(store: Arc<SqliteSessionStore>) -> Self {
        Self::with_hub(store, Arc::new(EventHub::new()))
    }

    /// A server that shares an existing [`EventHub`], so a turn driven from
    /// elsewhere and the WebSocket fan-out see one numbering per session.
    /// Unauthenticated. Fine for tests and for a loopback development run; the
    /// only product entry (`--serve`) replaces this with a real token, and
    /// [`http::serve`] refuses a non-loopback bind while it is in effect.
    pub fn with_hub(store: Arc<SqliteSessionStore>, hub: Arc<EventHub>) -> Self {
        let task_runner = Arc::new(TaskRunner::new(None));
        // Background task lifecycle fans out to the task's session lane (or
        // `global` for tasks without a session), both vocabularies.
        let task_event_hub = hub.clone();
        task_runner.set_event_sink(Arc::new(move |session, event| {
            let lane = session.unwrap_or("global").to_string();
            task_event_hub
                .bus_for(&lane)
                .publish(&crate::events::EngineEvent::Custom(event));
        }));
        // #3717 late-settle silence: server-side tasks settle while their
        // session may already be gone (a closed web client left a run
        // behind). The session row is the liveness source — the same check
        // the prompt route's 404 gate uses — so an archived/deleted
        // session's late settle neither announces nor queues anything.
        // `with_task_runner` installs the same predicate on replacements.
        Self::install_task_liveness(&store, &task_runner);
        let store_persister = store.clone();
        hub.set_persister(Arc::new(
            move |seq_ev: &crate::server::hub::SequencedEvent| {
                let now = chrono::Utc::now().timestamp_millis();
                let event_type = seq_ev.event.event_type().to_string();
                let is_checkpoint = event_type == "turn.ended" || event_type == "checkpoint";
                let is_compaction = event_type == "context.compaction";
                let payload_json =
                    serde_json::to_string(&seq_ev.event).unwrap_or_else(|_| "null".to_string());
                if let Err(err) = store_persister.append_wire_event_json(
                    &format!("wevt-{}", fastrand::u64(..)),
                    &seq_ev.session_id,
                    &event_type,
                    &payload_json,
                    is_checkpoint,
                    is_compaction,
                    now,
                ) {
                    tracing::warn!(
                        session_id = %seq_ev.session_id,
                        event_type = %event_type,
                        error = %err,
                        "Failed to persist wire event to sqlite store"
                    );
                }
            },
        ));

        let server = Self {
            store: store.clone(),
            hub: hub.clone(),
            engine: None,
            auth: ServerAuth::disabled(),
            heartbeat: crate::server::ws_protocol::DEFAULT_HEARTBEAT,
            // Reload the persisted server-scoped schedules here, in the one
            // place every entry point constructs a server through — a caller
            // that forgets a separate "reload" step must not silently drop
            // every REST-created schedule on restart.
            cron_scheduler: Arc::new(Mutex::new(CronScheduler::new(
                Self::read_persisted_cron_entries(&store),
                chrono::Local::now().offset().local_minus_utc(),
            ))),
            task_runner: task_runner.clone(),
            task_wake_started: false,
            server_id: format!("srv-{}", fastrand::u64(..)),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            web_assets_dir: None,
            mcp_manager: Arc::new(crate::mcp::manager::McpManager::new()),
            mcp_oauth: Arc::new(tokio::sync::Mutex::new(None)),
            mcp_auth_code: Arc::new(crate::mcp::oauth::auth_code::AuthCodeRegistry::default()),
            interaction_manager: Arc::new(
                interaction::InteractionManager::new().with_hub(hub.clone()),
            ),
            plugin_manager: Arc::new(plugin_manager_with_env(store)),
            oauth_manager: Arc::new(oauth::OAuthManager::new()),
            prompt_queue: Arc::new(prompt_queue::PromptQueue::new()),
            config_override: Arc::new(Mutex::new(None)),
            models_dev_cache: Arc::new(models_dev::CatalogCache::new()),
            config_write_path: std::sync::Mutex::new(None),
            file_store: files::FileStore::new(),
            terminal_manager: Arc::new(terminal::TerminalManager::new(hub.clone())),
            subagent_manager: Arc::new(
                crate::subagent::SubagentManager::new().with_task_runner(task_runner),
            ),
            remote_control_state: Arc::new(Mutex::new(RemoteControlStatusWire::off())),
            remote_control_runtime: Arc::new(tokio::sync::Mutex::new(None)),
            remote_control_devices: Arc::new(remote_control::RemoteControlDeviceClient::new()),
            remote_control_logout_gate: Arc::new(remote_control::RemoteControlLogoutGate::new()),
            shutdown: tokio_util::sync::CancellationToken::new(),
            loopback_bind: true,
            debug_endpoints: false,
            allow_remote_shutdown: false,
            host_guard: None,
            telemetry_sink: None,
        };
        server.install_remote_control_logout_hook();
        server
    }

    /// Record the address the listener is bound to. It decides whether the
    /// loopback-only routes (`/api/v1/debug/*`, `POST /api/v1/shutdown`) are
    /// mounted at all.
    #[must_use]
    pub fn with_bind_host(mut self, host: &str) -> Self {
        self.loopback_bind = is_loopback_host(host);
        self
    }

    /// Mount the `/api/v1/debug/*` reflection surface. Still gated on a
    /// loopback bind, so this alone does not expose it on a network bind.
    #[must_use]
    pub fn with_debug_endpoints(mut self, enabled: bool) -> Self {
        self.debug_endpoints = enabled;
        self
    }

    /// Install the server-side telemetry sink (v2 #3897). `event` is the
    /// telemetry name (`swarm_mode_entered`, `remote_control_toggle`, …),
    /// `payload` the properties v2's `track2` call carries.
    #[must_use]
    pub fn with_telemetry_sink(
        mut self,
        sink: crate::tools::external_hooks::TelemetrySink,
    ) -> Self {
        self.telemetry_sink = Some(sink);
        self
    }

    /// Emit one server-side usage event through the sink when one is
    /// installed; otherwise log it. v2 #3897 injects an `ITelemetryService`
    /// into its route hosts; the standalone server has no upstream telemetry
    /// service to report into, so the log line is the always-observable
    /// fallback and `with_telemetry_sink` is where a host integration plugs
    /// in. Fire-and-forget: telemetry must never block or fail a route.
    pub fn emit_session_telemetry(&self, event: &str, payload: serde_json::Value) {
        match &self.telemetry_sink {
            Some(sink) => sink(event, payload),
            None => tracing::info!(event, ?payload, "server usage telemetry"),
        }
    }

    /// Emit one property-less usage event.
    pub fn emit_session_telemetry_named(&self, event: &str) {
        self.emit_session_telemetry(event, serde_json::Value::Null);
    }

    /// Keep `POST /api/v1/shutdown` registered on a non-loopback bind.
    #[must_use]
    pub fn with_allow_remote_shutdown(mut self, enabled: bool) -> Self {
        self.allow_remote_shutdown = enabled;
        self
    }

    /// Install the DNS-rebinding guard. `--serve` always does; an in-process
    /// server leaves it off, since it has no bind address to compare against.
    #[must_use]
    pub fn with_host_guard(mut self, guard: HostGuard) -> Self {
        self.host_guard = Some(guard);
        self
    }

    /// Whether a request's `Host` passes the DNS-rebinding guard. Always true
    /// when no guard is installed.
    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        self.host_guard
            .as_ref()
            .is_none_or(|guard| guard.allows(host))
    }

    /// `/api/v1/debug/*` is a test-introspection surface: mounted only when
    /// asked for *and* only where the caller is already on this machine.
    pub(super) fn debug_endpoints_enabled(&self) -> bool {
        self.debug_endpoints && self.loopback_bind
    }

    /// `POST /api/v1/shutdown` stops the accept loop, so a non-loopback bind
    /// registers it only behind an explicit opt-in.
    pub(super) fn shutdown_enabled(&self) -> bool {
        self.loopback_bind || self.allow_remote_shutdown
    }

    /// Signal the accept loop (and `--serve`) to stop. In-flight connections
    /// finish on their own; only new accepts stop.
    pub fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    /// A token that resolves when the server has been asked to shut down.
    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown.clone()
    }

    pub fn subagent_manager(&self) -> Arc<crate::subagent::SubagentManager> {
        if let Some(engine) = &self.engine {
            engine.subagent_manager()
        } else {
            self.subagent_manager.clone()
        }
    }

    /// Point the provider write routes at an explicit `config.toml`; unset
    /// keeps discovery (the `--serve` path passes the file it loaded).
    #[must_use]
    pub fn with_config_write_path(mut self, path: PathBuf) -> Self {
        self.config_write_path = std::sync::Mutex::new(Some(path));
        self
    }

    /// Seed the server's config view with the file it loaded, so reads and
    /// provider writes agree with the engine (the `--serve` path).
    #[must_use]
    pub fn with_config(mut self, config: crate::config::KimiConfig) -> Self {
        // `[background]` knobs apply to the daemon's own task runner too, so
        // the same file behaves the same on every entry point.
        self.task_runner.apply_background_limits(
            config.background.kill_grace_period_ms,
            config.resolve_background_max_running_tasks(),
        );
        let warnings = config.config_warnings.clone();
        self.config_override = Arc::new(Mutex::new(Some(config)));
        // Seeded from the file the server was started with, before any
        // subscriber exists: the global lane's replay ring hands the entity to
        // whoever attaches first, which is the only ordering available here —
        // config load happens before there is a hub to broadcast on.
        self.publish_startup_config_warnings(&warnings);
        self
    }

    /// Use a specific OAuth manager (tests point it at a mock credential
    /// store and hosts).
    #[must_use]
    pub fn with_oauth_manager(mut self, manager: Arc<oauth::OAuthManager>) -> Self {
        self.oauth_manager = manager;
        self.install_remote_control_logout_hook();
        self
    }

    /// Wire the managed Kimi logout to the remote-control cleanup. The hook
    /// wraps the credential wipe with the relay session-cache clears and the
    /// delayed tunnel teardown (upstream `registerOAuthRoutes` logout branch).
    fn install_remote_control_logout_hook(&self) {
        self.oauth_manager.set_remote_control_logout_hook(Arc::new(
            remote_control::RemoteControlLogoutCleanup::new(
                self.remote_control_runtime.clone(),
                self.remote_control_state.clone(),
                self.remote_control_devices.clone(),
                self.remote_control_logout_gate.clone(),
            ),
        ));
    }

    /// Use a specific upload store (tests point it at a temp directory).
    #[must_use]
    pub fn with_file_store(mut self, store: files::FileStore) -> Self {
        self.file_store = store;
        self
    }

    pub(super) fn config_write_path(&self) -> Option<PathBuf> {
        self.config_write_path
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Whether a provider carries a usable cached credential: its own token,
    /// or any managed-token alias when it is the managed provider.
    pub(super) fn has_cached_token(&self, provider: &str) -> bool {
        self.oauth_manager.has_cached_token(provider)
            || (provider == crate::server::model_catalog::MANAGED_PROVIDER_NAME
                && self.oauth_manager.has_managed_token())
    }

    #[must_use]
    pub fn with_subagent_manager(mut self, manager: Arc<crate::subagent::SubagentManager>) -> Self {
        self.subagent_manager = manager;
        self
    }

    pub fn terminal_manager(&self) -> Arc<terminal::TerminalManager> {
        self.terminal_manager.clone()
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn started_at(&self) -> &str {
        &self.started_at
    }

    pub fn plugin_manager(&self) -> Arc<plugins::PluginManager> {
        self.plugin_manager.clone()
    }

    pub fn oauth_manager(&self) -> Arc<oauth::OAuthManager> {
        self.oauth_manager.clone()
    }

    #[must_use]
    pub fn with_interaction_manager(
        mut self,
        manager: Arc<interaction::InteractionManager>,
    ) -> Self {
        self.interaction_manager = manager.clone();
        if let Some(engine) = &self.engine {
            engine.set_interaction_manager(manager);
        }
        self
    }

    pub fn interaction_manager(&self) -> Arc<interaction::InteractionManager> {
        self.interaction_manager.clone()
    }

    #[must_use]
    pub fn with_web_assets(mut self, path: impl Into<PathBuf>) -> Self {
        self.web_assets_dir = Some(path.into());
        self
    }

    pub fn web_assets_dir(&self) -> Option<&Path> {
        self.web_assets_dir.as_deref()
    }

    #[must_use]
    pub fn with_mcp_manager(mut self, manager: Arc<crate::mcp::manager::McpManager>) -> Self {
        self.mcp_manager = manager.clone();
        if let Some(engine) = &self.engine {
            engine.set_mcp_manager(manager);
        }
        self
    }

    /// Point the plugin registry at the Kimi home, so a remote plugin has
    /// somewhere to be installed (`<home>/plugins/<id>`).
    #[must_use]
    pub fn with_plugin_home(mut self, home: PathBuf) -> Self {
        self.plugin_manager = Arc::new(
            plugin_manager_with_env(self.store.clone())
                .with_marketplace_dir(plugins::default_marketplace_dir())
                .with_home_dir(Some(home)),
        );
        self
    }

    /// Install the MCP OAuth credential service; it also backs the manager's
    /// bearer-token injection.
    pub async fn with_mcp_oauth_service(
        self,
        service: Arc<crate::mcp::oauth::McpOAuthService>,
    ) -> Self {
        self.mcp_manager.set_oauth_service(service.clone());
        *self.mcp_oauth.lock().await = Some(service);
        self
    }

    pub fn mcp_manager(&self) -> Arc<crate::mcp::manager::McpManager> {
        self.mcp_manager.clone()
    }

    /// Require `Authorization: Bearer <token>` (and the WebSocket subprotocol
    /// equivalent) on every non-bypassed route.
    #[must_use]
    pub fn with_auth(mut self, auth: ServerAuth) -> Self {
        self.auth = auth;
        self
    }

    pub fn auth(&self) -> &ServerAuth {
        &self.auth
    }

    /// Set the period of the JSON `ping` heartbeat each WebSocket connection
    /// sends. kap-server takes the same as `heartbeatIntervalMs`.
    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    pub fn heartbeat(&self) -> Duration {
        self.heartbeat
    }

    /// Attach the turn driver, without which `POST /sessions/:id/prompt` has
    /// nothing to run a turn with and answers 503.
    pub fn with_engine(mut self, engine: ServerEngine) -> Self {
        if engine.mcp_manager().is_none() {
            engine.set_mcp_manager(self.mcp_manager.clone());
        }
        if engine.interaction_manager().is_none() {
            engine.set_interaction_manager(self.interaction_manager.clone());
        }
        // Let the engine's OAuth-bound providers fetch tokens from the same
        // store the login/usage routes drive.
        engine.set_oauth_manager(self.oauth_manager.clone());
        engine.set_config_source(self.config_override.clone());
        // The engine's `Skill` tool and the prompt's skills section must see the
        // same plugin catalog the REST skill lists do, so the plugin roots are
        // read from this server's manager on every session spec. Read lazily
        // rather than cached: an install / enable / disable / remove is then
        // reflected in the next turn with no refresh bookkeeping to miss.
        let plugin_manager = self.plugin_manager.clone();
        engine.with_plugin_skill_roots(Arc::new(move || plugin_manager.plugin_skill_dirs()));
        let contributions_manager = self.plugin_manager.clone();
        engine.with_plugin_contributions(Arc::new(move || {
            crate::server::engine::PluginContributionSet {
                sections: contributions_manager.plugin_system_prompt_block(),
                hooks: contributions_manager.enabled_hooks(),
            }
        }));
        self.subagent_manager = engine.subagent_manager();
        self.subagent_manager
            .set_task_runner_sync(self.task_runner.clone());
        self.engine = Some(Arc::new(engine));
        self.spawn_task_wake_service();
        self
    }

    /// Start the background-task wake service (v2 `taskService.notifyAgentTask`
    /// → `loop.notify`): when a task settles, its session gets a notification
    /// turn even if no prompt is waiting. The turn is admitted through the
    /// prompt queue, so it serializes with user prompts exactly like any other
    /// turn.
    ///
    /// Started from [`Self::with_engine`] — the turn driver is what the wake
    /// queues into. A caller outside a runtime simply gets no service: there
    /// is no turn driver to wake either.
    fn spawn_task_wake_service(&mut self) {
        if self.task_wake_started {
            return;
        }
        let Some(engine) = self.engine.clone() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.task_wake_started = true;
        let store = self.store_arc();
        let queue = self.prompt_queue.clone();
        let hub = self.hub.clone();
        let runner = self.task_runner.clone();
        handle.spawn(async move {
            let mut bell = runner.notification_bell();
            loop {
                // Closed only when the runner itself is dropped (server gone).
                if bell.changed().await.is_err() {
                    return;
                }
                for session_id in runner.sessions_with_pending_notifications() {
                    let Some((text, origin)) =
                        crate::server::prompt_queue::take_task_wake_prompt(&runner, &session_id)
                    else {
                        continue;
                    };
                    let Some(run) = queue.admit_wake(&session_id, text, origin) else {
                        // A turn is already active; the running driver promotes
                        // this entry when it advances.
                        continue;
                    };
                    tokio::spawn(crate::server::prompt_queue::run_prompt_loop(
                        Some(engine.clone()),
                        store.clone(),
                        queue.clone(),
                        hub.clone(),
                        session_id,
                        run,
                    ));
                }
            }
        });
    }

    pub fn engine(&self) -> Option<Arc<ServerEngine>> {
        self.engine.clone()
    }

    #[must_use]
    pub fn with_cron_scheduler(mut self, scheduler: Arc<Mutex<CronScheduler>>) -> Self {
        self.cron_scheduler = scheduler;
        self
    }

    pub fn cron_scheduler(&self) -> Arc<Mutex<CronScheduler>> {
        self.cron_scheduler.clone()
    }

    /// Server-scoped cron persistence: entries live under state key
    /// `("cron", "entries")` in the session store, so REST-created schedules
    /// survive a restart and are reloaded by `run_serve`. (The model-facing
    /// Cron* tools use the workspace file state store's `cron` domain instead —
    /// a separate surface, unchanged here.)
    pub fn load_cron_entries(&self) -> Vec<CronEntry> {
        Self::read_persisted_cron_entries(&self.store)
    }

    pub(super) fn read_persisted_cron_entries(store: &SqliteSessionStore) -> Vec<CronEntry> {
        store
            .get_state("cron", "entries")
            .unwrap_or(None)
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default()
    }

    pub fn persist_cron_entries(&self, entries: &[CronEntry]) {
        if let Ok(value) = serde_json::to_value(entries)
            && let Err(e) = self.store.put_state("cron", "entries", &value)
        {
            tracing::warn!(error = %e, "persisting cron entries failed");
        }
    }

    #[must_use]
    pub fn with_task_runner(mut self, task_runner: Arc<TaskRunner>) -> Self {
        Self::install_task_liveness(&self.store, &task_runner);
        self.subagent_manager
            .set_task_runner_sync(task_runner.clone());
        if let Some(engine) = &self.engine {
            engine
                .subagent_manager()
                .set_task_runner_sync(task_runner.clone());
        }
        self.task_runner = task_runner;
        self
    }

    /// The #3717 liveness predicate on a server task runner: a session row's
    /// existence is "alive" (the same check the prompt route's 404 gate
    /// uses), so a deleted/archived session's late task settles stay silent.
    /// Idempotent — it only overwrites the predicate field on the runner.
    pub(super) fn install_task_liveness(store: &Arc<SqliteSessionStore>, runner: &Arc<TaskRunner>) {
        let liveness_store = store.clone();
        runner.set_liveness_check(Arc::new(move |session: Option<&str>| match session {
            None => true,
            Some(session_id) => liveness_store
                .get_session(session_id)
                .map(|found| found.is_some())
                .unwrap_or(false),
        }));
    }

    pub fn task_runner(&self) -> Arc<TaskRunner> {
        self.task_runner.clone()
    }

    /// The session store, for a host that wants to read transcripts or share
    /// the same store with the engine it builds.
    pub fn store(&self) -> &SqliteSessionStore {
        &self.store
    }

    pub fn store_arc(&self) -> Arc<SqliteSessionStore> {
        self.store.clone()
    }

    /// Enqueue one prompt for a session: validate, resolve attachments, admit
    /// into the queue (`running` when it starts a turn, `queued` behind an
    /// active one), publish `prompt.submitted`, and spawn the turn loop.
    ///
    /// Shared by `POST /sessions/{id}/prompts` (the protocol route) and
    /// `POST /prompts` (the collection route). The collection route used to
    /// answer `{"status":"enqueued"}` after minting an id without touching the
    /// queue, so a client got a receipt for a prompt that never ran.
    pub(super) async fn submit_session_prompt(
        &self,
        session_id: &str,
        body: Value,
    ) -> HttpResponse {
        let content = body
            .get("content")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let (mut prompt, blocks) = match prompt_content_to_blocks(&content, &self.file_store) {
            Ok(parsed) => parsed,
            Err(error) => return HttpResponse::bad_request(error),
        };
        if prompt.is_empty() {
            prompt = body
                .get("prompt")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
        }
        if prompt.is_empty() && blocks.is_empty() {
            return HttpResponse::bad_request("prompt content is empty");
        }
        if self.store.get_session(session_id).ok().flatten().is_none() {
            return HttpResponse::not_found();
        }
        if let Err(error) = apply_prompt_submission_options(self, session_id, &body) {
            return HttpResponse::bad_request(error);
        }
        let Some(engine) = self.engine.clone() else {
            return HttpResponse::json(
                503,
                &json!({ "error": "no engine configured for this server" }),
            );
        };
        let prompt_id = body
            .get("prompt_id")
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("prompt-{}", fastrand::u64(..)));
        if self.prompt_queue.contains(session_id, &prompt_id) {
            return HttpResponse::json(
                409,
                &json!({
                    "code": crate::server::envelope::error_codes::PROMPT_ID_CONFLICT,
                    "msg": "prompt_id already exists",
                }),
            );
        }
        let wire_content = if content.is_empty() {
            json!([{ "type": "text", "text": prompt }])
        } else {
            Value::Array(content)
        };
        // The item carries no status; `admit` stamps it (`running` when it
        // starts a turn, `queued` behind the active prompt).
        // Client metadata rides the request as an opaque object; the route
        // wraps it in the one-element array v2's `clientMetadata` is, echoes it
        // on the prompt item, and persists it as the turn's origin payload
        // (#3764). It never reaches the model content.
        let client_metadata = match body.get("metadata") {
            None | Some(Value::Null) => None,
            Some(value) if !value.is_object() => {
                return HttpResponse::bad_request("Field 'metadata' must be an object");
            }
            Some(value) => Some(json!([value])),
        };
        let origin = prompt_origin_from_metadata(client_metadata.as_ref());
        let mut item = json!({
            "prompt_id": prompt_id.clone(),
            "user_message_id": format!("msg-{prompt_id}"),
            "content": wire_content,
            "created_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Some(metadata) = &client_metadata {
            item["metadata"] = metadata[0].clone();
        }
        let (item, run) = self
            .prompt_queue
            .admit(session_id, item, prompt, blocks, origin.clone());
        if let Some(turn_number) = self.prompt_queue.interrupted_wake_turn(session_id) {
            engine.cancel_turn_at(session_id, turn_number);
        }
        crate::server::prompt_queue::publish_prompt_event(
            &self.hub,
            session_id,
            json!({
                "type": "prompt.submitted",
                "promptId": item.get("prompt_id").cloned().unwrap_or(Value::Null),
                "userMessageId": item
                    .get("user_message_id")
                    .cloned()
                    .unwrap_or(Value::Null),
                "status": item.get("status").cloned().unwrap_or(Value::Null),
                "content": item.get("content").cloned().unwrap_or_else(|| json!([])),
                "createdAt": item.get("created_at").cloned().unwrap_or(Value::Null),
                "metadata": item.get("metadata").cloned().unwrap_or(Value::Null),
            }),
        );
        if let Some(run) = run {
            let queue = self.prompt_queue.clone();
            let store = self.store_arc();
            let hub = self.hub.clone();
            let session_id = session_id.to_string();
            tokio::spawn(async move {
                crate::server::prompt_queue::run_prompt_loop(
                    Some(engine),
                    store,
                    queue,
                    hub,
                    session_id,
                    run,
                )
                .await;
            });
        }
        HttpResponse::ok(&item)
    }

    /// The resolved default-workspace root for the `is_default` wire flag
    /// (upstream #4224). Sync on purpose: `format_wire_session` is sync and
    /// nine call sites reach it from async contexts that should not each
    /// re-discover config. Uses the override when one is set, else discovers
    /// once and caches for the process lifetime — the default workspace root
    /// is not something a running server needs to re-resolve per request.
    pub fn default_workspace_root(&self) -> Option<std::path::PathBuf> {
        static CACHED: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
        // `try_lock`, never `blocking_lock`: every call site runs inside the
        // async runtime, and blocking it there panics ("Cannot block the
        // current thread from within a runtime" — measured). The override
        // lock is held only for a clone, so a contended try simply falls
        // through to the cached discovery, which resolves the same root.
        if let Ok(Some(config)) = self.config_override.try_lock().map(|guard| guard.clone()) {
            return crate::default_workspace::resolve_root(
                &config,
                &crate::default_workspace::user_home_dir()?,
            )
            .ok();
        }
        CACHED
            .get_or_init(|| {
                crate::config::KimiConfig::discover()
                    .ok()
                    .and_then(|(config, _)| {
                        crate::default_workspace::resolve_root(
                            &config,
                            &crate::default_workspace::user_home_dir()?,
                        )
                        .ok()
                    })
            })
            .clone()
    }

    pub async fn config(&self) -> crate::config::KimiConfig {
        self.config_override
            .lock()
            .await
            .clone()
            .unwrap_or_else(|| {
                crate::config::KimiConfig::discover()
                    .map(|(c, _)| c)
                    .unwrap_or_default()
            })
    }

    /// Publish `event.config.changed` after a config mutation the route layer
    /// applied (provider CRUD / refresh): the Web client folds the payload's
    /// config instead of re-fetching. Best-effort —the mutation already
    /// landed, so this only costs the push when the payload build fails.
    /// Refresh provider models and publish the config/catalog change events.
    /// Shared by the manual `providers:refresh*` routes and the
    /// `[model_catalog]` auto-refresh (`refresh_on_start` / interval).
    pub async fn refresh_models(&self, scope: &str, provider_id: Option<&str>) -> Value {
        let result = crate::server::provider_refresh::refresh(
            &self.config_override,
            self.config_write_path().as_deref(),
            &self.oauth_manager,
            scope,
            provider_id,
        )
        .await;
        self.publish_config_changed(&["providers", "models"]).await;
        self.publish_model_catalog_changed(&result);
        result
    }

    pub(super) async fn publish_config_changed(&self, changed_fields: &[&str]) {
        let config = self.config().await;
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::ConfigChanged {
                changed_fields: changed_fields.iter().map(|s| (*s).to_string()).collect(),
                config: format_config_response(&config),
            });
    }

    /// Publish `event.model_catalog.changed` after a provider refresh: the Web
    /// client refreshes its provider/model caches from the per-provider diff
    /// (`{changed, unchanged, failed}` —the refresh result's own shape).
    pub(super) fn publish_model_catalog_changed(&self, result: &Value) {
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "event.model_catalog.changed",
                "changed": result.get("changed").cloned().unwrap_or_else(|| json!([])),
                "unchanged": result.get("unchanged").cloned().unwrap_or_else(|| json!([])),
                "failed": result.get("failed").cloned().unwrap_or_else(|| json!([])),
            })));
    }

    /// Publish `event.plugin.changed` after a plugin mutation (install /
    /// remove / enable / disable): the payload is a bare bump — v3 clients
    /// re-fetch the plugin list, the same contract as upstream's
    /// `PluginMessage` global entity.
    pub(crate) fn publish_plugin_changed(&self) {
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "event.plugin.changed",
            })));
    }

    /// Publish `event.config.warning` on the global lane for the `[models]`
    /// entries the loaded `config.toml` could not resolve (v2 #3681).
    ///
    /// The event carries `warnings[].message` on the global lane; the v1
    /// broadcaster passes it through as-is. `domain` is left off — the fork's
    /// only warning source is the config file itself, and upstream treats the
    /// field as optional.
    ///
    /// Always publishes, `warnings: []` included, because an empty list means
    /// "no warnings now" and is what clears a client's stale advisory after
    /// the file is fixed. Upstream's `publishConfigWarnings`
    /// (`kap-server/src/start.ts`) does the same — it is wired to the
    /// diagnostics-change event with no empty check, and only the startup
    /// call filters.
    pub(super) fn publish_config_warnings(&self, warnings: &[String]) {
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "event.config.warning",
                "warnings": warnings
                    .iter()
                    .map(|message| json!({ "message": message }))
                    .collect::<Vec<Value>>(),
            })));
    }

    /// The startup call: publish the loaded file's warnings, if it has any.
    ///
    /// A clean load stays silent rather than publishing `[]`. Upstream guards
    /// the same way at startup (`start.ts`: the `ready` handler publishes only
    /// when some diagnostic is a warning), because a fresh daemon has no stale
    /// advisory to clear — and the global lane's replay ring would otherwise
    /// hand every future attacher an empty entity that says nothing.
    pub(super) fn publish_startup_config_warnings(&self, warnings: &[String]) {
        if warnings.is_empty() {
            return;
        }
        self.publish_config_warnings(warnings);
    }

    /// The fan-out handle connections attach to and turns publish through.
    pub fn hub(&self) -> Arc<EventHub> {
        self.hub.clone()
    }

    /// Enqueue a caller-rendered rich wake and drive it when the session is
    /// idle. The returned id can be passed to [`Self::drop_wake`]. A later user
    /// prompt interrupts and automatically replays the wake after its queue.
    pub fn enqueue_wake(
        &self,
        session_id: &str,
        payload: crate::session::wake::WakePayload,
    ) -> Result<String, WakeEnqueueError> {
        if self.store.get_session(session_id).ok().flatten().is_none() {
            return Err(WakeEnqueueError::SessionNotFound);
        }
        let engine = self
            .engine
            .clone()
            .ok_or(WakeEnqueueError::EngineUnavailable)?;
        let (wake_id, run) = self
            .prompt_queue
            .admit_rich_wake_with_id(session_id, payload);
        if let Some(run) = run {
            let queue = self.prompt_queue.clone();
            let store = self.store_arc();
            let hub = self.hub.clone();
            let session_id = session_id.to_string();
            tokio::spawn(crate::server::prompt_queue::run_prompt_loop(
                Some(engine),
                store,
                queue,
                hub,
                session_id,
                run,
            ));
        }
        Ok(wake_id)
    }

    /// Explicitly discard a rich wake. This is the teardown/drop operation, not
    /// the automatic user-prompt interruption path, so no replay is scheduled.
    pub fn drop_wake(&self, session_id: &str, wake_id: &str) -> bool {
        let Some(engine) = self.engine.clone() else {
            return false;
        };
        let turn_number = self.prompt_queue.active_turn_number(session_id);
        match self.prompt_queue.cancel(session_id, wake_id) {
            Some(crate::server::prompt_queue::CancelOutcome::Active) => {
                if let Some(turn_number) = turn_number {
                    engine.cancel_turn_at(session_id, turn_number);
                }
                true
            }
            Some(crate::server::prompt_queue::CancelOutcome::Queued) => true,
            None => false,
        }
    }

    /// Discard a producer's whole wake channel, including a replay whose turn
    /// id changed after user interruption.
    pub fn drop_wakes(&self, session_id: &str, variant: &str) -> bool {
        let (found, active_turn) = self.prompt_queue.drop_rich_wakes(session_id, variant);
        if let (Some(engine), Some(turn_number)) = (self.engine.clone(), active_turn) {
            engine.cancel_turn_at(session_id, turn_number);
        }
        found
    }

    /// Install this standalone server's wake driver into the turn engine. The
    /// engine owns per-session runtimes; `Arc<HttpServer>` stays only at the
    /// composition root, so no server/engine reference cycle is introduced.
    pub fn install_tower_wake_driver(self: &Arc<Self>) {
        if let Some(engine) = self.engine.clone() {
            engine.install_tower_wake_driver(Arc::new(HttpServerWakeDriver::new(self.clone())));
        }
    }

    /// Delegate replay of one session + variant to an async owner. Unregistered
    /// variants retain the queue's original direct replay behavior.
    pub fn set_wake_replay_handler(
        &self,
        session_id: &str,
        variant: &str,
        handler: crate::session::wake::WakeReplayHandler,
    ) {
        self.prompt_queue
            .set_wake_replay_handler(session_id, variant, handler);
    }

    /// Read the retained payload/error state for a delegated server replay.
    pub fn wake_replay_status(
        &self,
        session_id: &str,
        variant: &str,
    ) -> Option<crate::session::wake::WakeReplayStatus> {
        self.prompt_queue.wake_replay_status(session_id, variant)
    }

    /// Admit a prompt and, when it becomes active, drive its turn in the
    /// background. Used by the steer path to run prompts that could not attach
    /// to a running turn instead of dropping them.
    pub(super) fn run_or_queue_prompt(
        &self,
        engine: &Arc<ServerEngine>,
        session_id: &str,
        item: Value,
        prompt: String,
        blocks: Vec<crate::rpc::types::ContentBlock>,
        origin: Option<Value>,
    ) {
        let (_item, run) = self
            .prompt_queue
            .admit(session_id, item, prompt, blocks, origin);
        if let Some(turn_number) = self.prompt_queue.interrupted_wake_turn(session_id) {
            engine.cancel_turn_at(session_id, turn_number);
        }
        let Some(run) = run else {
            return;
        };
        let queue = self.prompt_queue.clone();
        let store = self.store_arc();
        let hub = self.hub.clone();
        let session_id = session_id.to_string();
        let engine = engine.clone();
        tokio::spawn(async move {
            crate::server::prompt_queue::run_prompt_loop(
                Some(engine),
                store,
                queue,
                hub,
                session_id,
                run,
            )
            .await;
        });
    }

    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let store = Arc::new(SqliteSessionStore::in_memory()?);
        Ok(Self::new(store))
    }
}
