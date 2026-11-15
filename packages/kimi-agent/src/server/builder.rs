use super::*;

fn plugin_node_runner() -> Option<std::path::PathBuf> {
    std::env::var_os("KIMI_NODE_RUNNER")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

fn plugin_manager_with_env(store: Arc<SqliteSessionStore>) -> plugins::PluginManager {
    plugins::PluginManager::new(store).with_node_runner(plugin_node_runner())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeEnqueueError {
    SessionNotFound,
    EngineUnavailable,
}

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
    pub fn new(store: Arc<SqliteSessionStore>) -> Self {
        Self::with_hub(store, Arc::new(EventHub::new()))
    }

    pub fn with_hub(store: Arc<SqliteSessionStore>, hub: Arc<EventHub>) -> Self {
        let cleanup_store = store.clone();
        let task_runner = Arc::new(TaskRunner::new(None));

        let task_event_hub = hub.clone();
        task_runner.set_event_sink(Arc::new(move |session, event| {
            let lane = session.unwrap_or("global").to_string();
            task_event_hub
                .bus_for(&lane)
                .publish(&crate::events::EngineEvent::Custom(event));
        }));

        Self::install_task_liveness(&store, &task_runner);
        let store_persister = store.clone();
        let wire_journal = store
            .path()
            .and_then(|path| path.parent())
            .filter(|home| !home.as_os_str().is_empty())
            .map(|home| {
                Arc::new(crate::session::wire_journal::WireJournal::new(
                    home,
                    store.clone(),
                ))
            });
        let persisted_heads = hub.persisted_heads();
        let wire_journal_handle = wire_journal.clone();
        hub.set_persister(Arc::new(
            move |seq_ev: &crate::server::hub::SequencedEvent| {
                let now = chrono::Utc::now().timestamp_millis();
                let event_type = seq_ev.event.event_type().to_string();
                let is_checkpoint = event_type == "turn.ended"
                    || event_type == "checkpoint"
                    || event_type == "context.apply_compaction";

                let is_compaction = event_type == "context.compaction"
                    || event_type == "compaction.completed"
                    || event_type == "context.apply_compaction";
                let payload_json =
                    serde_json::to_string(&seq_ev.event).unwrap_or_else(|_| "null".to_string());
                match store_persister.append_wire_event_json(
                    &format!("wevt-{}", fastrand::u64(..)),
                    &seq_ev.session_id,
                    &event_type,
                    &payload_json,
                    is_checkpoint,
                    is_compaction,
                    now,
                ) {
                    Ok(seq) => {
                        persisted_heads.note(&seq_ev.session_id, seq);
                        if let Some(journal) = &wire_journal {
                            journal.append(&seq_ev.session_id, &seq_ev.event, now);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            session_id = %seq_ev.session_id,
                            event_type = %event_type,
                            error = %err,
                            "Failed to persist wire event to sqlite store"
                        );
                    }
                }
            },
        ));

        let server = Self {
            store: store.clone(),
            hub: hub.clone(),
            wire_journal: wire_journal_handle,
            engine: None,
            auth: ServerAuth::disabled(),
            heartbeat: crate::server::ws_protocol::DEFAULT_HEARTBEAT,

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
            model_refresh_chain: tokio::sync::Mutex::new(()),
            hooks_source: crate::tools::external_hooks::HooksSource::default(),
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
            reclaimed: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            resume_slots: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            session_cleanup_hooks: Arc::new(std::sync::Mutex::new(Vec::new())),
            idle_reaper: std::sync::OnceLock::new(),
        };
        server.install_remote_control_logout_hook();

        {
            let store = cleanup_store;
            server.register_session_cleanup_hook(Box::new(move |session_id| {
                crate::server::idle_sessions::remove_session_state_dir(&store, session_id)
            }));
        }
        server
    }

    #[must_use]
    pub fn with_bind_host(mut self, host: &str) -> Self {
        self.loopback_bind = is_loopback_host(host);
        self
    }

    #[must_use]
    pub fn with_debug_endpoints(mut self, enabled: bool) -> Self {
        self.debug_endpoints = enabled;
        self
    }

    #[must_use]
    pub fn with_telemetry_sink(
        mut self,
        sink: crate::tools::external_hooks::TelemetrySink,
    ) -> Self {
        self.telemetry_sink = Some(sink);
        self
    }

    pub fn emit_session_telemetry(&self, event: &str, payload: serde_json::Value) {
        match &self.telemetry_sink {
            Some(sink) => sink(event, payload),
            None => tracing::info!(event, ?payload, "server usage telemetry"),
        }
    }

    pub fn emit_session_telemetry_named(&self, event: &str) {
        self.emit_session_telemetry(event, serde_json::Value::Null);
    }

    #[must_use]
    pub fn with_allow_remote_shutdown(mut self, enabled: bool) -> Self {
        self.allow_remote_shutdown = enabled;
        self
    }

    #[must_use]
    pub fn with_host_guard(mut self, guard: HostGuard) -> Self {
        self.host_guard = Some(guard);
        self
    }

    pub fn host_allowed(&self, host: Option<&str>) -> bool {
        self.host_guard
            .as_ref()
            .is_none_or(|guard| guard.allows(host))
    }

    pub(super) fn debug_endpoints_enabled(&self) -> bool {
        self.debug_endpoints && self.loopback_bind
    }

    pub(super) fn shutdown_enabled(&self) -> bool {
        self.loopback_bind || self.allow_remote_shutdown
    }

    pub fn request_shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn shutdown_token(&self) -> tokio_util::sync::CancellationToken {
        self.shutdown.clone()
    }

    pub(super) fn is_shutting_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    pub fn subagent_manager(&self) -> Arc<crate::subagent::SubagentManager> {
        if let Some(engine) = &self.engine {
            engine.subagent_manager()
        } else {
            self.subagent_manager.clone()
        }
    }

    #[must_use]
    pub fn with_config_write_path(mut self, path: PathBuf) -> Self {
        self.config_write_path = std::sync::Mutex::new(Some(path));
        self
    }

    #[must_use]
    pub fn with_config(mut self, config: crate::config::KimiConfig) -> Self {
        self.task_runner.apply_background_limits(
            config.background.kill_grace_period_ms,
            config.resolve_background_max_running_tasks(),
            config.resolve_max_settled_tasks(),
            config.background.keep_alive_on_exit,
        );
        let warnings = config.config_warnings.clone();

        self.hooks_source.replace(config.hooks.clone());
        self.config_override = Arc::new(Mutex::new(Some(config)));

        self.publish_startup_config_warnings(&warnings);
        self
    }

    #[must_use]
    pub fn with_oauth_manager(mut self, manager: Arc<oauth::OAuthManager>) -> Self {
        self.oauth_manager = manager;
        self.install_remote_control_logout_hook();
        self
    }

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

    #[must_use]
    pub fn with_plugin_home(mut self, home: PathBuf) -> Self {
        self.plugin_manager = Arc::new(
            plugin_manager_with_env(self.store.clone())
                .with_marketplace_dir(plugins::default_marketplace_dir())
                .with_home_dir(Some(home)),
        );
        self
    }

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

    #[must_use]
    pub fn with_auth(mut self, auth: ServerAuth) -> Self {
        self.auth = auth;
        self
    }

    pub fn auth(&self) -> &ServerAuth {
        &self.auth
    }

    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    pub fn heartbeat(&self) -> Duration {
        self.heartbeat
    }

    pub fn with_engine(mut self, engine: ServerEngine) -> Self {
        if engine.mcp_manager().is_none() {
            engine.set_mcp_manager(self.mcp_manager.clone());
        }
        if engine.interaction_manager().is_none() {
            engine.set_interaction_manager(self.interaction_manager.clone());
        }

        engine.set_oauth_manager(self.oauth_manager.clone());
        engine.set_config_source(self.config_override.clone());

        engine.set_hooks_source(self.hooks_source.clone());

        let plugin_manager = self.plugin_manager.clone();
        engine.with_plugin_skill_roots(Arc::new(move || plugin_manager.plugin_skill_dirs()));
        let contributions_manager = self.plugin_manager.clone();
        engine.with_plugin_contributions(Arc::new(move || {
            crate::server::engine::PluginContributionSet {
                sections: contributions_manager.plugin_system_prompt_block(),
                hooks: contributions_manager.enabled_hooks(),
            }
        }));

        let (_, skipped_plugins) = self
            .plugin_manager
            .plugin_system_prompt_block_with_skipped();
        self.publish_plugin_section_warning(&skipped_plugins);
        self.subagent_manager = engine.subagent_manager();
        self.subagent_manager
            .set_task_runner_sync(self.task_runner.clone());
        self.engine = Some(Arc::new(engine));
        self.install_background_activity_sink();
        self.spawn_task_wake_service();
        self
    }

    fn install_background_activity_sink(&self) {
        let Some(engine) = self.engine.as_ref().map(Arc::downgrade) else {
            return;
        };
        self.task_runner.set_settle_sink(Arc::new(move |session| {
            if let (Some(session), Some(engine)) = (session, engine.upgrade()) {
                engine.publish_background_work_changed(session);
            }
        }));
    }

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
        self.install_background_activity_sink();
        self
    }

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

    pub fn store(&self) -> &SqliteSessionStore {
        &self.store
    }

    pub fn store_arc(&self) -> Arc<SqliteSessionStore> {
        self.store.clone()
    }

    pub(super) async fn submit_session_prompt(
        &self,
        session_id: &str,
        body: Value,
    ) -> HttpResponse {
        if self.is_shutting_down() {
            return HttpResponse::json(
                503,
                &json!({ "error": "server is shutting down; new turns are not accepted" }),
            );
        }
        let content = body
            .get("content")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();

        let provider = match &self.engine {
            Some(engine) => engine.session_provider_type(session_id).await,
            None => None,
        };
        let (mut prompt, blocks) =
            match prompt_content_to_blocks(&content, &self.file_store, provider.as_deref()) {
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
        if let Err(error) = apply_prompt_submission_options(self, session_id, &body).await {
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

        if item.get("status").and_then(Value::as_str) == Some("queued")
            && let Some(origin) = origin
                .as_ref()
                .filter(|origin| origin.get("kind").and_then(Value::as_str) == Some("user"))
        {
            let content = crate::turn_events::strip_bundled_skill_blocks(
                &item.get("content").cloned().unwrap_or_else(|| json!([])),
                origin,
            );
            crate::server::prompt_queue::publish_prompt_event(
                &self.hub,
                session_id,
                json!({
                    "type": "prompt.queued",
                    "promptId": item.get("prompt_id").cloned().unwrap_or(Value::Null),
                    "content": content,
                    "queueLength": self.prompt_queue.queued_count(session_id),
                }),
            );
        }
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

    pub fn default_workspace_root(&self) -> Option<std::path::PathBuf> {
        static CACHED: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

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

    pub(crate) async fn current_hooks(&self) -> Vec<crate::permission::HookDef> {
        match self.hooks_source.snapshot() {
            Some(hooks) => hooks,
            None => self.config().await.hooks,
        }
    }

    pub async fn refresh_models(&self, scope: &str, provider_id: Option<&str>) -> Value {
        let result = crate::server::provider_refresh::refresh(
            &self.model_refresh_chain,
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

    pub(crate) fn publish_plugin_changed(&self, kind: &str, id: &str) {
        crate::injection::plugin::publish_plugin_change_notice(kind, id);
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "event.plugin.changed",
            })));

        let (_, skipped) = self
            .plugin_manager
            .plugin_system_prompt_block_with_skipped();
        self.publish_plugin_section_warning(&skipped);
    }

    fn publish_plugin_section_warning(&self, skipped: &[String]) {
        if skipped.is_empty() {
            return;
        }
        let ids = skipped
            .iter()
            .map(|id| format!("\"{id}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let kb = crate::server::plugins::PLUGIN_SECTIONS_MAX_BYTES / 1024;
        self.hub
            .bus_for("global")
            .publish(&crate::events::EngineEvent::Custom(json!({
                "type": "warning",
                "code": "plugin-sections-oversized",
                "message": format!(
                    "Plugin system-prompt contributions from {ids} were skipped: \
                     the aggregate {kb} KB budget is exhausted."
                ),
            })));
    }

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

    pub(super) fn publish_startup_config_warnings(&self, warnings: &[String]) {
        if warnings.is_empty() {
            return;
        }
        self.publish_config_warnings(warnings);
    }

    pub fn hub(&self) -> Arc<EventHub> {
        self.hub.clone()
    }

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

    pub fn drop_wakes(&self, session_id: &str, variant: &str) -> bool {
        let (found, active_turn) = self.prompt_queue.drop_rich_wakes(session_id, variant);
        if let (Some(engine), Some(turn_number)) = (self.engine.clone(), active_turn) {
            engine.cancel_turn_at(session_id, turn_number);
        }
        found
    }

    pub fn install_tower_wake_driver(self: &Arc<Self>) {
        if let Some(engine) = self.engine.clone() {
            engine.install_tower_wake_driver(Arc::new(HttpServerWakeDriver::new(self.clone())));
        }
    }

    pub fn set_wake_replay_handler(
        &self,
        session_id: &str,
        variant: &str,
        handler: crate::session::wake::WakeReplayHandler,
    ) {
        self.prompt_queue
            .set_wake_replay_handler(session_id, variant, handler);
    }

    pub fn wake_replay_status(
        &self,
        session_id: &str,
        variant: &str,
    ) -> Option<crate::session::wake::WakeReplayStatus> {
        self.prompt_queue.wake_replay_status(session_id, variant)
    }

    pub(super) fn run_or_queue_prompt(
        &self,
        engine: &Arc<ServerEngine>,
        session_id: &str,
        item: Value,
        prompt: String,
        blocks: Vec<crate::rpc::types::ContentPart>,
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

#[cfg(test)]
mod wire_journal_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_hub_persister_writes_the_v2_journal_next_to_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(dir.path().join("sessions.db")).unwrap());
        let workspace = store.create_workspace("/work/wire", None).unwrap();
        store
            .create_session_with_workspace("sess-wire", None, Some(&workspace.id))
            .unwrap();
        let server = HttpServer::new(store.clone());
        let bus = server.hub().bus_for("sess-wire");
        for event in [
            json!({
                "type": "turn.prompt",
                "turnId": 1,
                "promptId": "prompt-1",
                "input": [{ "type": "text", "text": "hi" }],
                "origin": { "kind": "user" },
            }),
            json!({
                "type": "llm.step.begin",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "step_id": "turn-1-step-1",
            }),
            json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": "ok" },
            }),
            json!({
                "type": "llm.step.end",
                "agent_id": "main",
                "turn_id": "turn-1",
                "step": 1,
                "finish_reason": "stop",
                "provider_finish_reason": "completed",
                "raw_finish_reason": "stop",
            }),
            json!({
                "type": "turn.ended",
                "turnId": 1,
                "reason": "completed",
            }),
        ] {
            bus.publish(&crate::events::EngineEvent::Custom(event));
        }

        let path = dir
            .path()
            .join("sessions")
            .join(&workspace.id)
            .join("sess-wire")
            .join("agents")
            .join("main")
            .join(crate::session::wire_journal::WIRE_FILE_NAME);
        let contents = std::fs::read_to_string(&path).expect("the hub must write the journal");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 7, "{contents}");
        assert!(lines[0].contains("\"metadata\""), "{contents}");
        assert!(lines[1].contains("\"turn.prompt\""), "{contents}");
        assert!(
            lines[2].contains("\"context.append_message\""),
            "{contents}"
        );
        assert!(lines[3].contains("\"step.begin\""), "{contents}");
        assert!(lines[4].contains("\"content.part\""), "{contents}");
        assert!(lines[5].contains("\"step.end\""), "{contents}");
        assert!(lines[6].contains("\"turn.ended\""), "{contents}");
    }
}
