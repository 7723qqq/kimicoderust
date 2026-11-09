use super::*;

impl NativeToolset {
    pub fn execute(&self, tool_name: &str, args: &Value) -> Option<ExecutableToolResult> {
        let sandbox = self.sandbox();
        match tool_name.to_ascii_lowercase().as_str() {
            "read" => self
                .read_media(args)
                .or_else(|| Self::read(&sandbox, &self.shell_bridge, args)),
            "grep" => Self::grep(&sandbox, &self.shell_bridge, args),
            "glob" => Self::glob(&sandbox, &self.shell_bridge, args),
            _ => None,
        }
    }

    pub fn handles(&self, tool_name: &str) -> bool {
        is_native_tool_name(tool_name)
    }

    async fn tool_registration(&self, tool_name: &str) -> select_tools::ToolRegistration {
        if is_builtin_tool_name(tool_name) {
            return select_tools::ToolRegistration::Inline;
        }
        match self.mcp_manager.as_ref() {
            Some(mcp) if mcp.is_tool_available(tool_name).await => {
                if mcp.deferred_tool_names_blocking().contains(tool_name) {
                    select_tools::ToolRegistration::Deferred
                } else {
                    select_tools::ToolRegistration::Inline
                }
            }
            _ => select_tools::ToolRegistration::Missing,
        }
    }

    pub async fn execute_tool(
        &self,
        tool_name: &str,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        self.execute_tool_ext(None, tool_name, args).await
    }

    pub async fn execute_tool_ext(
        &self,
        tool_call_id: Option<&str>,
        tool_name: &str,
        args: &Value,
    ) -> Option<ExecutableToolResult> {
        self.execute_tool_streaming(tool_call_id, tool_name, args, None)
            .await
    }

    pub async fn execute_tool_streaming(
        &self,
        tool_call_id: Option<&str>,
        tool_name: &str,
        args: &Value,
        on_update: Option<OutputUpdate<'_>>,
    ) -> Option<ExecutableToolResult> {
        let caller = self.effective_caller_agent_id();
        if let Some(denial) =
            crate::subagent::check_btw_tool_denial(Some(caller.as_str()), tool_name)
        {
            return Some(denial);
        }

        let registration = self.tool_registration(tool_name).await;
        let was_loaded = self
            .loaded_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(tool_name);

        if let Some(refusal) = select_tools::missing_refusal(
            self.supports_dynamic_tool_loading(),
            tool_name,
            registration,
            was_loaded,
        ) {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: refusal,
                is_error: true,
                note: None,
                display: None,
            });
        }

        if let Some(filter) = &self.tools_filter
            && filter.blocks_call(tool_name)
        {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: LocalizedText::with_params(
                    "engine.tools.disabled",
                    i18n_params!["tool_name" => tool_name],
                )
                .render(),
                is_error: true,
                note: None,
                display: None,
            });
        }

        if let Some(refusal) = select_tools::unavailable_refusal(
            self.supports_dynamic_tool_loading(),
            tool_name,
            registration,
            was_loaded,
        ) {
            return Some(ExecutableToolResult {
                spill: None,
                parts: Vec::new(),
                delivery: None,
                stop_turn: false,
                content: refusal,
                is_error: true,
                note: None,
                display: None,
            });
        }

        match tool_name.to_ascii_lowercase().as_str() {
            "read" => {
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
            "fetchurl" | "fetch_url" => fetch_url::execute_fetch_url(args, tool_call_id).await,
            "websearch" | "web_search" => web_search::execute_web_search(args, tool_call_id).await,
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
                let result =
                    goal_tools::execute_update_goal(callbacks, caller.as_str(), args).await;
                if !result.is_error {
                    // v2's `GoalUpdated` handler: a goal that (re)becomes active
                    // evicts an active tower (`tower.exit('mode-mutex')`).
                    let _ =
                        mode_mutex::evict_tower_for_goal_activation(callbacks, &self.root).await;
                }
                Some(result)
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

                let display_name = if tool_name.eq_ignore_ascii_case("waitfor")
                    || tool_name.eq_ignore_ascii_case("wait_for")
                {
                    "WaitFor"
                } else {
                    "TaskWait"
                };
                let wait = task_tools::WaitContext {
                    tally: &self.wait_tally,
                    turn_id: self.effective_turn_id(),
                    caller_agent_id: caller.as_str(),
                    display_name,
                };
                Some(
                    task_tools::execute_task_wait(callbacks, runner, steer.as_ref(), &wait, args)
                        .await,
                )
            }
            "exitplanmode" | "exit_plan_mode" => {
                let callbacks = self.callbacks.as_deref()?;

                let mode = self.permission_mode.unwrap_or_else(|| {
                    crate::config::KimiConfig::discover()
                        .map(|(config, _)| config.build_policy_snapshot(None).mode)
                        .unwrap_or(crate::permission::PermissionMode::Manual)
                });
                Some(exit_plan_mode::execute_exit_plan_mode_with_mode(callbacks, args, mode).await)
            }
            "enterspecmode" | "enter_spec_mode" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(
                    spec_mode::execute_enter_spec_mode(
                        callbacks,
                        &self.root,
                        caller.as_str(),
                        args,
                    )
                    .await,
                )
            }
            "exitspecmode" | "exit_spec_mode" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                let mode = self.permission_mode.unwrap_or_else(|| {
                    crate::config::KimiConfig::discover()
                        .map(|(config, _)| config.build_policy_snapshot(None).mode)
                        .unwrap_or(crate::permission::PermissionMode::Manual)
                });
                Some(
                    spec_mode::execute_exit_spec_mode(
                        callbacks,
                        &self.root,
                        caller.as_str(),
                        args,
                        mode,
                    )
                    .await,
                )
            }
            "creategoal" | "create_goal" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                let result =
                    create_goal::execute_create_goal(callbacks, caller.as_str(), args).await;
                if !result.is_error {
                    let _ =
                        mode_mutex::evict_tower_for_goal_activation(callbacks, &self.root).await;
                }
                Some(result)
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
                        spill: None,
                        parts: Vec::new(),
                        delivery: None,
                        stop_turn: false,
                        content: "message must not be empty.".to_string(),
                        is_error: true,
                        note: None,
                        display: None,
                    })
                } else {
                    Some(ExecutableToolResult {
                        spill: None,
                        parts: Vec::new(),
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
                if !self.supports_dynamic_tool_loading() {
                    return Some(select_tools::text_result(
                        select_tools::SELECT_TOOLS_UNAVAILABLE,
                        true,
                    ));
                }
                if args.get("names").is_none() {
                    let Some(query) = args.get("query").and_then(Value::as_str) else {
                        return Some(select_tools::text_result(
                            select_tools::SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY,
                            true,
                        ));
                    };
                    if query.is_empty() {
                        return Some(select_tools::text_result(
                            select_tools::SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY,
                            true,
                        ));
                    }
                    let entries: Vec<crate::tools::tool_retriever::ToolSearchEntry> = self
                        .mcp_manager
                        .as_ref()
                        .map(|mcp| {
                            mcp.deferred_tool_infos_blocking()
                                .into_iter()
                                .map(|(name, description, schema)| {
                                    let parameter_names = schema
                                        .get("properties")
                                        .and_then(Value::as_object)
                                        .map(|properties| properties.keys().cloned().collect())
                                        .unwrap_or_default();
                                    crate::tools::tool_retriever::ToolSearchEntry {
                                        name,
                                        description,
                                        source: "mcp".into(),
                                        parameter_names,
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let hits =
                        crate::tools::tool_retriever::rank_tools_by_query(&entries, query, 10);
                    return Some(select_tools::text_result(
                        &select_tools::search_output(query, &hits),
                        false,
                    ));
                }

                let registered = self.full_tool_catalogue().await;

                let loadable: std::collections::HashSet<String> = self
                    .mcp_manager
                    .as_ref()
                    .map(|mcp| mcp.deferred_tool_names_blocking())
                    .unwrap_or_default();

                let mut loaded = self.loaded_tools.lock().unwrap_or_else(|e| e.into_inner());

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
            "agent" => {
                let mgr = self.subagent_manager.as_ref()?;
                agent_tool::execute_agent(
                    mgr,
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

            "memory" => {
                if caller != "main" {
                    return Some(err_result(memory_action::main_agent_only_message()));
                }
                let session_id = self.session_id.as_deref().unwrap_or_default();
                Some(memory_action::execute(&self.root, args, session_id))
            }

            "sessionquery" | "session_query" => {
                if caller != "main" {
                    return Some(err_result(
                        "The session_query tool is available to the main agent only.".to_string(),
                    ));
                }
                Some(
                    session_query::execute_session_query(
                        self.session_store(),
                        self.session_id.as_deref(),
                        &self.root,
                        args,
                    )
                    .await,
                )
            }
            "workflow" => {
                let home = crate::workflow::kimi_home();
                let host: Option<std::sync::Arc<dyn crate::workflow::WorkflowHost>> =
                    self.subagent_manager.clone().map(|manager| {
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
                    spill: None,
                    parts: Vec::new(),
                    delivery: None,
                    stop_turn: outcome.stop_turn,
                    content: outcome.content,
                    is_error: outcome.is_error,
                    note: None,
                    display: None,
                })
            }
            "run_code" | "runcode" => Some(run_code::execute_run_code(args).await),
            "towerinit" | "tower_init" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let session = self.session_id.as_deref().unwrap_or("session-main");
                let mut result =
                    tower::execute_tower_init(&self.root, caller, session, &args.to_string()).await;

                if caller == "main"
                    && !result.is_error
                    && let Some(callbacks) = self.callbacks.as_deref()
                {
                    let _switch = mode_mutex::mode_switch_lock().lock().await;
                    if mode_mutex::exit_plan_for_tower_enter(callbacks).await {
                        result.content =
                            format!("{}\n\n{}", mode_mutex::plan_exited_note(), result.content);
                    }
                    if mode_mutex::exit_swarm_for_tower_enter(callbacks, caller) {
                        result.content =
                            format!("{}\n\n{}", mode_mutex::swarm_exited_note(), result.content);
                    }
                    match mode_mutex::exit_spec_for_mode_enter(callbacks, caller).await {
                        Ok(true) => {
                            result.content = format!(
                                "{}\n\n{}",
                                mode_mutex::spec_exited_for_tower_note(),
                                result.content
                            );
                        }
                        Ok(false) => {}
                        Err(error) => {
                            result.content = format!(
                                "{}\n\nMode mutex: spec mode could not be exited ({error}); treat spec mode as still active.",
                                result.content
                            );
                        }
                    }
                    // v2's `TowerModeEnter` handler: an active goal is paused so
                    // goal continuations cannot race the tower fleet.
                    mode_mutex::pause_goal_for_tower_enter(callbacks).await;
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
                    self.subagent_manager.as_ref().map(|manager| {
                        (
                            manager,
                            self.secondary_model.as_deref(),
                            self.tower_max_running_tasks,
                        )
                    }),
                    tool_call_id,
                    self.effective_parent_cancel().as_ref(),
                    &args.to_string(),
                )
                .await
            }
            "towerrebase" | "tower_rebase" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(
                    tower::execute_tower_rebase(
                        &self.root,
                        caller,
                        &args.to_string(),
                        self.task_runner.as_deref(),
                    )
                    .await,
                )
            }
            "towermerge" | "tower_merge" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                Some(
                    tower::execute_tower_merge(
                        &self.root,
                        caller,
                        &args.to_string(),
                        self.task_runner.as_deref(),
                    )
                    .await,
                )
            }
            "towerteardown" | "tower_teardown" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let session = self.session_id.as_deref().unwrap_or("session-main");
                let wake_runtime = self.tower_wake_runtime();
                Some(
                    tower::execute_tower_teardown(
                        &self.root,
                        caller,
                        session,
                        &args.to_string(),
                        self.task_runner.as_deref(),
                        wake_runtime.as_ref(),
                    )
                    .await,
                )
            }
            "towersend" | "tower_send" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let wake_runtime = self.tower_wake_runtime();
                Some(
                    tower::execute_tower_send(
                        &self.root,
                        caller,
                        &args.to_string(),
                        self.task_runner.as_ref(),
                        wake_runtime.as_ref(),
                    )
                    .await,
                )
            }
            "towercomplete" | "tower_complete" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let wake_runtime = self.tower_wake_runtime();
                Some(
                    tower::execute_tower_complete(
                        &self.root,
                        caller,
                        &args.to_string(),
                        self.task_runner.as_ref(),
                        wake_runtime.as_ref(),
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
                Some(tower::execute_tower_finding(&self.root, caller, &args.to_string()).await)
            }
            "towerreview" | "tower_review" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let wake_runtime = self.tower_wake_runtime();
                Some(
                    tower::execute_tower_review(
                        &self.root,
                        caller,
                        &args.to_string(),
                        self.task_runner.as_ref(),
                        wake_runtime.as_ref(),
                    )
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
                Some(
                    tower::execute_tower_status(
                        &self.root,
                        caller,
                        self.task_runner.as_deref(),
                        self.tower_max_running_tasks,
                    )
                    .await,
                )
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
            "powershell" => self.powershell_with(args, on_update).await,
            _ if github::is_github_tool(tool_name) => {
                github::execute_github_tool(tool_name, args, self.github_credentials.as_ref()).await
            }
            _ => {
                if let Some(ref mcp) = self.mcp_manager
                    && mcp.handles(tool_name).await
                {
                    let cancelled = self.effective_parent_cancel();
                    let probe = move || cancelled.as_ref().is_some_and(|cancel| cancel.triggered());
                    return mcp.call_tool(tool_name, args, Some(&probe)).await;
                }
                None
            }
        }
    }

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
            "powershell" => self.powershell_with(args, None).await,
            _ => None,
        }
    }

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

    pub(super) fn read_media(&self, args: &Value) -> Option<ExecutableToolResult> {
        let request = match read_media::ReadMediaRequest::from_args(args) {
            Ok(request) => request,
            Err(message) => return Some(err_result(message)),
        };
        let path = args.get("path")?.as_str()?;
        let resolved = Self::resolve(&self.sandbox(), &self.shell_bridge, path)?;
        read_media::read_image_media(&resolved, &request, &self.read_media_limits())
    }

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
