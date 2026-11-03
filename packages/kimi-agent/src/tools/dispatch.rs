use super::*;

impl NativeToolset {
    /// Execute a read-only tool natively when supported and inside the
    /// sandbox. `None` means "not handled here — send it to the host".
    pub fn execute(&self, tool_name: &str, args: &Value) -> Option<ExecutableToolResult> {
        let sandbox = self.sandbox();
        match tool_name.to_ascii_lowercase().as_str() {
            "read" => self
                .read_media(args)
                .or_else(|| Self::read(&sandbox, &self.shell_bridge, args)),
            "grep" => Self::grep(&sandbox, &self.shell_bridge, args),
            "glob" => Self::glob(&sandbox, &self.shell_bridge, args),
            "listdirectory" | "list_directory" => list_directory::execute_list_directory(
                &self.root,
                &self.extra_roots,
                &self.shell_bridge,
                args,
            ),
            _ => None,
        }
    }

    /// Whether the sandbox knows how to execute this tool natively (subject
    /// to a host permission grant and sandbox confinement).
    pub fn handles(&self, tool_name: &str) -> bool {
        is_native_tool_name(tool_name)
    }

    /// Execute a tool natively (async).
    ///
    /// The file-I/O tools (`read` / `grep` / `glob` / `write` / `edit`)
    /// delegate to [`Self::run_readonly_file_tool_on_blocking_pool`] /
    /// [`Self::run_mutating_file_tool_on_blocking_pool`]: their bodies
    /// are synchronous syscalls, and with up to `MAX_PARALLEL_TOOLS` calls
    /// in flight per step they would otherwise pin tokio worker threads,
    /// starving the Bash output pumps, LLM streams, and steer queue on the
    /// same runtime.
    pub async fn execute_tool(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        self.execute_tool_ext(None, tool_name, args).await
    }

    /// [`Self::execute_tool`] with the caller's tool-call id, so the
    /// foreground `Agent` tool can key its lifecycle events onto the right
    /// transcript card (v2 `parentToolCallId`).
    pub async fn execute_tool_ext(
        &self,
        tool_call_id: Option<&str>,
        tool_name: &str,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        self.execute_tool_streaming(tool_call_id, tool_name, args, None)
            .await
    }

    /// [`Self::execute_tool_ext`] with a mid-execution output stream
    /// (P57): bash's stdout/stderr chunks are handed to `on_update` so the
    /// host can drive live `tool.progress` cards.
    pub async fn execute_tool_streaming(
        &self,
        tool_call_id: Option<&str>,
        tool_name: &str,
        args: &Value,
        on_update: Option<OutputUpdate<'_>>,
    ) -> Option<ExecutableToolResult> {
        // BTW side-channel veto (v2 `onBeforeExecuteTool` in `SessionBtwService`):
        // all tool calls from a side-channel agent are denied with TOOL_CALL_DISABLED_MESSAGE.
        let caller = self.effective_caller_agent_id();
        if let Some(denial) =
            crate::subagent::check_btw_tool_denial(Some(caller.as_str()), tool_name)
        {
            return Some(denial);
        }

        // Global `[tools]` switch (v2 tool policy): enforced again before
        // execution so a disabled tool is refused even when the model calls
        // it from memory instead of reading the advertised table.
        if let Some(filter) = &self.tools_filter
            && filter.blocks_call(tool_name)
        {
            return Some(ExecutableToolResult {
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.disabled",
                    i18n_params!["tool_name" => tool_name],
                )
                .render(),
                is_error: true,
                note: Some("tool_policy".into()),
                display: None,
            });
        }

        // Progressive tool disclosure (v2 `shouldIntercept`): with the
        // disclosure switch on, a call to a deferred MCP tool the model has
        // not loaded yet is refused with load guidance instead of executed —
        // the table hid it, so the call came from memory and running it would
        // defeat the point of keeping its schema out of the request.
        if self.tool_select_enabled
            && tool_name.starts_with("mcp__")
            && !tool_name.starts_with(crate::tools::select_tools::SELECT_TOOLS_TOOL_NAME)
        {
            let deferred = self
                .mcp_manager
                .as_ref()
                .map(|mcp| mcp.deferred_tool_names_blocking())
                .unwrap_or_default();
            let loaded_ok = self
                .loaded_tools
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(tool_name);
            if deferred.contains(tool_name) && !loaded_ok {
                return Some(ExecutableToolResult {
                    delivery: None,
                    stop_turn: false,
                    content: select_tools::not_loaded_tool_output(tool_name),
                    is_error: true,
                    note: Some("tool_select".into()),
                    display: None,
                });
            }
        }

        match tool_name.to_ascii_lowercase().as_str() {
            "read" => {
                // An image read is delivered to the model as media (v2
                // `executeMediaRead`), `region` / `full_resolution` included.
                // Text files and plain binary reads fall through to the text
                // read.
                if let Some(result) = self.read_media_on_blocking_pool(args).await {
                    return Some(result);
                }
                self.run_readonly_file_tool_on_blocking_pool(args, Self::read)
                    .await
            }
            "grep" => {
                self.run_readonly_file_tool_on_blocking_pool(args, Self::grep)
                    .await
            }
            "glob" => {
                self.run_readonly_file_tool_on_blocking_pool(args, Self::glob)
                    .await
            }
            "listdirectory" | "list_directory" => list_directory::execute_list_directory(
                &self.root,
                &self.extra_roots,
                &self.shell_bridge,
                args,
            ),
            "fetchurl" | "fetch_url" => fetch_url::execute_fetch_url(args, tool_call_id).await,
            "websearch" | "web_search" => web_search::execute_web_search(args, tool_call_id).await,
            "switchengine" | "switch_engine" | "switchsearchengine" => {
                web_search::execute_switch_engine(args).await
            }
            "lsp" => lsp_tool::execute_lsp_tool(&self.root, &self.shell_bridge, args).await,
            "invokesubagent" | "invoke_subagent" => {
                let mgr = self.subagent_manager.as_ref()?;
                Some(subagent_tools::execute_invoke_subagent(mgr, args).await)
            }
            "managesubagents" | "manage_subagents" => {
                let mgr = self.subagent_manager.as_deref()?;
                Some(subagent_tools::execute_manage_subagents(mgr, args).await)
            }
            "definesubagent" | "define_subagent" => {
                // Registering a subagent profile is a main-agent decision: a
                // profile is a standing grant of tools to future runs, and
                // `ToolPolicyFilter` reads an empty `tools` list as "everything
                // except the denylist". A subagent holding the unfiltered table
                // could therefore mint a full-permission profile for itself
                // and have the parent's own tool policy never re-checked. v2
                // scopes the tool to the main agent the same way (its
                // modeMutex and swarm gates are Agent-scoped).
                if self.effective_caller_agent_id() != "main" {
                    return Some(err_result(
                        "Only the main agent can register a subagent profile. \
                         Ask the main agent if you need one."
                            .to_string(),
                    ));
                }
                let mgr = self.subagent_manager.as_deref()?;
                Some(subagent_tools::execute_define_subagent(mgr, args).await)
            }
            "askuserquestion" | "ask_user_question" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(ask_user_question::execute_ask_user_question(callbacks, args).await)
            }
            "getgoal" | "get_goal" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(get_goal::execute_get_goal(callbacks, caller.as_str(), args).await)
            }
            "todolist" | "todo_list" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(todo_list::execute_todo_list(callbacks, args).await)
            }
            "enterplanmode" | "enter_plan_mode" => {
                let callbacks = self.callbacks.as_deref()?;
                let mut result = plan_mode::execute_enter_plan_mode(callbacks, args).await;
                // Mode mutex (v2 `PlanModeEnter` → tower exit), main agent
                // only: a subagent's plan entry is scoped to that subagent, so
                // it must not touch the main agent's tower (v2's modeMutex is
                // Agent-scoped and `tower.isActive` is main-only).
                if !result.is_error && self.effective_caller_agent_id() == "main" {
                    let paused =
                        mode_mutex::pause_tower_for_mode_enter(&self.root, "plan mode entered")
                            .await;
                    if !paused.is_empty() {
                        result
                            .content
                            .push_str(&mode_mutex::tower_paused_note(&paused));
                    }
                }
                Some(result)
            }
            "cronlist" | "cron_list" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(cron_tools::execute_cron_list(callbacks, args).await)
            }
            "croncreate" | "cron_create" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(cron_tools::execute_cron_create(callbacks, args).await)
            }
            "crondelete" | "cron_delete" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(cron_tools::execute_cron_delete(callbacks, args).await)
            }
            "updategoal" | "update_goal" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(goal_tools::execute_update_goal(callbacks, caller.as_str(), args).await)
            }
            "setgoalbudget" | "set_goal_budget" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(goal_tools::execute_set_goal_budget(callbacks, caller.as_str(), args).await)
            }
            "tasklist" | "task_list" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(task_tools::execute_task_list(callbacks, args).await)
            }
            "taskoutput" | "task_output" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(task_tools::execute_task_output(callbacks, args).await)
            }
            "taskstop" | "task_stop" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(task_tools::execute_task_stop(callbacks, args).await)
            }
            "taskwait" | "task_wait" | "waitfor" | "wait_for" => {
                let callbacks = self.callbacks.as_deref()?;
                let runner = self.task_runner.as_deref();
                let steer = self.effective_steer();
                Some(task_tools::execute_task_wait(callbacks, runner, steer.as_ref(), args).await)
            }
            "exitplanmode" | "exit_plan_mode" => {
                let callbacks = self.callbacks.as_deref()?;
                // The live session mode, not the mode in `config.toml`. Only
                // auto exits without asking, so the two must not be confused:
                // a session the user switched to `manual` this turn would
                // otherwise auto-exit through a stale config value.
                let mode = self.permission_mode.unwrap_or_else(|| {
                    crate::config::KimiConfig::discover()
                        .map(|(config, _)| config.build_policy_snapshot(None).mode)
                        // Fail-safe: an unapproved plan must never deactivate
                        // silently.
                        .unwrap_or(crate::permission::PermissionMode::Manual)
                });
                Some(exit_plan_mode::execute_exit_plan_mode_with_mode(callbacks, args, mode).await)
            }
            "creategoal" | "create_goal" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(create_goal::execute_create_goal(callbacks, caller.as_str(), args).await)
            }
            "skill" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(
                    skill::execute_skill(
                        callbacks,
                        self.session_id.as_deref(),
                        args,
                        skill::SkillScan {
                            root: Some(self.root.as_path()),
                            extra_dirs: &self.skill_dirs,
                            plugin_dirs: &self.plugin_skill_dirs,
                            merge_all_available_skills: self.merge_all_available_skills,
                        },
                    )
                    .await,
                )
            }
            "notifyuser" | "notify_user" => {
                let message = args.get("message").and_then(Value::as_str).unwrap_or("");
                if message.trim().is_empty() {
                    Some(ExecutableToolResult {
                        delivery: None,
                        stop_turn: false,
                        content: "message must not be empty.".to_string(),
                        is_error: true,
                        note: None,
                        display: None,
                    })
                } else {
                    Some(ExecutableToolResult {
                        delivery: None,
                        stop_turn: false,
                        content: "Update shown to the user.".to_string(),
                        is_error: false,
                        note: None,
                        display: None,
                    })
                }
            }
            "select_tools" | "selecttools" => {
                // A failed enumeration is not an empty catalogue. Collapsing it
                // to an empty set made every requested name look like a typo the
                // model had made, hiding the RPC failure behind "unknown tool".
                // The catalogue is the FULL set (advertised plus the deferred
                // tools the table hides — v2's registry universe), not the
                // shaped table `list_tools` serves.
                let registered = self.full_tool_catalogue().await;
                // v2 `loadableToolNames`: the deferred set — but only while
                // disclosure is on; with the switch off the deferred tools sit
                // in the advertised table and are called directly, so there is
                // nothing to load and they classify as statically callable.
                let loadable: std::collections::HashSet<String> = if self.tool_select_enabled {
                    self.mcp_manager
                        .as_ref()
                        .map(|mcp| mcp.deferred_tool_names_blocking())
                        .unwrap_or_default()
                } else {
                    Default::default()
                };
                // The loaded set is the toolset's shared, session-scoped one:
                // a fresh set per call made `already_available` unreachable and
                // reported every requested tool as newly loaded.
                let mut loaded = self.loaded_tools.lock().unwrap_or_else(|e| e.into_inner());
                // Upstream #4190: failure counts live as long as the catalogue
                // and policy that produced them; a change on either side
                // resets the streak.
                let mut tracker = self
                    .selection_failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                tracker.reset_if_environment_changed(
                    select_tools::selection_environment_fingerprint(
                        self.tools_filter.as_ref(),
                        &registered,
                        &loadable,
                    ),
                );
                Some(select_tools::execute_select_tools(
                    args,
                    &loadable,
                    &registered,
                    &mut loaded,
                    self.tools_filter.as_ref(),
                    &mut tracker,
                ))
            }
            "team" => {
                let mgr = self.subagent_manager.as_ref()?;
                let cancel = self.effective_parent_cancel();
                Some(team_tool::execute_team(mgr, args, cancel.as_ref()).await)
            }
            "agent" => {
                let mgr = self.subagent_manager.as_ref()?;
                agent_tool::execute_agent(
                    mgr,
                    // The toolset's canonical root feeds the tower resume
                    // gate's roster lookup (P2-2): the session's workspace,
                    // not the process cwd.
                    Some(&self.root),
                    args,
                    self.subagent_timeout_ms,
                    self.effective_parent_cancel().as_ref(),
                    tool_call_id,
                    self.secondary_model.as_deref(),
                )
                .await
            }
            "agentswarm" | "agent_swarm" => {
                // v2 #3976: swarm and tower modes are mutually exclusive — the
                // veto replaces the pre-#3976 auto-pause. With tower active
                // the fleet runs through TowerSpawn; a batch here would race
                // worker writes in the same repo. The gate runs before the
                // manager lookup, so a missing subagent runtime cannot turn a
                // refusal into a silent decline. Main agent only (v2 swarm
                // mode is Agent-scoped).
                if self.effective_caller_agent_id() == "main"
                    && let Some(denial) =
                        mode_mutex::refuse_swarm_with_active_tower(&self.root).await
                {
                    return Some(err_result(denial));
                }
                let mgr = self.subagent_manager.as_ref()?;
                swarm_tool::execute_agent_swarm(
                    mgr,
                    args,
                    self.subagent_timeout_ms,
                    self.effective_parent_cancel().as_ref(),
                    tool_call_id,
                    self.secondary_model.as_deref(),
                )
                .await
            }
            "knowledge" => Some(knowledge_tool::execute_knowledge(&self.root, args)),
            "memoryread" | "memory_read" => {
                Some(memory_tool::execute_memory_read(&self.root, args))
            }
            "memorywrite" | "memory_write" => {
                Some(memory_tool::execute_memory_write(&self.root, args))
            }
            "memorystrreplace" | "memory_str_replace" => {
                Some(memory_tool::execute_memory_str_replace(&self.root, args))
            }
            "memoryappend" | "memory_append" => {
                Some(memory_tool::execute_memory_append(&self.root, args))
            }
            "memorylist" | "memory_list" => {
                Some(memory_tool::execute_memory_list(&self.root, args))
            }
            "memorydelete" | "memory_delete" => {
                Some(memory_tool::execute_memory_delete(&self.root, args))
            }
            "workflow" => {
                let home = crate::workflow::kimi_home();
                let host: Option<std::sync::Arc<dyn crate::workflow::WorkflowHost>> =
                    self.subagent_manager.clone().map(|manager| {
                        // `search()` backend: without a provider the workflow
                        // primitive returned an empty list with no error, so a
                        // script's web lookup silently found nothing. Bound to
                        // the native DuckDuckGo search (the same engine the
                        // WebSearch tool uses). The host trait takes a sync
                        // provider, so this call blocks its executor thread for
                        // the search timeout — bounded by `timeout_ms` and only
                        // reached from a workflow script.
                        let search: std::sync::Arc<crate::workflow::host::SearchProvider> =
                            std::sync::Arc::new(|query: String, count: usize| {
                                let config = crate::native::web_search::WebSearchConfig {
                                    query,
                                    max_results: count.clamp(1, 10),
                                    ..Default::default()
                                };
                                crate::native::web_search::web_search(&config)
                                    .results
                                    .into_iter()
                                    .map(|hit| crate::workflow::SearchHit {
                                        title: hit.title,
                                        url: hit.url,
                                        snippet: hit.snippet,
                                    })
                                    .collect()
                            });
                        std::sync::Arc::new(
                            crate::workflow::host::SubagentWorkflowHost::new(
                                manager,
                                self.root.clone(),
                                home.clone(),
                            )
                            .with_search(search),
                        )
                            as std::sync::Arc<dyn crate::workflow::WorkflowHost>
                    });
                let outcome = crate::tools::workflow::run_workflow_tool(
                    crate::workflow::global_service(),
                    host,
                    home.as_deref(),
                    args,
                )
                .await;
                Some(ExecutableToolResult {
                    delivery: None,
                    stop_turn: outcome.stop_turn,
                    content: outcome.content,
                    is_error: outcome.is_error,
                    note: None,
                    display: None,
                })
            }
            "towerinit" | "tower_init" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let session = self.session_id.as_deref().unwrap_or("session-main");
                let mut result =
                    tower::execute_tower_init(&self.root, caller, session, &args.to_string()).await;
                // Mode mutex (v2 `TowerModeEnter`): a tower starting while plan
                // mode or swarm mode is open would split the brain — main turns
                // stay guarded by the old mode while workers run free — so exit
                // them once the tower actually entered. Ordered after init
                // because v2 exits on the entry event: a refused or failed init
                // never entered tower mode, so it must not drop either mode.
                // Main agent only, matching v2's Agent-scoped mutex.
                if caller == "main"
                    && !result.is_error
                    && let Some(callbacks) = self.callbacks.as_deref()
                {
                    if mode_mutex::exit_plan_for_tower_enter(callbacks).await {
                        result.content =
                            format!("{}\n\n{}", mode_mutex::plan_exited_note(), result.content);
                    }
                    if mode_mutex::exit_swarm_for_tower_enter(callbacks, caller) {
                        result.content =
                            format!("{}\n\n{}", mode_mutex::swarm_exited_note(), result.content);
                    }
                }
                Some(result)
            }
            "towerplan" | "tower_plan" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(tower::execute_tower_plan(&self.root, caller, &args.to_string()).await)
            }
            "towerspawn" | "tower_spawn" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let session = self.session_id.as_deref().unwrap_or("session-main");
                tower::execute_tower_spawn(
                    &self.root,
                    caller,
                    session,
                    self.subagent_manager.as_ref(),
                    tool_call_id,
                    self.effective_parent_cancel().as_ref(),
                    &args.to_string(),
                )
                .await
            }
            "towermerge" | "tower_merge" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(tower::execute_tower_merge(&self.root, caller, &args.to_string()).await)
            }
            "towerteardown" | "tower_teardown" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let session = self.session_id.as_deref().unwrap_or("session-main");
                Some(
                    tower::execute_tower_teardown(
                        &self.root,
                        caller,
                        session,
                        &args.to_string(),
                        self.task_runner.as_deref(),
                    )
                    .await,
                )
            }
            "towersend" | "tower_send" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                // Upstream #3847: the sender's cumulative token total rides
                // the record (v2 `callerTokens`).
                let tokens = self.caller_tokens(caller).await;
                Some(
                    tower::execute_tower_send(
                        &self.root,
                        caller,
                        &args.to_string(),
                        self.task_runner.as_deref(),
                        tokens,
                    )
                    .await,
                )
            }
            "towerinbox" | "tower_inbox" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(tower::execute_tower_inbox(&self.root, caller, &args.to_string()).await)
            }
            "towerfinding" | "tower_finding" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                // Upstream #3847: the filer's cumulative token total rides
                // the record (v2 `callerTokens`).
                let tokens = self.caller_tokens(caller).await;
                Some(
                    tower::execute_tower_finding(&self.root, caller, &args.to_string(), tokens)
                        .await,
                )
            }
            "towerreview" | "tower_review" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                // Upstream #3847: the reviewer's cumulative token total rides
                // the record (v2 `callerTokens`).
                let tokens = self.caller_tokens(caller).await;
                Some(
                    tower::execute_tower_review(&self.root, caller, &args.to_string(), tokens)
                        .await,
                )
            }
            "towermission" | "tower_mission" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(tower::execute_tower_mission(&self.root, caller, &args.to_string()).await)
            }
            "towerstatus" | "tower_status" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(tower::execute_tower_status(&self.root, caller).await)
            }
            "write" => {
                self.run_mutating_file_tool_on_blocking_pool(args, Self::write)
                    .await
            }
            "edit" => {
                self.run_mutating_file_tool_on_blocking_pool(args, Self::edit)
                    .await
            }
            "bash" => self.bash_with(args, on_update).await,
            _ if github::is_github_tool(tool_name) => {
                github::execute_github_tool(tool_name, args, self.github_credentials.as_ref()).await
            }
            _ => {
                if let Some(ref mcp) = self.mcp_manager
                    && mcp.handles(tool_name).await
                {
                    // The conversion walks every block and can preserve
                    // megabytes of media: a turn that was aborted must stop it
                    // (v2 passes its AbortSignal into the same conversion).
                    let cancelled = self.effective_parent_cancel();
                    let probe = move || cancelled.as_ref().is_some_and(|cancel| cancel.triggered());
                    return mcp.call_tool(tool_name, args, Some(&probe)).await;
                }
                None
            }
        }
    }

    /// Execute a mutating tool natively. Callers must have obtained a
    /// permission grant from the host for this exact call first.
    pub async fn execute_mutating(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        match tool_name.to_ascii_lowercase().as_str() {
            "write" => {
                self.run_mutating_file_tool_on_blocking_pool(args, Self::write)
                    .await
            }
            "edit" => {
                self.run_mutating_file_tool_on_blocking_pool(args, Self::edit)
                    .await
            }
            "bash" => self.bash_with(args, None).await,
            _ => None,
        }
    }

    /// Run a synchronous read-only file-I/O tool (`read` / `grep` / `glob`) on
    /// tokio's blocking pool instead of the async worker thread. The closure
    /// must be `Send + 'static`, so only owned state crosses over: a clone of
    /// the sandbox root and of the JSON arguments (both cheap relative to the
    /// I/O the tool does). No other `NativeToolset` field is needed — these
    /// tools depend solely on the root.
    ///
    /// A `JoinError` (panic / runtime shutdown) returns `None`, which hands the
    /// call to the host: read-only tools are idempotent, so re-running one
    /// there is always safe, and a task lost to the runtime must not reach the
    /// model as a tool failure.
    pub(super) async fn run_readonly_file_tool_on_blocking_pool(
        &self,
        args: &Value,
        tool: fn(&Sandbox, &ShellPathBridge, &Value) -> Option<ExecutableToolResult>,
    ) -> Option<ExecutableToolResult> {
        match Self::spawn_file_tool(
            self.sandbox(),
            self.shell_bridge.clone(),
            args.clone(),
            tool,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => blocking_pool_failure(false, e.to_string()),
        }
    }

    /// The native Read media path (v2 `executeMediaRead`): an image the model
    /// asked to read is delivered to the conversation as media, honoring
    /// `region` and `full_resolution`. `None` = not a media read (a text file
    /// without those args), so the caller runs the text read instead.
    pub(super) fn read_media(&self, args: &Value) -> Option<ExecutableToolResult> {
        let request = match read_media::ReadMediaRequest::from_args(args) {
            Ok(request) => request,
            Err(message) => return Some(err_result(message)),
        };
        let path = args.get("path")?.as_str()?;
        let resolved = Self::resolve(&self.sandbox(), &self.shell_bridge, path)?;
        read_media::read_image_media(&resolved, &request, &self.read_media_limits())
    }

    /// [`Self::read_media`] on the blocking pool: decoding, cropping and
    /// re-encoding a large image would otherwise pin the tokio workers that
    /// carry the LLM streams, bash output pumps and steer queue.
    pub(super) async fn read_media_on_blocking_pool(
        &self,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        let request = match read_media::ReadMediaRequest::from_args(args) {
            Ok(request) => request,
            Err(message) => return Some(err_result(message)),
        };
        let path = args.get("path")?.as_str()?;
        let resolved = Self::resolve(&self.sandbox(), &self.shell_bridge, path)?;
        let limits = self.read_media_limits();
        match tokio::task::spawn_blocking(move || {
            read_media::read_image_media(&resolved, &request, &limits)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => blocking_pool_failure(false, error.to_string()),
        }
    }

    pub(super) async fn spawn_file_tool(
        sandbox: Sandbox,
        bridge: Arc<ShellPathBridge>,
        args: Value,
        tool: fn(&Sandbox, &ShellPathBridge, &Value) -> Option<ExecutableToolResult>,
    ) -> Result<Option<ExecutableToolResult>, tokio::task::JoinError> {
        tokio::task::spawn_blocking(move || tool(&sandbox, &bridge, &args)).await
    }

    /// Run a synchronous mutating file-I/O tool (`write` / `edit`) on tokio's
    /// blocking pool. Same ownership rules as the read-only variant, but a
    /// `JoinError` becomes an error result rather than a `None` host fallback:
    /// the call was already permission-granted and may have partially applied,
    /// so letting the host re-run it could double-apply the change.
    pub(super) async fn run_mutating_file_tool_on_blocking_pool(
        &self,
        args: &Value,
        tool: fn(&Sandbox, &ShellPathBridge, &Value) -> Option<ExecutableToolResult>,
    ) -> Option<ExecutableToolResult> {
        let ctx = self
            .file_history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let turn_id = self.effective_turn_id();
        match Self::spawn_mutating_file_tool(
            self.sandbox(),
            self.shell_bridge.clone(),
            args.clone(),
            tool,
            ctx,
            turn_id,
        )
        .await
        {
            Ok(result) => result,
            Err(e) => blocking_pool_failure(true, e.to_string()),
        }
    }

    pub(super) async fn spawn_mutating_file_tool(
        sandbox: Sandbox,
        bridge: Arc<ShellPathBridge>,
        args: Value,
        tool: fn(&Sandbox, &ShellPathBridge, &Value) -> Option<ExecutableToolResult>,
        ctx: Option<FileHistoryCtx>,
        turn_id: usize,
    ) -> Result<Option<ExecutableToolResult>, tokio::task::JoinError> {
        tokio::task::spawn_blocking(move || {
            // Install the recorder and the turn number for the duration of this
            // blocking call; `write` / `edit` read both back out to record the
            // change. Thread-locals must be set *here* — the blocking pool runs
            // on another thread, so a value installed on the async thread would
            // be invisible. `ctx` is `None` for callers without
            // `with_file_history`, in which case nothing is recorded.
            FILE_HISTORY.with(|cell| *cell.borrow_mut() = ctx);
            TURN_ID.with(|cell| *cell.borrow_mut() = turn_id);
            let result = tool(&sandbox, &bridge, &args);
            FILE_HISTORY.with(|cell| *cell.borrow_mut() = None);
            TURN_ID.with(|cell| *cell.borrow_mut() = 0);
            result
        })
        .await
    }
}
