use super::*;

const RETRIEVAL_MIN_TOOLS: usize = 20;

const RETRIEVAL_TOP_K: usize = 10;

impl NativeToolset {
    pub fn new(workspace_root: &str, shell_path: Option<&str>) -> Option<Self> {
        let root = std::fs::canonicalize(workspace_root).ok()?;
        if !root.is_dir() {
            return None;
        }
        let shell = if cfg!(windows) {
            shell_path
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .or_else(|| Some(crate::native::shell::resolve_shell(None).program))
        } else {
            Some(
                shell_path
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or("bash")
                    .to_string(),
            )
        };
        Some(Self {
            root,
            extra_roots: Vec::new(),
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            shell_bridge: std::sync::Arc::new(
                crate::native::shell_path_bridge::ShellPathBridge::new(
                    shell.as_deref().unwrap_or_default(),
                ),
            ),
            shell,
            subagent_manager: None,
            mcp_manager: None,
            subagent_timeout_ms: None,
            parent_cancel: None,
            parent_cancel_slot: None,
            steer_slot: None,
            callbacks: None,
            github_credentials: None,
            caller_agent_id: None,
            permission_mode: None,
            session_id: None,
            task_runner: None,
            tower_wake_runtime: Arc::new(std::sync::Mutex::new(None)),
            file_history: Arc::new(std::sync::Mutex::new(None)),
            session_store: Arc::new(std::sync::Mutex::new(None)),
            turn_id: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            loaded_tools: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            selection_failures: Arc::new(std::sync::Mutex::new(Default::default())),
            wait_tally: Arc::new(std::sync::Mutex::new(Default::default())),
            progress_tracker: Arc::new(std::sync::Mutex::new(Default::default())),
            tools_filter: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            tool_select_enabled: false,
            initial_tool_names: std::sync::Arc::new(std::sync::Mutex::new(None)),
            bash_auto_background: true,
            bash_task_timeout_s: None,
            tower_max_running_tasks: None,
            provider: None,
        })
    }

    pub fn with_skill_scan(
        mut self,
        dirs: impl IntoIterator<Item = PathBuf>,
        plugin_dirs: Vec<crate::skills::PluginSkillDir>,
        merge_all_available_skills: bool,
    ) -> Self {
        self.skill_dirs = dirs.into_iter().collect();
        self.plugin_skill_dirs = plugin_dirs;
        self.merge_all_available_skills = merge_all_available_skills;
        self
    }

    /// The plugin-contributed skill roots this toolset scans (v2
    /// `manager.pluginSkillRoots`), carrying each plugin's `sessionStart`
    /// declaration. The turn loop reads the session-start skills from here so
    /// the prompt, the `Skill` tool and the injection agree on one catalog.
    pub fn plugin_skill_dirs(&self) -> &[crate::skills::PluginSkillDir] {
        &self.plugin_skill_dirs
    }

    /// The session this toolset serves, when the host bound one (v2
    /// `ISessionContext.sessionId`).
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn with_extra_roots(mut self, extra: Vec<String>) -> Self {
        let roots: Vec<PathBuf> = extra
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .collect();
        self.extra_roots = Sandbox::new(self.root.clone())
            .with_extra(roots)
            .extra()
            .to_vec();
        self
    }

    pub(super) fn sandbox(&self) -> Sandbox {
        let primary = self.execution_root();
        let mut extra = self.extra_roots.clone();
        if primary != self.root && !extra.contains(&self.root) {
            extra.insert(0, self.root.clone());
        }
        Sandbox { primary, extra }
    }

    pub(super) fn caller_environment(&self) -> Option<crate::environment::AgentEnvironment> {
        let manager = self.subagent_manager.as_ref()?;
        manager.agent_environment_sync(&self.effective_caller_agent_id())
    }

    pub fn execution_root(&self) -> PathBuf {
        self.caller_environment()
            .map(|environment| environment.cwd().to_path_buf())
            .unwrap_or_else(|| self.root.clone())
    }

    pub(super) fn execution_environment(&self) -> std::collections::BTreeMap<String, String> {
        self.caller_environment()
            .map(|environment| environment.env().clone())
            .unwrap_or_default()
    }

    pub fn shell_bridge(&self) -> Arc<ShellPathBridge> {
        self.shell_bridge.clone()
    }

    pub fn tower_root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn workspace_root(&self) -> &std::path::Path {
        &self.root
    }

    pub fn with_image_limits(
        mut self,
        read_byte_budget: Option<u64>,
        max_edge_px: Option<u32>,
    ) -> Self {
        self.image_read_byte_budget = read_byte_budget;
        self.image_max_edge_px = max_edge_px;
        self
    }

    pub fn with_model_capabilities(mut self, capabilities: Option<Vec<String>>) -> Self {
        self.model_capabilities = capabilities.filter(|caps| !caps.is_empty());
        self
    }

    #[must_use]
    pub fn with_tool_select_enabled(mut self, enabled: bool) -> Self {
        self.tool_select_enabled = enabled;
        self
    }

    pub fn tool_select_enabled(&self) -> bool {
        self.tool_select_enabled
    }

    pub fn supports_dynamic_tool_loading(&self) -> bool {
        self.tool_select_enabled
            && self.declares_capability("dynamically_loaded_tools")
            && self.declares_capability("tool_use")
    }

    pub fn record_initial_tool_table_ref(&self, tools: &[crate::turn_loop::types::ToolInfo]) {
        let mut snapshot = self
            .initial_tool_names
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if snapshot.is_none() {
            *snapshot = Some(tools.iter().map(|tool| tool.name.clone()).collect());
        }
    }

    pub fn seed_loaded_tools_from_history(&self, history: &[crate::turn_loop::types::LLMMessage]) {
        if !self.supports_dynamic_tool_loading() {
            return;
        }
        let announced = crate::tools::tool_select_state::fold_announced_tool_names(history);
        if announced.is_empty() {
            return;
        }
        let mut loaded = self
            .loaded_tools
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        loaded.extend(announced);
    }

    pub fn is_dynamic_tool_loaded(&self, name: &str) -> bool {
        self.loaded_tools
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(name)
    }

    pub fn take_disclosure_announcement(
        &self,
        history: &[crate::turn_loop::types::LLMMessage],
    ) -> Option<String> {
        if !self.supports_dynamic_tool_loading() {
            return None;
        }

        let entries: Vec<crate::tools::tool_retriever::ToolSearchEntry> = self
            .mcp_manager
            .as_ref()
            .map(|mcp| mcp.deferred_tool_infos_blocking())
            .unwrap_or_default()
            .into_iter()
            .filter(|(name, _, _)| {
                self.tools_filter
                    .as_ref()
                    .is_none_or(|filter| filter.allows(name))
            })
            .map(|(name, description, schema)| {
                let parameter_names = schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .map(|properties| properties.keys().cloned().collect())
                    .unwrap_or_default();
                crate::tools::tool_retriever::ToolSearchEntry {
                    name,
                    description,
                    source: "mcp".into(),
                    parameter_names,
                }
            })
            .collect();
        if entries.is_empty() {
            return self.empty_disclosure_announcement(history);
        }

        let loaded = self.active_loaded_tool_names();
        let candidates: Vec<crate::tools::tool_retriever::ToolSearchEntry> = entries
            .iter()
            .filter(|entry| !loaded.contains(&entry.name))
            .cloned()
            .collect();

        if entries.len() >= RETRIEVAL_MIN_TOOLS {
            if candidates.is_empty() {
                return None;
            }
            let query =
                crate::tools::tool_select_state::latest_user_query(history).unwrap_or_default();
            let ranked = crate::tools::tool_retriever::rank_tools_by_query(
                &candidates,
                &query,
                RETRIEVAL_TOP_K,
            );

            let selected: Vec<crate::tools::tool_retriever::ToolSearchHit> = if ranked.is_empty() {
                candidates
                    .iter()
                    .take(RETRIEVAL_TOP_K)
                    .map(|entry| crate::tools::tool_retriever::ToolSearchHit {
                        name: entry.name.clone(),
                        description: entry.description.clone(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                ranked
            };
            let announcement = crate::tools::tool_select_state::render_retrieved_tools_announcement(
                &selected,
                candidates.len(),
            );
            let previous =
                crate::tools::tool_select_state::last_retrieved_announcement_text(history);
            if previous.as_deref() == Some(announcement.as_str()) {
                return None;
            }
            return Some(announcement);
        }

        let mut loadable: Vec<String> = entries.iter().map(|entry| entry.name.clone()).collect();
        loadable.sort();
        let loadable_set: std::collections::HashSet<&str> =
            loadable.iter().map(String::as_str).collect();
        let announced = crate::tools::tool_select_state::fold_announced_tool_names(history);
        let added: Vec<String> = loadable
            .iter()
            .filter(|name| !announced.contains(*name))
            .cloned()
            .collect();
        let mut removed: Vec<String> = announced
            .iter()
            .filter(|name| !loadable_set.contains(name.as_str()))
            .cloned()
            .collect();
        removed.sort();
        crate::tools::select_tools::render_loadable_tools_announcement(
            &added,
            &removed,
            !loadable.is_empty(),
        )
    }

    fn empty_disclosure_announcement(
        &self,
        history: &[crate::turn_loop::types::LLMMessage],
    ) -> Option<String> {
        let empty =
            crate::tools::select_tools::render_loadable_tools_announcement(&[], &[], false)?;
        let last = history.iter().rev().find(|message| {
            crate::tools::tool_select_state::is_loadable_tools_announcement(message)
        });
        if last.is_some_and(|message| message.content.contains(&empty)) {
            return None;
        }
        Some(empty)
    }

    fn active_loaded_tool_names(&self) -> std::collections::HashSet<String> {
        let loadable = self
            .mcp_manager
            .as_ref()
            .map(|mcp| mcp.deferred_tool_names_blocking())
            .unwrap_or_default();
        self.loaded_tools
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|name| loadable.contains(*name))
            .filter(|name| {
                self.tools_filter
                    .as_ref()
                    .is_none_or(|filter| filter.allows(name))
            })
            .cloned()
            .collect()
    }

    pub fn observe_progress(&self, receipt: &crate::turn_loop::progress_track::ToolReceipt) -> u32 {
        self.progress_tracker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(receipt)
    }

    pub(super) fn declares_capability(&self, name: &str) -> bool {
        self.model_capabilities
            .as_deref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == name))
    }

    pub async fn wait_for_mcp_discovery_before_disclosure(&self) {
        if !self.supports_dynamic_tool_loading() {
            return;
        }
        if let Some(mcp) = &self.mcp_manager {
            mcp.wait_for_initial_load().await;
        }
    }

    #[must_use]
    pub fn with_bash_auto_background(mut self, auto_background: Option<bool>) -> Self {
        self.bash_auto_background = auto_background.unwrap_or(true);
        self
    }

    #[must_use]
    pub fn with_bash_task_timeout(mut self, timeout_s: Option<u64>) -> Self {
        self.bash_task_timeout_s = timeout_s;
        self
    }

    #[must_use]
    pub fn with_tower_max_running_tasks(mut self, max_running_tasks: Option<u32>) -> Self {
        self.tower_max_running_tasks = max_running_tasks.filter(|value| *value > 0);
        self
    }

    pub(super) fn background_bash_timeout(&self, args: &Value) -> Option<Duration> {
        if args
            .get("disable_timeout")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return None;
        }
        let seconds = args
            .get("timeout")
            .and_then(Value::as_u64)
            .or(self.bash_task_timeout_s)
            .unwrap_or(BASH_TASK_DEFAULT_TIMEOUT_S);
        if seconds == 0 {
            return None;
        }
        Some(Duration::from_secs(seconds.min(BASH_TASK_MAX_SECONDS)))
    }

    pub(super) fn read_media_limits(&self) -> read_media::ReadMediaLimits {
        let capability = |name: &str| {
            self.model_capabilities
                .as_deref()
                .map(|caps| caps.iter().any(|cap| cap == name))
        };
        read_media::ReadMediaLimits {
            read_byte_budget: self.image_read_byte_budget,
            max_edge_px: self.image_max_edge_px,
            image_in: capability("image_in"),
            video_in: capability("video_in"),
            provider: self.provider.clone(),
        }
    }

    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn with_task_runner(mut self, runner: std::sync::Arc<crate::storage::TaskRunner>) -> Self {
        self.task_runner = Some(runner);
        self
    }

    pub fn bind_tower_wake_runtime(
        &self,
        runtime: Arc<crate::tools::tower::wake::TowerWakeRuntime>,
    ) -> crate::session::wake::WakeReplayHandler {
        *self
            .tower_wake_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(runtime.clone());
        runtime.replay_handler(
            self.session_id.as_deref().unwrap_or("session-main"),
            &self.root,
            self.task_runner.as_ref(),
        )
    }

    pub fn bind_tower_wake_driver(
        &self,
        driver: Arc<dyn crate::session::wake::TowerWakeDriver>,
    ) -> crate::session::wake::WakeReplayHandler {
        self.bind_tower_wake_runtime(crate::tools::tower::wake::TowerWakeRuntime::new(driver))
    }

    pub(crate) fn tower_wake_runtime(
        &self,
    ) -> Option<Arc<crate::tools::tower::wake::TowerWakeRuntime>> {
        self.tower_wake_runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn take_previous_session_reminder(
        &self,
        already: &HashSet<String>,
    ) -> Option<String> {
        let runner = self.task_runner.as_ref()?;
        runner.reconcile_previous_session(already)
    }

    pub fn with_caller_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.caller_agent_id = Some(agent_id.into());
        self
    }

    pub fn with_permission_mode(mut self, mode: crate::permission::PermissionMode) -> Self {
        self.permission_mode = Some(mode);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_file_history(
        self,
        store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
        session_id: impl Into<String>,
    ) -> Self {
        self.set_file_history(store, session_id, self.effective_turn_id());
        self
    }

    #[must_use]
    pub fn with_turn_id(self, turn_id: usize) -> Self {
        self.set_turn_id(turn_id);
        self
    }

    pub fn set_file_history(
        &self,
        store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
        session_id: impl Into<String>,
        turn_id: usize,
    ) {
        *self.file_history.lock().unwrap_or_else(|e| e.into_inner()) = Some(FileHistoryCtx {
            store,
            session_id: session_id.into(),
        });
        self.set_turn_id(turn_id);
    }

    pub fn set_turn_id(&self, turn_id: usize) {
        self.turn_id
            .store(turn_id, std::sync::atomic::Ordering::SeqCst);
    }

    #[must_use]
    pub fn with_session_store(
        self,
        store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
    ) -> Self {
        self.set_session_store(store);
        self
    }

    pub fn set_session_store(
        &self,
        store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
    ) {
        *self.session_store.lock().unwrap_or_else(|e| e.into_inner()) = Some(store);
    }

    pub(super) fn session_store(
        &self,
    ) -> Option<std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>> {
        self.session_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(super) fn effective_turn_id(&self) -> usize {
        self.turn_id.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) fn effective_caller_agent_id(&self) -> String {
        CALLER_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_else(|_| {
                self.caller_agent_id
                    .clone()
                    .unwrap_or_else(|| "main".to_string())
            })
    }

    pub fn with_subagents(
        mut self,
        manager: std::sync::Arc<crate::subagent::SubagentManager>,
    ) -> Self {
        self.subagent_manager = Some(manager);
        self
    }

    pub fn with_mcp(mut self, manager: std::sync::Arc<crate::mcp::McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub fn mcp_manager(&self) -> Option<&std::sync::Arc<crate::mcp::McpManager>> {
        self.mcp_manager.as_ref()
    }

    pub fn shape_tool_table(
        &self,
        mut tools: Vec<crate::turn_loop::types::ToolInfo>,
    ) -> Vec<crate::turn_loop::types::ToolInfo> {
        if !self.supports_dynamic_tool_loading() {
            let snapshot = self
                .initial_tool_names
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            return match snapshot.as_ref() {
                Some(initial) => tools
                    .into_iter()
                    .filter(|tool| initial.contains(&tool.name))
                    .collect(),
                None => tools,
            };
        }
        let deferred = self
            .mcp_manager
            .as_ref()
            .map(|mcp| mcp.deferred_tool_names_blocking())
            .unwrap_or_default();

        if deferred.is_empty() {
            tools.retain(|tool| tool.name != crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME);
            return tools;
        }
        let loaded = self
            .loaded_tools
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        tools.retain(|tool| {
            tool.name == crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME
                || !deferred.contains(&tool.name)
                || loaded.contains(&tool.name)
        });
        if tools
            .iter()
            .all(|tool| tool.name != crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME)
        {
            tools.push(crate::turn_loop::types::ToolInfo {
                name: crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME.to_string(),
                description: crate::tools::select_tools::SELECT_TOOLS_DESCRIPTION.to_string(),
                input_schema: crate::tools::select_tools::select_tools_schema(),
            });
        }
        tools
    }

    pub async fn full_tool_catalogue(&self) -> std::collections::HashSet<String> {
        let mut names: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(mcp) = &self.mcp_manager {
            mcp.wait_for_initial_load().await;
            for tool in mcp.list_tool_infos().await {
                names.insert(tool.name);
            }
        }
        names.extend(
            crate::tools::tool_policy::native_tool_defs(
                self.github_credentials().is_some(),
                false,
                None,
                self.secondary_model.as_deref(),
            )
            .into_iter()
            .map(|tool| tool.name),
        );

        if let Some(callbacks) = &self.callbacks
            && let Ok(response) = callbacks.list_tools().await
        {
            names.extend(response.tools.into_iter().map(|tool| tool.name));
        }
        names
    }

    pub fn with_agent_context(
        mut self,
        timeout_ms: Option<u64>,
        parent_cancel: Option<crate::subagent::types::ParentCancel>,
    ) -> Self {
        self.subagent_timeout_ms = timeout_ms;
        self.parent_cancel = parent_cancel;
        self
    }

    pub fn with_parent_cancel_slot(
        mut self,
        slot: Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>,
    ) -> Self {
        self.parent_cancel_slot = Some(slot);
        self
    }

    pub fn with_parent_cancel_slot_if(
        mut self,
        slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    ) -> Self {
        self.parent_cancel_slot = slot;
        self
    }

    pub fn with_steer_slot_if(
        mut self,
        slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    ) -> Self {
        self.steer_slot = slot;
        self
    }

    pub(super) fn effective_parent_cancel(&self) -> Option<crate::subagent::types::ParentCancel> {
        if let Some(slot) = &self.parent_cancel_slot {
            let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(signal) = guard.as_ref() {
                return Some(signal.clone());
            }
        }
        self.parent_cancel.clone()
    }

    pub(super) fn effective_steer(&self) -> Option<crate::subagent::types::ParentCancel> {
        let slot = self.steer_slot.as_ref()?;
        let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    }

    pub fn with_callbacks(
        mut self,
        callbacks: std::sync::Arc<dyn crate::callbacks::HostCallbacks>,
    ) -> Self {
        self.callbacks = Some(callbacks);
        self
    }

    pub fn with_github_credentials(mut self, credentials: github::GitHubCredentials) -> Self {
        self.github_credentials = Some(credentials);
        self
    }

    pub fn with_tools_filter(mut self, filter: Option<tool_policy::ToolsFilter>) -> Self {
        self.tools_filter = filter;
        self
    }

    pub fn tools_filter(&self) -> Option<&tool_policy::ToolsFilter> {
        self.tools_filter.as_ref()
    }

    pub fn github_credentials(&self) -> Option<&github::GitHubCredentials> {
        self.github_credentials.as_ref()
    }

    pub fn with_secondary_model(
        mut self,
        pool: Option<std::sync::Arc<crate::subagent::secondary::SecondaryModelRuntime>>,
    ) -> Self {
        self.secondary_model = pool;
        self
    }

    pub fn secondary_model(
        &self,
    ) -> Option<&std::sync::Arc<crate::subagent::secondary::SecondaryModelRuntime>> {
        self.secondary_model.as_ref()
    }

    pub fn subagent_manager(&self) -> Option<&std::sync::Arc<crate::subagent::SubagentManager>> {
        self.subagent_manager.as_ref()
    }
}
