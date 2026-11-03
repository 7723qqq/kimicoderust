use super::*;

impl NativeToolset {
    /// Build a toolset rooted at `workspace_root`. Returns `None` when the
    /// root does not exist or cannot be canonicalized (no sandbox — no
    /// native execution).
    pub fn new(workspace_root: &str, shell_path: Option<&str>) -> Option<Self> {
        let root = std::fs::canonicalize(workspace_root).ok()?;
        if !root.is_dir() {
            return None;
        }
        let shell = if cfg!(windows) {
            // Windows without an explicit shell path resolves the `[shell]`
            // preference / auto-detected shell (pwsh → powershell → Git Bash →
            // cmd), mirroring the v2 native bash engine.
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
            file_history: Arc::new(std::sync::Mutex::new(None)),
            turn_id: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            loaded_tools: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            announced_tools: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            empty_disclosure_announced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            selection_failures: Arc::new(std::sync::Mutex::new(Default::default())),
            tools_filter: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            tool_select_enabled: false,
            bash_auto_background: true,
            bash_task_timeout_s: None,
            provider: None,
        })
    }

    /// Host-authorized extra roots (`additionalDirs`): directories outside the
    /// workspace root that this session may still read and write natively.
    /// Bind the skill scan: the extra roots the `Skill` tool scans **and** the
    /// `merge_all_available_skills` policy — both must match what the system
    /// prompt's skills section was rendered from, or the tool and the prompt
    /// advertise different skills.
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

    /// The sandbox this toolset executes inside. Roots were canonicalized when
    /// [`Self::with_extra_roots`] ran, so this is a cheap clone per call.
    pub(super) fn sandbox(&self) -> Sandbox {
        Sandbox {
            primary: self.root.clone(),
            extra: self.extra_roots.clone(),
        }
    }

    /// The shell path bridge this toolset resolves arguments through. Shared
    /// with the stale-write gate so both agree on what a path names — the
    /// gate's map key and the writer's target must be the same file.
    pub fn shell_bridge(&self) -> Arc<ShellPathBridge> {
        self.shell_bridge.clone()
    }

    /// Apply the host-resolved `[image]` limits for model-initiated reads
    /// (v2 `resolveReadImageByteBudget` / `resolveMaxImageEdgePx`).
    pub fn with_image_limits(
        mut self,
        read_byte_budget: Option<u64>,
        max_edge_px: Option<u32>,
    ) -> Self {
        self.image_read_byte_budget = read_byte_budget;
        self.image_max_edge_px = max_edge_px;
        self
    }

    /// Apply the session model's declared capabilities (v2 model catalog
    /// `capabilities`); an empty/`None` set stays unknown.
    pub fn with_model_capabilities(mut self, capabilities: Option<Vec<String>>) -> Self {
        self.model_capabilities = capabilities.filter(|caps| !caps.is_empty());
        self
    }

    /// Progressive tool disclosure switch (v2 `toolSelectService.enabled()`:
    /// the `tool_select` experimental flag). When on, the advertised tool
    /// table drops this toolset's deferred MCP tools and offers
    /// `select_tools` instead.
    #[must_use]
    pub fn with_tool_select_enabled(mut self, enabled: bool) -> Self {
        self.tool_select_enabled = enabled;
        self
    }

    /// Whether progressive tool disclosure is active for this toolset.
    pub fn tool_select_enabled(&self) -> bool {
        self.tool_select_enabled
    }

    /// The turn-start disclosure announcement (v2
    /// `toolSelectAnnouncementsService`): on every new turn, diff the current
    /// deferred tool set against what was already announced and render the
    /// `<tools_added>`/`<tools_removed>` block. `None` when disclosure is off
    /// or nothing changed. The announced set lives on the toolset, so the
    /// diff spans turns within a session.
    pub fn take_disclosure_announcement(&self) -> Option<String> {
        if !self.tool_select_enabled || !self.declares_capability("dynamically_loaded_tools") {
            return None;
        }
        let loadable = self
            .mcp_manager
            .as_ref()
            .map(|mcp| mcp.deferred_tool_names_blocking())
            .unwrap_or_default();
        let mut announced = self
            .announced_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let added: Vec<String> = loadable
            .iter()
            .filter(|name| !announced.contains(*name))
            .cloned()
            .collect();
        let removed: Vec<String> = announced
            .iter()
            .filter(|name| !loadable.contains(*name))
            .cloned()
            .collect();
        if added.is_empty() && removed.is_empty() {
            // Upstream #4190: when nothing can be loaded at all, say so once
            // (replacing the usage trailer), then stay silent until a real
            // delta resets the state.
            if !loadable.is_empty() {
                return None;
            }
            if self
                .empty_disclosure_announced
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return None;
            }
            return crate::tools::select_tools::render_loadable_tools_announcement(&[], &[], false);
        }
        self.empty_disclosure_announced
            .store(loadable.is_empty(), std::sync::atomic::Ordering::SeqCst);
        for name in &added {
            announced.insert(name.clone());
        }
        for name in &removed {
            announced.remove(name);
        }
        // A delta that drained the last loadable tools carries the empty-state
        // text instead of the usage trailer (upstream #4190's
        // `hasLoadableTools = loadable.length > 0`).
        crate::tools::select_tools::render_loadable_tools_announcement(
            &added,
            &removed,
            !loadable.is_empty(),
        )
    }

    /// The model-declared capability probe shared by the media and
    /// disclosure gates.
    pub(super) fn declares_capability(&self, name: &str) -> bool {
        self.model_capabilities
            .as_deref()
            .is_some_and(|caps| caps.iter().any(|cap| cap == name))
    }

    /// v2 #4364: the first turn's `<tools_added>` announcement must not run
    /// ahead of the MCP servers' first discovery — otherwise it announces a
    /// partial (often empty) set and the model is told nothing is loadable
    /// until the next turn's delta. Await the initial load before the
    /// announcement is computed; `wait_for_initial_load` settles when every
    /// connect does (success or failure, each bounded by its own connect
    /// timeout), so this cannot park a turn forever. No-op when disclosure
    /// is off or no MCP manager is wired.
    pub async fn wait_for_mcp_discovery_before_disclosure(&self) {
        if !self.tool_select_enabled || !self.declares_capability("dynamically_loaded_tools") {
            return;
        }
        if let Some(mcp) = &self.mcp_manager {
            mcp.wait_for_initial_load().await;
        }
    }

    /// `[background].bash_auto_background_on_timeout`: whether a timed-out
    /// foreground Bash call migrates to the background. `None` keeps the
    /// default (`true`).
    #[must_use]
    pub fn with_bash_auto_background(mut self, auto_background: Option<bool>) -> Self {
        self.bash_auto_background = auto_background.unwrap_or(true);
        self
    }

    /// `[background].bash_task_timeout_s`: the default timeout for background
    /// Bash tasks. `None` keeps the built-in 600s; `Some(0)` means "no
    /// timeout".
    #[must_use]
    pub fn with_bash_task_timeout(mut self, timeout_s: Option<u64>) -> Self {
        self.bash_task_timeout_s = timeout_s;
        self
    }

    /// The effective bound for a background Bash task: the call's own
    /// `timeout` wins over `[background].bash_task_timeout_s`, which wins over
    /// the built-in 600s. `disable_timeout` and a resolved `0` mean "no
    /// timeout"; anything else is clamped to 24h.
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

    /// Set the bound provider type (e.g. "kimi").
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Attach a TaskRunner for native background tasks and inspection.
    pub fn with_task_runner(mut self, runner: std::sync::Arc<crate::storage::TaskRunner>) -> Self {
        self.task_runner = Some(runner);
        self
    }

    /// The previous-session reminder for tasks the last process left behind,
    /// taken once so a turn only ever announces it a single time.
    ///
    /// The caller is the turn-level injection registry, which asks on every
    /// new turn; two gates keep it silent afterwards: the runner's persisted
    /// `resumeReminded` marker (so a later process does not repeat it
    /// either), and `already` — the ids an earlier reminder in this
    /// conversation already named (v2 `hasPreviousSessionReminder`).
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

    /// The session's *live* permission mode, as the host resolved it for this
    /// turn (`/permissions` can change it mid-session, and it is deliberately
    /// not the same thing as the mode in `config.toml`).
    ///
    /// `ExitPlanMode` needs it: an auto-mode session exits plan mode without
    /// asking, and reading `config.toml` instead would let a session the user
    /// set to `manual` auto-exit — silently, with no approval. `None` means
    /// "not wired", and the tool falls back to asking.
    pub fn with_permission_mode(mut self, mode: crate::permission::PermissionMode) -> Self {
        self.permission_mode = Some(mode);
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Enable native file-history capture for mutating file tools (`write` /
    /// `edit`). Each successful mutation records a `session_file_history`
    /// row (v2 `fileHistoryService.onWillExecuteTool` capture, plus the
    /// post-image diff) so `undo {revert_files:true}` and the
    /// `/file-history/*` endpoints have real data.
    ///
    /// `turn_id` is attributed on the toolset ([`Self::with_turn_id`], default
    /// 0) and installed alongside the recorder inside the blocking call, so it
    /// is visible to the tool regardless of which pool thread runs it.
    pub fn with_file_history(
        self,
        store: std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
        session_id: impl Into<String>,
    ) -> Self {
        self.set_file_history(store, session_id, self.effective_turn_id());
        self
    }

    /// Turn number attributed to file-history rows recorded by this toolset.
    #[must_use]
    pub fn with_turn_id(self, turn_id: usize) -> Self {
        self.set_turn_id(turn_id);
        self
    }

    /// Install/refresh the file-history recorder. Takes `&self` so a host can
    /// arm capture on a toolset already moved into the callbacks wrapper — the
    /// reason this is not builder-only.
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

    /// Refresh only the turn number (the recorder stays as installed).
    pub fn set_turn_id(&self, turn_id: usize) {
        self.turn_id
            .store(turn_id, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn effective_turn_id(&self) -> usize {
        self.turn_id.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The agent id executing the current turn: the [`CALLER_AGENT_ID`]
    /// task-local when the turn runner scoped one (a subagent's id), else the
    /// construction-time `caller_agent_id`, else `"main"`. The tower tools use
    /// this so a worker's calls are attributed to the worker rather than to the
    /// main agent, and the main-agent-only gate actually denies workers.
    pub(super) fn effective_caller_agent_id(&self) -> String {
        CALLER_AGENT_ID
            .try_with(|id| id.clone())
            .unwrap_or_else(|_| {
                self.caller_agent_id
                    .clone()
                    .unwrap_or_else(|| "main".to_string())
            })
    }

    /// The calling agent's cumulative token total for tower records (v2
    /// `callerTokens`, upstream #3847). `None` when no manager is attached
    /// or the caller has no recorded usage (the main agent's session total
    /// lives in the host's SQLite turn records, unreachable from here).
    pub(super) async fn caller_tokens(&self, caller_agent_id: &str) -> Option<i64> {
        let manager = self.subagent_manager.as_ref()?;
        manager.caller_tokens(caller_agent_id).await
    }

    /// Attach a SubagentManager for in-process multi-agent collaboration.
    pub fn with_subagents(
        mut self,
        manager: std::sync::Arc<crate::subagent::SubagentManager>,
    ) -> Self {
        self.subagent_manager = Some(manager);
        self
    }

    /// Attach an McpManager for external MCP server tools.
    pub fn with_mcp(mut self, manager: std::sync::Arc<crate::mcp::McpManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Get the attached McpManager if any.
    pub fn mcp_manager(&self) -> Option<&std::sync::Arc<crate::mcp::McpManager>> {
        self.mcp_manager.as_ref()
    }

    /// Shape the advertised tool table for progressive tool disclosure (v2
    /// `toolSelectService.shapeTools`): when the disclosure switch is on and
    /// the model declares `dynamically_loaded_tools`, the deferred MCP tools
    /// come out of the table and the `select_tools` loader is advertised in
    /// their place; when it is off, the table passes through unchanged
    /// (`select_tools` stays unadvertised — the model cannot see or call it).
    pub fn shape_tool_table(
        &self,
        mut tools: Vec<crate::turn_loop::types::ToolInfo>,
    ) -> Vec<crate::turn_loop::types::ToolInfo> {
        if !self.tool_select_enabled || !self.declares_capability("dynamically_loaded_tools") {
            return tools;
        }
        let deferred = self
            .mcp_manager
            .as_ref()
            .map(|mcp| mcp.deferred_tool_names_blocking())
            .unwrap_or_default();
        // Nothing is deferred: disclosure has nothing to fold, so the
        // loader is not advertised either (v2 keeps `select_tools` out of
        // the table whenever there are no dynamically loadable tools).
        if deferred.is_empty() {
            tools.retain(|tool| tool.name != crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME);
            return tools;
        }
        tools.retain(|tool| {
            tool.name == crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME
                || !deferred.contains(&tool.name)
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

    /// The full catalogue a `select_tools` call may load from: every
    /// advertised tool **plus** the deferred ones the table hides (v2
    /// `loadableToolNames`). Execution stays refused until the model loads a
    /// deferred name, so this set describes availability, not advertisement.
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
        // Host-registered tools join too: the shaped table filters them only
        // when their names collide with the toolset's, so the loader must
        // accept them as available.
        if let Some(callbacks) = &self.callbacks
            && let Ok(response) = callbacks.list_tools().await
        {
            names.extend(response.tools.into_iter().map(|tool| tool.name));
        }
        names
    }

    /// Attach the foreground `Agent` tool's turn context (P46): the host's
    /// resolved subagent timeout and the parent turn's cancellation signal
    /// (P51: flag + notify, so the subagent abort is event-driven).
    pub fn with_agent_context(
        mut self,
        timeout_ms: Option<u64>,
        parent_cancel: Option<crate::subagent::types::ParentCancel>,
    ) -> Self {
        self.subagent_timeout_ms = timeout_ms;
        self.parent_cancel = parent_cancel;
        self
    }

    /// Attach the P55 session-wide cancel slot (session-built toolsets).
    pub fn with_parent_cancel_slot(
        mut self,
        slot: Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>,
    ) -> Self {
        self.parent_cancel_slot = Some(slot);
        self
    }

    /// [`Self::with_parent_cancel_slot`] for optional slots.
    pub fn with_parent_cancel_slot_if(
        mut self,
        slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    ) -> Self {
        self.parent_cancel_slot = slot;
        self
    }

    /// Attach the session-wide steer slot (#3697): the per-turn signal a
    /// steering message fires to end a blocking `WaitFor` early.
    pub fn with_steer_slot_if(
        mut self,
        slot: Option<Arc<std::sync::Mutex<Option<crate::subagent::types::ParentCancel>>>>,
    ) -> Self {
        self.steer_slot = slot;
        self
    }

    /// The live parent cancel signal: the session slot wins (per-turn
    /// refresh), the static value is the per-turn-wired fallback.
    pub(super) fn effective_parent_cancel(&self) -> Option<crate::subagent::types::ParentCancel> {
        if let Some(slot) = &self.parent_cancel_slot {
            let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(signal) = guard.as_ref() {
                return Some(signal.clone());
            }
        }
        self.parent_cancel.clone()
    }

    /// The live steer signal for this turn, when a steering-capable session
    /// wired one.
    pub(super) fn effective_steer(&self) -> Option<crate::subagent::types::ParentCancel> {
        let slot = self.steer_slot.as_ref()?;
        let guard = slot.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    }

    /// Attach the host callbacks for interactive tools (AskUserQuestion).
    pub fn with_callbacks(
        mut self,
        callbacks: std::sync::Arc<dyn crate::callbacks::HostCallbacks>,
    ) -> Self {
        self.callbacks = Some(callbacks);
        self
    }

    /// Attach host-resolved `[github]` config credentials for the native
    /// GitHub tools.
    pub fn with_github_credentials(mut self, credentials: github::GitHubCredentials) -> Self {
        self.github_credentials = Some(credentials);
        self
    }

    /// Attach the user's global `[tools]` switch; every native call must
    /// survive it even when the model calls a disabled tool from memory.
    pub fn with_tools_filter(mut self, filter: Option<tool_policy::ToolsFilter>) -> Self {
        self.tools_filter = filter;
        self
    }

    /// The user's global `[tools]` switch, if configured.
    pub fn tools_filter(&self) -> Option<&tool_policy::ToolsFilter> {
        self.tools_filter.as_ref()
    }

    /// Host-resolved `[github]` credentials, when the session has any.
    pub fn github_credentials(&self) -> Option<&github::GitHubCredentials> {
        self.github_credentials.as_ref()
    }

    /// Attach the `[secondary_model]` subagent model pool.
    pub fn with_secondary_model(
        mut self,
        pool: Option<std::sync::Arc<crate::subagent::secondary::SecondaryModelRuntime>>,
    ) -> Self {
        self.secondary_model = pool;
        self
    }

    /// The subagent model pool, when `[secondary_model]` is configured.
    pub fn secondary_model(
        &self,
    ) -> Option<&std::sync::Arc<crate::subagent::secondary::SecondaryModelRuntime>> {
        self.secondary_model.as_ref()
    }

    /// The attached SubagentManager, when the pipeline wired one
    /// (`with_subagents`) — the spawn gate's catalog source for the `Agent`
    /// description's type listing.
    pub fn subagent_manager(&self) -> Option<&std::sync::Arc<crate::subagent::SubagentManager>> {
        self.subagent_manager.as_ref()
    }
}
