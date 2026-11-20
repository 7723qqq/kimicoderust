use super::*;

use crate::features::operationLog::operationService;

const RETRIEVAL_MIN_TOOLS: usize = 20;

const RETRIEVAL_TOP_K: usize = 10;

impl NativeToolset {
    pub fn new(workspace_root: &str, shell_path: Option<&str>) -> Option<Self> {
        Self::try_new(workspace_root, shell_path).ok().flatten()
    }

    pub fn try_new(
        workspace_root: &str,
        shell_path: Option<&str>,
    ) -> Result<Option<Self>, crate::native::environment_probe::ProbeShellNotFound> {
        let Some(root) = std::fs::canonicalize(workspace_root).ok() else {
            return Ok(None);
        };
        if !root.is_dir() {
            return Ok(None);
        }
        let shell = if cfg!(windows) {
            shell_path
                .map(str::to_string)
                .filter(|s| !s.trim().is_empty())
                .map(Ok)
                .unwrap_or_else(|| {
                    crate::native::shell::resolve_shell(None).map(|shell| shell.program)
                })?
        } else {
            shell_path
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("bash")
                .to_string()
        };
        let shell = Some(shell);
        let git_memory = crate::tools::memory_store::memory_base().map(|base| {
            Arc::new(crate::tools::git_memory::GitMemoryService::new(
                root.clone(),
                base,
            ))
        });
        let file_memory = git_memory.clone().map(|service| {
            service as Arc<dyn crate::agent::tools::file_memory::FileMemoryProvider>
        });
        let operation_log = operationService::AgentOperationService::new(
            root.to_string_lossy(),
            "",
            "main",
        );
        Ok(Some(Self {
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
            non_interactive: false,
            task_runner: None,
            tower_wake_runtime: Arc::new(std::sync::Mutex::new(None)),
            file_history: Arc::new(std::sync::Mutex::new(None)),
            operation_log,
            file_memory,
            git_memory,
            session_store: Arc::new(std::sync::Mutex::new(None)),
            turn_id: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            llm_trace_id: Arc::new(std::sync::Mutex::new(None)),
            loaded_tools: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            tool_select_pending_loaded: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            selection_failures: Arc::new(std::sync::Mutex::new(Default::default())),
            tool_select_state: Arc::new(std::sync::Mutex::new(Default::default())),
            tool_select_reconcile: Arc::new(tokio::sync::Mutex::new(())),
            tool_select_notified_updates: Arc::new(std::sync::Mutex::new(Default::default())),
            tool_select_pending_reminders: Arc::new(std::sync::Mutex::new(Vec::new())),
            tool_select_last_disclosure: Arc::new(std::sync::Mutex::new(None)),
            tool_select_persist_listener: Arc::new(std::sync::Mutex::new(None)),
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
            notify_user_enabled: true,
            provider: None,
            memory_graph: Arc::new(std::sync::Mutex::new(None)),
        }))
    }

    pub fn git_memory_service(&self) -> Option<Arc<crate::tools::git_memory::GitMemoryService>> {
        self.git_memory.clone()
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

    pub fn plugin_skill_dirs(&self) -> &[crate::skills::PluginSkillDir] {
        &self.plugin_skill_dirs
    }

    pub fn skill_scan_roots(&self) -> crate::skills::SkillScanRoots {
        crate::skills::SkillScanRoots {
            root: Some(self.root.clone()),
            extra_dirs: self.skill_dirs.clone(),
            plugin_dirs: self.plugin_skill_dirs.clone(),
            merge_all_available_skills: self.merge_all_available_skills,
        }
    }

    pub fn skill_listing_outcome(&self) -> crate::primitives::read::ReadOutcome<String> {
        let scan = self.skill_scan_roots();
        let roots = scan.scanned_roots();
        scan.catalog_outcome()
            .map(|skills| {
                crate::prompt::skills_renderer::render_skills_markdown_with_roots(
                    &skills,
                    &roots,
                )
            })
    }

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

    pub fn workspace_roots(
        &self,
    ) -> Result<crate::environment::WorkspaceRoots, crate::environment::EnvironmentError> {
        crate::environment::WorkspaceRoots::new(self.root.clone(), self.extra_roots.clone())
    }

    pub fn start_agents_md_watch(
        &self,
        root: &std::path::Path,
    ) -> crate::environment::AgentsMdWatch {
        crate::environment::AgentsMdWatch::start(root, None, super::user_home_dir().as_deref())
    }

    pub fn user_home_dir(&self) -> Option<PathBuf> {
        super::user_home_dir()
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

    pub async fn reconcile_tool_select(
        &self,
        active: &[crate::tools::tool_select_ops::ToolSelectTool],
        live: &[crate::tools::tool_select_ops::ToolSelectTool],
    ) -> Result<crate::tools::tool_select_ops::ToolSelectReconcilePlan, String> {
        let _serial = self.tool_select_reconcile.lock().await;
        let agent_id = self.effective_caller_agent_id();
        let persistence = self
            .session_store()
            .zip(self.session_id.as_deref().map(str::to_string));
        let state = match &persistence {
            Some((store, session_id)) => crate::tools::tool_select_ops::restore_tool_select_state(
                store, session_id, &agent_id,
            )?,
            None => self
                .tool_select_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        };
        let notified = self
            .tool_select_notified_updates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let plan = crate::tools::tool_select_ops::plan_tool_select_reconcile(
            &state, &agent_id, active, live, &notified,
        );
        let mut persisted_seq = None;
        if let Some((store, session_id)) = &persistence {
            if let Some(event) = &plan.initialized {
                persisted_seq = Some(self.persist_tool_select_event(
                    store,
                    session_id,
                    crate::tools::tool_select_ops::TOOL_SELECT_INITIALIZED,
                    event,
                )?);
            }
            if let Some(event) = &plan.changed {
                persisted_seq = Some(self.persist_tool_select_event(
                    store,
                    session_id,
                    crate::tools::tool_select_ops::TOOL_SELECT_TOOLS_CHANGED,
                    event,
                )?);
            }
        }
        if let (Some(seq), Some(listener)) = (
            persisted_seq,
            self.tool_select_persist_listener
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        ) {
            listener(self.session_id.as_deref().unwrap_or_default(), seq);
        }
        *self
            .tool_select_state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = plan.state.clone();
        self.tool_select_notified_updates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(plan.updates.iter().map(|update| update.signature.clone()));
        self.tool_select_pending_reminders
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(plan.updates.iter().map(|update| {
                format!(
                    "Tool \"{}\" has an update, but the update is not applied in this session. Continue using the currently registered version.",
                    update.name
                )
            }));
        Ok(plan)
    }

    fn persist_tool_select_event<T: serde::Serialize>(
        &self,
        store: &std::sync::Arc<crate::session::sqlite_store::SqliteSessionStore>,
        session_id: &str,
        event_type: &str,
        event: &T,
    ) -> Result<u64, String> {
        if let Some(callbacks) = self.callbacks.as_ref() {
            let mut payload = serde_json::to_value(event).map_err(|error| error.to_string())?;
            payload["type"] = serde_json::json!(event_type);
            callbacks.emit_event(payload);
            return store
                .latest_wire_event_seq(session_id)
                .map_err(|error| error.to_string());
        }
        crate::tools::tool_select_ops::persist_tool_select_payload(
            store, session_id, event_type, event,
        )
    }

    pub fn take_tool_update_pending_reminders(&self) -> Vec<String> {
        std::mem::take(
            &mut *self
                .tool_select_pending_reminders
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    pub fn set_tool_select_persist_listener(&self, listener: Arc<dyn Fn(&str, u64) + Send + Sync>) {
        *self
            .tool_select_persist_listener
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(listener);
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
        let history = crate::tools::tool_select_state::context_messages(history);
        let state = self
            .tool_select_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let current = if state.initial.is_some() {
            state.current
        } else {
            Vec::new()
        };
        let mut loaded: std::collections::HashSet<String> =
            crate::tools::tool_select_state::collect_loaded_dynamic_tools(&history, &current)
                .into_keys()
                .collect();
        loaded.extend(
            self.tool_select_pending_loaded
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .cloned(),
        );
        *self
            .loaded_tools
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = loaded;
    }

    pub fn drain_pending_tool_schemas(&self) -> Option<Vec<kimi_core::types::ToolDescription>> {
        if !self.supports_dynamic_tool_loading() {
            return None;
        }
        let mut pending = self
            .tool_select_pending_loaded
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.is_empty() {
            return None;
        }
        let state = self
            .tool_select_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let mut names: Vec<String> = pending.iter().cloned().collect();
        names.sort();
        let deferred_infos = self
            .mcp_manager
            .as_ref()
            .map(|mcp| mcp.deferred_tool_infos_blocking())
            .unwrap_or_default();
        let mut tools = Vec::new();
        for name in names {
            let description =
                if let Some(tool) = state.current.iter().find(|tool| tool.name == name) {
                    Some(kimi_core::types::ToolDescription {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        parameters: tool.parameters.clone(),
                        deferred: None,
                    })
                } else {
                    deferred_infos
                        .iter()
                        .find(|(tool_name, _, _)| tool_name == &name)
                        .map(|(tool_name, description, parameters)| {
                            kimi_core::types::ToolDescription {
                                name: tool_name.clone(),
                                description: description.clone(),
                                parameters: parameters.as_object().cloned().unwrap_or_default(),
                                deferred: None,
                            }
                        })
                };
            let Some(tool) = description else {
                continue;
            };
            pending.remove(&name);
            tools.push(tool);
        }
        if tools.is_empty() {
            return None;
        }
        let mut loaded = self
            .loaded_tools
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for tool in &tools {
            loaded.insert(tool.name.clone());
        }
        Some(tools)
    }

    pub fn clear_pending_tool_schemas(&self) {
        self.tool_select_pending_loaded
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
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
        let history = crate::tools::tool_select_state::context_messages(history);
        self.take_last_announcement_disclosure();
        if !self.tool_select_enabled {
            return None;
        }
        self.ensure_tool_select_state_for_announcement();
        let state = self
            .tool_select_state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        state.initial.as_ref()?;
        self.selection_failures
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reset_if_environment_changed(
                crate::tools::select_tools::selection_environment_for_tools(
                    self.tools_filter.as_ref(),
                    &state.current,
                ),
            );
        let revision = state.changes.len();
        let loadable = self.loadable_tool_entries(&state);
        let result = if loadable.len() >= RETRIEVAL_MIN_TOOLS {
            let loaded = self
                .loaded_tools
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let candidates = loadable
                .iter()
                .filter(|tool| !loaded.contains(&tool.name))
                .cloned()
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                return None;
            }
            let query =
                crate::tools::tool_select_state::latest_user_query(&history).unwrap_or_default();
            let search_entries = candidates
                .iter()
                .map(|tool| crate::tools::tool_retriever::ToolSearchEntry {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    source: match tool.source {
                        crate::tools::tool_select_ops::ToolSelectSource::Builtin => "builtin",
                        crate::tools::tool_select_ops::ToolSelectSource::User => "user",
                        crate::tools::tool_select_ops::ToolSelectSource::Mcp => "mcp",
                    }
                    .to_string(),
                    parameter_names: tool
                        .parameters
                        .get("properties")
                        .and_then(serde_json::Value::as_object)
                        .map(|properties| properties.keys().cloned().collect())
                        .unwrap_or_default(),
                })
                .collect::<Vec<_>>();
            let ranked = crate::tools::tool_retriever::rank_tools_by_query(
                &search_entries,
                &query,
                RETRIEVAL_TOP_K,
            );
            let matched = !query.trim().is_empty() && ranked.iter().any(|hit| hit.score > 0.0);
            let selected = if ranked.is_empty() {
                search_entries
                    .iter()
                    .take(RETRIEVAL_TOP_K)
                    .map(|entry| crate::tools::tool_retriever::ToolSearchHit {
                        name: entry.name.clone(),
                        description: entry.description.clone(),
                        score: 0.0,
                    })
                    .collect::<Vec<_>>()
            } else {
                ranked
            };
            let content = crate::tools::tool_select_state::render_retrieved_tools_announcement(
                &selected,
                candidates.len(),
            );
            if crate::tools::tool_select_state::last_retrieved_announcement_text(&history)
                .as_deref()
                == Some(content.as_str())
            {
                return None;
            }
            if let Some(callbacks) = &self.callbacks {
                callbacks.telemetry(serde_json::json!({
                    "event": "tool_retrieval",
                    "mode": "announce",
                    "catalog_size": loadable.len(),
                    "candidate_count": candidates.len(),
                    "selected_count": selected.len(),
                    "query_chars": query.encode_utf16().count(),
                    "matched": matched,
                }));
            }
            Some((
                content,
                crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure::Snapshot {
                    revision,
                    tools: loadable.clone(),
                },
            ))
        } else {
            let last = crate::tools::tool_select_state::last_announcement_disclosure(&history);
            let last_revision = last.as_ref().map(|disclosure| disclosure.revision());
            if last_revision.is_none_or(|last| last > revision) {
                self.legacy_disclosure_announcement(&state, &history, revision)
                    .or_else(|| {
                        let tools = state
                            .current
                            .iter()
                            .filter(|tool| {
                                self.is_dynamically_loadable(&state, tool)
                                    && self
                                        .tools_filter
                                        .as_ref()
                                        .is_none_or(|filter| filter.allows(&tool.name))
                            })
                            .cloned()
                            .collect::<Vec<_>>();
                        Some(Self::snapshot_disclosure_announcement(tools, revision))
                    })
            } else {
                let mut delta = None;
                for index in last_revision.unwrap()..revision {
                    let change = state.changes.get(index)?;
                    let added = change
                        .added
                        .iter()
                        .filter(|tool| self.is_dynamically_loadable(&state, tool))
                        .cloned()
                        .collect::<Vec<_>>();
                    let removed = change
                        .removed
                        .iter()
                        .filter(|tool| {
                            self.is_dynamically_loadable(&state, tool)
                                || !change.added.iter().any(|next| {
                                    next.name == tool.name
                                        && !self.is_dynamically_loadable(&state, next)
                                })
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if added.is_empty() && removed.is_empty() {
                        continue;
                    }
                    let has_loadable = state
                        .current
                        .iter()
                        .any(|tool| self.is_dynamically_loadable(&state, tool));
                    delta = Some(Self::delta_disclosure_announcement(
                        added,
                        removed,
                        has_loadable,
                        index + 1,
                    ));
                    break;
                }
                delta.or_else(|| self.legacy_disclosure_announcement(&state, &history, revision))
            }
        };
        let (content, disclosure) = result?;
        *self
            .tool_select_last_disclosure
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(serde_json::to_value(disclosure).ok()?);
        Some(content)
    }

    pub fn take_last_announcement_disclosure(&self) -> Option<serde_json::Value> {
        self.tool_select_last_disclosure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    fn ensure_tool_select_state_for_announcement(&self) {
        if self.session_store().is_some()
            || self
                .tool_select_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .initial
                .is_some()
        {
            return;
        }
        let active = self
            .mcp_manager
            .as_ref()
            .map(|mcp| {
                mcp.active_definitions_blocking()
                    .iter()
                    .map(crate::tools::tool_select_ops::tool_select_tool_from_definition)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let live = self
            .mcp_manager
            .as_ref()
            .map(|mcp| {
                mcp.live_definitions_blocking()
                    .iter()
                    .map(crate::tools::tool_select_ops::tool_select_tool_from_definition)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let notified = self
            .tool_select_notified_updates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let plan = crate::tools::tool_select_ops::plan_tool_select_reconcile(
            &crate::tools::tool_select_ops::ToolSelectModelState::default(),
            &self.effective_caller_agent_id(),
            &active,
            &live,
            &notified,
        );
        *self
            .tool_select_state
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = plan.state;
        self.tool_select_notified_updates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(plan.updates.iter().map(|update| update.signature.clone()));
        self.tool_select_pending_reminders
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .extend(plan.updates.iter().map(|update| {
                format!(
                    "Tool \"{}\" has an update, but the update is not applied in this session. Continue using the currently registered version.",
                    update.name
                )
            }));
    }

    pub(crate) fn is_dynamically_loadable(
        &self,
        state: &crate::tools::tool_select_ops::ToolSelectModelState,
        tool: &crate::tools::tool_select_ops::ToolSelectTool,
    ) -> bool {
        tool.disclosure == Some(crate::tools::registry::ToolDisclosure::Deferred)
            || (tool.origin == crate::tools::tool_select_ops::ToolSelectOrigin::Runtime
                && !state.initial.as_ref().is_some_and(|initial| {
                    initial.iter().any(|initial_tool| {
                        initial_tool.name == tool.name
                            && initial_tool.disclosure
                                != Some(crate::tools::registry::ToolDisclosure::Deferred)
                    })
                }))
    }

    pub(crate) fn loadable_tool_entries(
        &self,
        state: &crate::tools::tool_select_ops::ToolSelectModelState,
    ) -> Vec<crate::tools::tool_select_ops::ToolSelectTool> {
        state
            .current
            .iter()
            .filter(|tool| {
                self.is_dynamically_loadable(state, tool)
                    && self
                        .tools_filter
                        .as_ref()
                        .is_none_or(|filter| filter.allows(&tool.name))
            })
            .cloned()
            .collect()
    }

    fn snapshot_disclosure_announcement(
        mut tools: Vec<crate::tools::tool_select_ops::ToolSelectTool>,
        revision: usize,
    ) -> (
        String,
        crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure,
    ) {
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let names = tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let has_loadable = !names.is_empty();
        let content = crate::tools::select_tools::render_loadable_tools_announcement(
            &names,
            &[],
            has_loadable,
        )
        .expect("a snapshot always renders");
        (
            content,
            crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure::Snapshot {
                revision,
                tools,
            },
        )
    }

    fn delta_disclosure_announcement(
        added: Vec<crate::tools::tool_select_ops::ToolSelectTool>,
        removed: Vec<crate::tools::tool_select_ops::ToolSelectTool>,
        has_loadable: bool,
        revision: usize,
    ) -> (
        String,
        crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure,
    ) {
        let added_names = added
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let removed_names = removed
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        let content = crate::tools::select_tools::render_loadable_tools_announcement(
            &added_names,
            &removed_names,
            has_loadable,
        )
        .expect("a non-empty delta always renders");
        (
            content,
            crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure::Delta {
                revision,
                added,
                removed,
            },
        )
    }

    fn legacy_disclosure_announcement(
        &self,
        state: &crate::tools::tool_select_ops::ToolSelectModelState,
        history: &[crate::agent::context_memory::types::ContextMessage],
        revision: usize,
    ) -> Option<(
        String,
        crate::tools::tool_select_state::ToolSelectAnnouncementDisclosure,
    )> {
        let announced = crate::tools::tool_select_state::fold_announced_tool_names(history);
        let loadable = self.loadable_tool_entries(state);
        let loadable_names = loadable
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        let added = loadable
            .iter()
            .filter(|tool| !announced.contains(&tool.name))
            .cloned()
            .collect::<Vec<_>>();
        let known = state
            .current
            .iter()
            .chain(state.initial.iter().flatten())
            .chain(
                state
                    .changes
                    .iter()
                    .flat_map(|change| change.added.iter().chain(change.removed.iter())),
            )
            .collect::<Vec<_>>();
        let mut removed_names = announced
            .iter()
            .filter(|name| !loadable_names.contains(name.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        removed_names.sort();
        let removed = removed_names
            .iter()
            .filter_map(|name| {
                known
                    .iter()
                    .find(|tool| tool.name == *name)
                    .map(|tool| (*tool).clone())
            })
            .collect::<Vec<_>>();
        if !added.is_empty() || !removed.is_empty() {
            return Some(Self::delta_disclosure_announcement(
                added,
                removed,
                !loadable.is_empty(),
                revision,
            ));
        }
        if !loadable.is_empty() {
            return None;
        }
        let last = history.iter().rev().find(|message| {
            crate::tools::tool_select_state::is_loadable_tools_announcement(message)
        });
        if last.is_some_and(|message| {
            crate::tools::tool_select_state::message_text(message)
                .contains(crate::tools::select_tools::NO_LOADABLE_TOOLS_MESSAGE)
        }) {
            return None;
        }
        Some(Self::snapshot_disclosure_announcement(Vec::new(), revision))
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
    pub fn with_notify_user_enabled(mut self, enabled: bool) -> Self {
        self.notify_user_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_tower_max_running_tasks(mut self, max_running_tasks: Option<u32>) -> Self {
        self.tower_max_running_tasks = max_running_tasks.filter(|value| *value > 0);
        self
    }

    pub(super) fn shell_detach_timeout(&self) -> Option<Duration> {
        use crate::agent::task::print_defaults::MAX_TIMER_DELAY_MS;
        use crate::agent::tools::os::bash::bash::BASH_TASK_TIMEOUT_S_DEFAULT;
        use crate::agent::tools::os::shell::shell_tool_results::MS_PER_SECOND;

        let seconds = self
            .bash_task_timeout_s
            .unwrap_or(BASH_TASK_TIMEOUT_S_DEFAULT);
        (seconds > 0).then(|| {
            Duration::from_millis(seconds.saturating_mul(MS_PER_SECOND).min(MAX_TIMER_DELAY_MS))
        })
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
        runtime: Arc<crate::features::tower::wake::TowerWakeRuntime>,
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
        self.bind_tower_wake_runtime(crate::features::tower::wake::TowerWakeRuntime::new(driver))
    }

    pub(crate) fn tower_wake_runtime(
        &self,
    ) -> Option<Arc<crate::features::tower::wake::TowerWakeRuntime>> {
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

    pub fn with_non_interactive(mut self, non_interactive: bool) -> Self {
        self.non_interactive = non_interactive;
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

    pub fn set_llm_trace_id(&self, trace_id: Option<String>) {
        *self.llm_trace_id.lock().unwrap_or_else(|e| e.into_inner()) = trace_id;
    }

    pub fn llm_trace_id(&self) -> Option<String> {
        self.llm_trace_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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

    pub(super) fn operation_log_service(
        &self,
        agent_id: &str,
    ) -> operationService::AgentOperationService {
        self.operation_log.clone().with_identity(
            self.session_id.clone().unwrap_or_default(),
            agent_id.to_string(),
        )
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

    pub fn shape_tool_table_with_state(
        &self,
        tools: Vec<crate::turn_loop::types::ToolInfo>,
        state: &crate::tools::tool_select_ops::ToolSelectModelState,
    ) -> Vec<crate::turn_loop::types::ToolInfo> {
        use crate::tools::tool_select_ops::{ToolSelectOrigin, ToolSelectTool};

        let disclosure = self.supports_dynamic_tool_loading();
        let initial = state.initial.clone().unwrap_or_else(|| {
            tools
                .iter()
                .map(|tool| {
                    let source = if tool.name.starts_with("mcp__") {
                        crate::tools::tool_select_ops::ToolSelectSource::Mcp
                    } else if crate::tools::is_builtin_tool_name(&tool.name) {
                        crate::tools::tool_select_ops::ToolSelectSource::Builtin
                    } else {
                        crate::tools::tool_select_ops::ToolSelectSource::User
                    };
                    crate::tools::tool_select_ops::tool_select_tool_from_info(tool, source, None)
                })
                .collect()
        });
        let current_by_name = state
            .current
            .iter()
            .map(|tool| (tool.name.as_str(), tool))
            .collect::<std::collections::HashMap<_, _>>();
        let initial_names = initial
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut capable = initial
            .iter()
            .map(|tool| {
                current_by_name
                    .get(tool.name.as_str())
                    .map(|current| (*current).clone())
                    .unwrap_or_else(|| tool.clone())
            })
            .collect::<Vec<_>>();
        if disclosure {
            capable.extend(
                state
                    .current
                    .iter()
                    .filter(|tool| {
                        tool.origin == ToolSelectOrigin::Runtime
                            && !initial_names.contains(tool.name.as_str())
                    })
                    .cloned(),
            );
        }
        capable.sort_by(|left, right| left.name.cmp(&right.name));
        // v2 `shapeToolsForDisclosure` keeps a dynamically loadable entry once
        // it is loaded (or queued for loading): the deferred filter below drops
        // everything else. Without this the model is told a tool was loaded but
        // the next request's table omits its definition again.
        let loaded = {
            let mut loaded = self
                .loaded_tools
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            loaded.extend(
                self.tool_select_pending_loaded
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .iter()
                    .cloned(),
            );
            loaded
        };
        let is_dynamic = |tool: &ToolSelectTool| {
            tool.disclosure == Some(crate::tools::registry::ToolDisclosure::Deferred)
                || (tool.origin == ToolSelectOrigin::Runtime
                    && !initial.iter().any(|initial_tool| {
                        initial_tool.name == tool.name
                            && initial_tool.disclosure
                                != Some(crate::tools::registry::ToolDisclosure::Deferred)
                    }))
        };
        capable
            .into_iter()
            .filter(|tool| {
                let is_select = tool.name == crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME;
                let active = self.tools_filter.as_ref().is_none_or(|policy| {
                    if is_select && disclosure {
                        !policy.disabled.iter().any(|pattern| {
                            pattern.eq_ignore_ascii_case(
                                crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME,
                            )
                        })
                    } else {
                        policy.allows(&tool.name)
                    }
                });
                active && (disclosure || !is_select)
            })
            .filter(|tool| {
                !disclosure
                    || tool.name == crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME
                    || !is_dynamic(tool)
                    || loaded.contains(&tool.name)
            })
            .map(|tool| crate::turn_loop::types::ToolInfo {
                name: tool.name,
                description: tool.description,
                input_schema: serde_json::Value::Object(tool.parameters),
            })
            .collect()
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
                self.github_tools_available(),
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

    pub fn with_github_credentials(
        mut self,
        credentials: crate::agent::tools::github::github_legacy::GitHubCredentials,
    ) -> Self {
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

    pub fn github_credentials(
        &self,
    ) -> Option<&crate::agent::tools::github::github_legacy::GitHubCredentials> {
        self.github_credentials.as_ref()
    }

    pub(crate) fn github_tools_available(&self) -> bool {
        crate::agent::tools::github::github_legacy::has_token(self.github_credentials())
    }

    pub(crate) fn background_tasks_available(&self) -> bool {
        CALLER_BACKGROUND_TASKS_ALLOWED
            .try_with(|allowed| *allowed)
            .unwrap_or(true)
            && self
                .tools_filter
                .as_ref()
                .is_none_or(tool_policy::ToolsFilter::allows_background_tasks)
    }

    pub(crate) fn shell_background_available(&self) -> bool {
        !operationService::operation_log_enabled() && self.background_tasks_available()
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
