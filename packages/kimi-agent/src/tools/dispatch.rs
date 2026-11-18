use super::*;

use crate::features::operationLog::operationService;

fn operation_log_captures_tool(tool_name: &str) -> bool {
    let name = tool_name.to_ascii_lowercase().replace('_', "");
    !matches!(
        name.as_str(),
        "agent"
            | "agentswarm"
            | "websearch"
            | "knowledge"
            | "fetchurl"
            | "askuserquestion"
            | "waitfor"
            | "taskwait"
            | "taskstop"
            | "taskoutput"
            | "tasklist"
            | "selecttools"
            | "grep"
            | "read"
            | "glob"
            | "lsp"
            | "towerstatus"
            | "memorygraph"
            | "updategoal"
            | "setgoalbudget"
            | "repomap"
            | "todolist"
            | "getgoal"
            | "creategoal"
            | "cronlist"
            | "exitspecmode"
            | "crondelete"
            | "skill"
            | "croncreate"
            | "exitplanmode"
            | "snapshotlist"
            | "notifyuser"
            | "sessionquery"
            | "memory"
    )
}

impl NativeToolset {
    pub fn execute(&self, tool_name: &str, args: &Value) -> Option<ExecutableToolResult> {
        let sandbox = self.sandbox();
        match tool_name.to_ascii_lowercase().as_str() {
            "read" => self.read_media(args).or_else(|| {
                let event_log = self.wire_journal_read(args);
                Self::read(&sandbox, &self.shell_bridge, args, event_log)
            }),
            "grep" => Self::grep(&sandbox, &self.shell_bridge, args),
            "glob" => Self::glob(&sandbox, &self.shell_bridge, args),
            _ => None,
        }
    }

    fn wire_journal_read(&self, args: &Value) -> bool {
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return false;
        };
        let Some(store) = self.session_store() else {
            return false;
        };
        let Some(home) = store.path().and_then(|path| path.parent()) else {
            return false;
        };
        crate::session::wire_journal::is_wire_journal_path(home, path)
    }

    async fn attach_file_memory(
        &self,
        args: &Value,
        result: Option<ExecutableToolResult>,
    ) -> Option<ExecutableToolResult> {
        let result = result?;
        let Some(path) = args.get("path").and_then(Value::as_str) else {
            return Some(result);
        };
        Some(
            crate::agent::tools::file_memory::with_file_memory(
                result,
                path,
                self.file_memory.as_deref(),
            )
            .await,
        )
    }

    async fn repo_map_with(&self, args: &Value) -> Option<ExecutableToolResult> {
        let base = crate::tools::memory_store::memory_base()?;
        let service = crate::tools::git_memory::GitMemoryService::new(self.root.clone(), base);
        let path = args.get("path").and_then(Value::as_str).map(str::to_string);
        let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(3) as usize;
        let output = tokio::task::spawn_blocking(move || service.repo_map(path.as_deref(), depth))
            .await
            .ok()?;
        Some(if output.is_empty() {
            err_result(crate::tools::git_memory::REPO_MAP_EMPTY_OUTPUT.to_string())
        } else {
            ok_result(output)
        })
    }

    pub fn handles(&self, tool_name: &str) -> bool {
        is_native_tool_name(tool_name)
    }

    fn memory_graph_service(
        &self,
    ) -> Option<Arc<crate::features::associativeMemory::service::AssociativeMemoryService>> {
        let mut slot = self
            .memory_graph
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(service) = slot.as_ref() {
            return Some(service.clone());
        }
        if !crate::tools::memory_action::associative_memory_enabled() {
            return None;
        }
        let home = crate::workflow::kimi_home()?;
        let service = Arc::new(
            crate::features::associativeMemory::service::AssociativeMemoryService::new(
                crate::features::associativeMemory::service::MemoryDeps {
                    home_dir: home.clone(),
                    store_dir: home,
                    cwd: self.root.clone(),
                    session_id: self.session_id.clone().unwrap_or_default(),
                    session_cwd: self.root.clone(),
                    logger: None,
                    flags: Some(Arc::new(
                        crate::features::associativeMemory::service::EnvMemoryFlags,
                    )),
                    reminders: None,
                    history: None,
                    legacy_store: None,
                },
            ),
        );
        *slot = Some(service.clone());
        Some(service)
    }

    async fn tool_registration(&self, tool_name: &str) -> select_tools::ToolRegistration {
        if self.supports_dynamic_tool_loading() {
            let state = self
                .tool_select_state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if state.initial.is_some() {
                let Some(info) = state.current.iter().find(|tool| tool.name == tool_name) else {
                    return select_tools::ToolRegistration::Missing;
                };
                if info.source == crate::tools::tool_select_ops::ToolSelectSource::Mcp {
                    let available = match &self.mcp_manager {
                        Some(mcp) => mcp.is_tool_available(tool_name).await,
                        None => false,
                    };
                    if !available {
                        return select_tools::ToolRegistration::Missing;
                    }
                }
                return if self.is_dynamically_loadable(&state, info) {
                    select_tools::ToolRegistration::Deferred
                } else {
                    select_tools::ToolRegistration::Inline
                };
            }
        }
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
        if !operation_log_captures_tool(tool_name)
            || crate::agent::tools::github::github_legacy::is_github_tool(tool_name)
        {
            return self
                .execute_tool_streaming_inner(tool_call_id, tool_name, args, on_update)
                .await;
        }
        let caller = self.effective_caller_agent_id();
        let service = self.operation_log_service(&caller);
        let context = operationService::CaptureContext {
            description: crate::tools::tool_display::tool_call_description(
                tool_name,
                args,
                self.root.to_str(),
            )
            .unwrap_or_else(|| tool_name.to_string()),
            origin: operationService::CaptureOrigin {
                session_id: self.session_id.clone().unwrap_or_default(),
                agent_id: caller,
                turn_id: Some(self.effective_turn_id() as u64),
                tool_call_id: tool_call_id.map(str::to_string),
                tool_name: tool_name.to_string(),
            },
            repo_mutation: None,
        };
        let mut result = None;
        let captured = service
            .capture(&context, || async {
                result = self
                    .execute_tool_streaming_inner(tool_call_id, tool_name, args, on_update)
                    .await;
                Ok(())
            })
            .await;
        match captured {
            Ok(()) => result,
            Err(error) => Some(err_result(format!(
                "onExecuteTool hook failed for \"{tool_name}\": {}",
                error.message
            ))),
        }
    }

    async fn execute_tool_streaming_inner(
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

        if matches!(
            tool_name.to_ascii_lowercase().as_str(),
            "cronlist" | "cron_list" | "croncreate" | "cron_create" | "crondelete" | "cron_delete"
        ) && let Some(refusal) =
            crate::tools::goal_tools::main_agent_only_refusal(caller.as_str())
        {
            return Some(refusal);
        }

        let registration = self.tool_registration(tool_name).await;
        let was_loaded = self
            .loaded_tools
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(tool_name)
            || self
                .tool_select_pending_loaded
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
                let event_log = self.wire_journal_read(args);
                let result = match Self::spawn_file_tool_with_event_log(
                    self.sandbox(),
                    self.shell_bridge.clone(),
                    args.clone(),
                    event_log,
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => blocking_pool_failure(false, e.to_string()),
                };
                self.attach_file_memory(args, result).await
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
                let callbacks = self.callbacks.as_ref()?.clone();
                let background_allowed = self.tools_filter.as_ref().is_none_or(|filter| {
                    ["TaskList", "TaskOutput", "TaskStop"]
                        .iter()
                        .all(|name| filter.allows(name))
                });
                let caller = self.effective_caller_agent_id();
                let llm_trace_id = self.llm_trace_id();
                Some(
                    crate::agent::tools::ask_user_question::ask_user_question_tool::execute_ask_user_question(
                        callbacks,
                        self.task_runner.as_ref(),
                        self.session_id.as_deref(),
                        Some(caller.as_str()),
                        llm_trace_id.as_deref(),
                        self.effective_turn_id() as u64,
                        tool_call_id,
                        self.non_interactive,
                        background_allowed,
                        args,
                    )
                    .await,
                )
            }
            "getgoal" | "get_goal" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(get_goal::execute_get_goal(callbacks, caller.as_str(), args).await)
            }
            "todolist" | "todo_list" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(todo_list::execute_todo_list(callbacks, caller.as_str(), args).await)
            }
            "cronlist" | "cron_list" => {
                let callbacks = self.callbacks.as_deref()?;
                Some(cron_tools::execute_cron_list(callbacks, args).await)
            }
            "croncreate" | "cron_create" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(cron_tools::execute_cron_create(callbacks, args, caller.as_str()).await)
            }
            "crondelete" | "cron_delete" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                Some(cron_tools::execute_cron_delete(callbacks, args, caller.as_str()).await)
            }
            "updategoal" | "update_goal" => {
                let callbacks = self.callbacks.as_deref()?;
                let caller = self.effective_caller_agent_id();
                let result =
                    goal_tools::execute_update_goal(callbacks, caller.as_str(), args).await;
                if !result.is_error {
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
                let runner = self.task_runner.as_deref();
                Some(task_tools::execute_task_stop(callbacks, runner, args).await)
            }
            "taskwait" | "task_wait" | "waitfor" | "wait_for" => {
                if !task_tools::wait_for_enabled() {
                    return Some(err_result(
                        "WaitFor is disabled: the wait_for experimental flag is off.".into(),
                    ));
                }
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
                let mut telemetry_holder: Option<Box<dyn Fn(Value) + '_>> = None;
                if let Some(callbacks) = self.callbacks.as_ref() {
                    telemetry_holder = Some(Box::new(|event: Value| callbacks.telemetry(event)));
                }
                let wait = task_tools::WaitContext {
                    tally: &self.wait_tally,
                    turn_id: self.effective_turn_id(),
                    caller_agent_id: caller.as_str(),
                    display_name,
                    on_update: on_update.map(|emit| emit as &(dyn Fn(&crate::tools::ToolUpdate<'_>))),
                    telemetry: telemetry_holder.as_deref(),
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
                if let Some(title) = args.get("title") {
                    let Some(title) = title.as_str() else {
                        return Some(err_result("title must be a string.".to_string()));
                    };
                    if title.trim().is_empty() {
                        return Some(err_result("title must not be empty.".to_string()));
                    }
                    if title.contains('\n') || title.contains('\r') {
                        return Some(err_result("title must be a single line.".to_string()));
                    }
                }
                let message = args.get("message").and_then(Value::as_str).unwrap_or("");
                if message.trim().is_empty() {
                    return Some(err_result("message must not be empty.".to_string()));
                }
                let displayed = self.notify_user_enabled;
                let streak = CURRENT_CONVERSATION_HISTORY
                    .try_with(|slot| {
                        crate::injection::notify_user_nudge::notify_streak(
                            &slot.lock().unwrap_or_else(|error| error.into_inner()),
                        )
                    })
                    .ok();
                let rounds = streak.as_ref().map_or(0, |streak| streak.rounds);
                let after_nudge = streak.as_ref().is_some_and(|streak| streak.nudges > 0);
                if let Some(callbacks) = &self.callbacks {
                    callbacks.telemetry(serde_json::json!({
                        "event": "notify_user_sent",
                        "turn_id": args
                            .get("turn_id")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        "rounds_since_notify": rounds,
                        "after_nudge": after_nudge,
                        "title_chars": args
                            .get("title")
                            .and_then(Value::as_str)
                            .map(|title| title.encode_utf16().count())
                            .unwrap_or(0),
                        "message_chars": message.encode_utf16().count(),
                        "displayed": displayed,
                    }));
                }
                Some(ok_result(
                    if displayed {
                        "Update shown to the user."
                    } else {
                        "Notifications are disabled; the update was not displayed."
                    }
                    .to_string(),
                ))
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
                    let state = self
                        .tool_select_state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    let entries: Vec<crate::tools::tool_retriever::ToolSearchEntry> = if state
                        .initial
                        .is_some()
                    {
                        self.loadable_tool_entries(&state)
                            .into_iter()
                            .map(|tool| crate::tools::tool_retriever::ToolSearchEntry {
                                name: tool.name,
                                description: tool.description,
                                source: match tool.source {
                                    crate::tools::tool_select_ops::ToolSelectSource::Builtin => {
                                        "builtin"
                                    }
                                    crate::tools::tool_select_ops::ToolSelectSource::User => "user",
                                    crate::tools::tool_select_ops::ToolSelectSource::Mcp => "mcp",
                                }
                                .into(),
                                parameter_names: tool
                                    .parameters
                                    .get("properties")
                                    .and_then(Value::as_object)
                                    .map(|properties| properties.keys().cloned().collect())
                                    .unwrap_or_default(),
                            })
                            .collect()
                    } else {
                        self.mcp_manager
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
                            .unwrap_or_default()
                    };
                    let hits =
                        crate::tools::tool_retriever::rank_tools_by_query(&entries, query, 10);
                    if let Some(callbacks) = &self.callbacks {
                        callbacks.telemetry(serde_json::json!({
                            "event": "tool_retrieval",
                            "mode": "search",
                            "catalog_size": entries.len(),
                            "candidate_count": entries.len(),
                            "selected_count": hits.len(),
                            "query_chars": query.encode_utf16().count(),
                            "matched": !hits.is_empty(),
                        }));
                    }
                    return Some(select_tools::text_result(
                        &select_tools::search_output(query, &hits),
                        false,
                    ));
                }

                let state = self
                    .tool_select_state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let registered = if state.initial.is_some() {
                    state.current.iter().map(|tool| tool.name.clone()).collect()
                } else {
                    self.full_tool_catalogue().await
                };

                let loadable: std::collections::HashSet<String> = if state.initial.is_some() {
                    self.loadable_tool_entries(&state)
                        .into_iter()
                        .map(|tool| tool.name)
                        .collect()
                } else {
                    self.mcp_manager
                        .as_ref()
                        .map(|mcp| mcp.deferred_tool_names_blocking())
                        .unwrap_or_default()
                };

                let mut loaded = self
                    .loaded_tools
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let mut pending = self
                    .tool_select_pending_loaded
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                loaded.extend(pending.iter().cloned());

                let mut tracker = self
                    .selection_failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let environment = if state.initial.is_some() {
                    select_tools::selection_environment_for_tools(
                        self.tools_filter.as_ref(),
                        &state.current,
                    )
                } else {
                    select_tools::selection_environment_fingerprint(
                        self.tools_filter.as_ref(),
                        &registered,
                        &loadable,
                    )
                };
                tracker.reset_if_environment_changed(environment);
                Some(select_tools::execute_select_tools(
                    args,
                    &loadable,
                    &registered,
                    &loaded,
                    &mut pending,
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
            "repomap" | "repo_map" => self.repo_map_with(args).await,
            "knowledge" => Some(knowledge_tool::execute_knowledge(&self.root, args)),

            "memory" => {
                if caller != "main" {
                    return Some(err_result(memory_action::main_agent_only_message()));
                }
                let session_id = self.session_id.as_deref().unwrap_or_default();
                Some(memory_action::execute(&self.root, args, session_id))
            }

            "memorygraph" => {
                if caller != "main" {
                    return Some(err_result(memory_action::main_agent_only_message()));
                }
                let Some(service) = self.memory_graph_service() else {
                    return Some(err_result(memory_action::unavailable_message()));
                };
                Some(memory_graph_tool::execute_memory_graph(&service, args).await)
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
            "run_code" => Some(run_code::execute_run_code(args).await),
            "towerinit" | "tower_init" => {
                let caller = self.effective_caller_agent_id();
                let caller = caller.as_str();
                let session = self.session_id.as_deref().unwrap_or("session-main");
                let mut result = tower::execute_tower_init(
                    &self.root,
                    caller,
                    session,
                    &args.to_string(),
                    self.callbacks.as_deref(),
                )
                .await;

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
                        self.callbacks.as_deref(),
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
                        self.non_interactive,
                        self.permission_mode,
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
                        &args.to_string(),
                        self.task_runner.as_deref(),
                        self.tower_max_running_tasks,
                    )
                    .await,
                )
            }
            "write" => {
                let result = self
                    .run_mutating_file_tool_on_blocking_pool(args, Self::write)
                    .await;
                self.attach_file_memory(args, result).await
            }
            "edit" => {
                let result = self
                    .run_mutating_file_tool_on_blocking_pool(args, Self::edit)
                    .await;
                self.attach_file_memory(args, result).await
            }
            "bash" => self.bash_with(args, on_update, tool_call_id).await,
            "powershell" => self.powershell_with(args, on_update, tool_call_id).await,
            _ if crate::agent::tools::github::github_legacy::is_github_tool(tool_name) => {
                let cancel = self.effective_parent_cancel();
                crate::agent::tools::github::github_legacy::execute_github_tool(
                    tool_name,
                    args,
                    self.github_credentials.as_ref(),
                    cancel.as_ref(),
                )
                .await
            }
            _ => {
                if let Some(ref mcp) = self.mcp_manager
                    && (mcp.handles(tool_name).await || mcp.is_removed_tool(tool_name).await)
                {
                    let cancelled = self.effective_parent_cancel();
                    let probe = move || cancelled.as_ref().is_some_and(|cancel| cancel.triggered());
                    return mcp
                        .call_tool_streaming(tool_name, args, Some(&probe), on_update)
                        .await;
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
        self.execute_tool_streaming(None, tool_name, args, None)
            .await
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

    async fn spawn_file_tool_with_event_log(
        sandbox: Sandbox,
        bridge: Arc<ShellPathBridge>,
        args: Value,
        event_log: bool,
    ) -> Result<Option<ExecutableToolResult>, tokio::task::JoinError> {
        tokio::task::spawn_blocking(move || Self::read(&sandbox, &bridge, &args, event_log)).await
    }

    pub(super) async fn run_mutating_file_tool_on_blocking_pool(
        &self,
        args: &Value,
        tool: fn(&Sandbox, &ShellPathBridge, &Value) -> Option<ExecutableToolResult>,
    ) -> Option<ExecutableToolResult> {
        let ctx = if operationService::operation_log_enabled() {
            None
        } else {
            self.file_history
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        };
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
