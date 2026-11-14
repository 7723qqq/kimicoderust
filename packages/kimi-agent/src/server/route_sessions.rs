use super::catalog_routes::format_wire_session;
use super::*;

fn session_not_found(session_id: &str) -> HttpResponse {
    HttpResponse::json(
        404,
        &json!({
            "code": crate::server::envelope::error_codes::SESSION_NOT_FOUND,
            "msg": format!("Session \"{session_id}\" not found"),
        }),
    )
}

impl HttpServer {
    pub(super) async fn route_sessions(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("POST", p) if extract_session_action(p, "init").is_some() => {
                let session_id = extract_session_action(p, "init").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let prompt = crate::prompt::DEFAULT_INIT_PROMPT;
                let Some(engine) = self.engine.as_ref() else {
                    return HttpResponse::ok(&json!({
                        "status": "initiated",
                        "sessionId": session_id,
                        "prompt": prompt,
                    }));
                };
                let history = match self.store.load_session_history(session_id) {
                    Ok(messages) => messages,
                    Err(error) => {
                        self.emit_session_telemetry(
                            "session_load_failed",
                            serde_json::json!({
                                "reason": error.to_string(),
                            }),
                        );
                        return HttpResponse::internal_error(error.to_string());
                    }
                };
                let turn_number = match self.store.next_turn_number(session_id) {
                    Ok(number) => number,
                    Err(error) => return HttpResponse::internal_error(error.to_string()),
                };
                match engine
                    .run_turn(session_id, turn_number, history, prompt)
                    .await
                {
                    Ok(report) => HttpResponse::ok(&json!({
                        "status": "completed",
                        "sessionId": session_id,
                        "turnId": report.turn_id,
                        "turnNumber": turn_number,
                        "stopReason": report.stop_reason,
                        "content": report.reply,
                        "steps": report.steps,
                        "llmTransport": report.llm_transport,
                        "eventsEmitted": report.events_emitted,
                        "nativeToolCalls": report.native_tool_calls,
                        "usage": report.usage,
                        "prompt": prompt,
                    })),
                    Err(error) => match error {
                        crate::server::engine::EngineError::Occupied(message) => {
                            HttpResponse::json(
                                409,
                                &json!({
                                    "code": crate::server::envelope::error_codes::SESSION_OCCUPIED,
                                    "msg": message,
                                }),
                            )
                        }
                        crate::server::engine::EngineError::Stale(message) => HttpResponse::json(
                            409,
                            &json!({
                                "code": crate::server::envelope::error_codes::SESSION_STALE,
                                "msg": message,
                            }),
                        ),
                        other => HttpResponse::internal_error(other.to_string()),
                    },
                }
            }
            ("GET", p) if extract_session_action(p, "status").is_some() => {
                let session_id = extract_session_action(p, "status").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let busy = self
                    .engine
                    .as_ref()
                    .map(|e| e.is_busy(session_id))
                    .unwrap_or(false);
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let context_tokens: usize = history.iter().map(|m| m.content.len() / 4).sum();

                let agent_config = self
                    .store
                    .get_state("agent_config", session_id)
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Null);
                let metadata = self
                    .store
                    .get_state("metadata", session_id)
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Null);
                let active_model = agent_config
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or_else(|| self.engine.as_ref().map(|e| e.model_name().to_string()))
                    .unwrap_or_else(|| "kimi-latest".to_string());

                let permission = agent_config
                    .get("permission_mode")
                    .and_then(|v| v.as_str())
                    .or_else(|| metadata.get("permission_mode").and_then(|v| v.as_str()))
                    .unwrap_or("auto");
                let thinking_level = agent_config
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("medium");
                let plan_mode = agent_config
                    .get("plan_mode")
                    .and_then(|v| v.as_bool())
                    .or_else(|| metadata.get("plan_mode").and_then(|v| v.as_bool()))
                    .unwrap_or(false);
                let max_context_tokens = metadata
                    .get("max_context_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(262_144);
                let context_usage = if max_context_tokens == 0 {
                    0.0
                } else {
                    context_tokens as f64 / max_context_tokens as f64
                };

                let swarm_mode = agent_config
                    .get("swarm_mode")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let tower_mode = crate::tools::tower::paths::tower_enabled(
                    crate::tools::tower::paths::tower_env_switch(),
                    None,
                );
                HttpResponse::ok(&json!({




                    "busy": busy,
                    "model": active_model,
                    "thinking_level": thinking_level,
                    "permission": permission,
                    "plan_mode": plan_mode,
                    "swarm_mode": swarm_mode,
                    "tower_mode": tower_mode,
                    "context_tokens": context_tokens,
                    "max_context_tokens": max_context_tokens,
                    "context_usage": context_usage,
                }))
            }
            ("GET", p) if extract_session_action(p, "snapshot").is_some() => {
                let session_id = extract_session_action(p, "snapshot").unwrap();
                let session_opt = match self.store.get_session(session_id) {
                    Ok(s) => s,
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                let session = match session_opt {
                    Some(s) => s,
                    None => return HttpResponse::not_found(),
                };
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let questions = self.interaction_manager.list_questions(session_id);
                let approvals = self.interaction_manager.list_approvals(session_id);

                let messages_items: Vec<Value> = history
                    .into_iter()
                    .enumerate()
                    .map(|(idx, m)| {
                        json!({
                            "id": format!("msg_{idx}"),
                            "session_id": session_id,
                            "role": m.role,
                            "content": m.content,
                            "created_at": chrono::Utc::now().to_rfc3339()
                        })
                    })
                    .collect();

                let wire_session = format_wire_session(
                    &session,
                    &self.store,
                    self.engine.as_ref(),
                    self.default_workspace_root().as_deref(),
                );

                let subagent_manager = self.subagent_manager();
                let subagent_list = subagent_manager.list().await;

                let mut identities: std::collections::HashMap<
                    String,
                    crate::subagent::manager::SubagentParent,
                > = std::collections::HashMap::new();
                for sub in &subagent_list {
                    if let Some(parent) = subagent_manager.parent_of(&sub.id).await {
                        identities.insert(sub.id.clone(), parent);
                    }
                }
                let subagents_val: Vec<Value> = subagent_list
                    .into_iter()
                    .map(|sub| {
                        let phase = match sub.state {
                            crate::subagent::types::SubagentState::Running => "working",
                            crate::subagent::types::SubagentState::Idle => "queued",
                            crate::subagent::types::SubagentState::Completed => "completed",
                            crate::subagent::types::SubagentState::Failed => "failed",
                            crate::subagent::types::SubagentState::Terminated => "failed",
                        };
                        let status = match sub.state {
                            crate::subagent::types::SubagentState::Running
                            | crate::subagent::types::SubagentState::Idle => "running",
                            crate::subagent::types::SubagentState::Completed => "completed",
                            crate::subagent::types::SubagentState::Failed
                            | crate::subagent::types::SubagentState::Terminated => "failed",
                        };
                        let identity = identities.get(&sub.id);
                        let mut entry = json!({
                            "id": sub.id,
                            "session_id": session_id,
                            "kind": "subagent",
                            "description": format!("{} ({})", sub.role, sub.type_name),
                            "status": status,
                            "subagent_phase": phase,
                            "subagent_type": sub.type_name,
                            "created_at": chrono::DateTime::from_timestamp_millis(sub.created_at_ms as i64)
                                .map(|dt| dt.to_rfc3339())
                                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
                        });
                        if let Some(obj) = entry.as_object_mut() {
                            if let Some(call_id) =
                                identity.and_then(|p| p.parent_tool_call_id.clone())
                            {
                                obj.insert("parent_tool_call_id".into(), json!(call_id));
                            }
                            if let Some(index) = identity.and_then(|p| p.swarm_index) {
                                obj.insert("swarm_index".into(), json!(index));
                            }
                        }
                        entry
                    })
                    .collect();

                HttpResponse::ok(&json!({
                    "as_of_seq": 1,
                    "epoch": "epoch_1",
                    "session": wire_session,
                    "messages": {
                        "items": messages_items,
                        "has_more": false
                    },
                    "in_flight_turn": serde_json::Value::Null,
                    "subagents": subagents_val,
                    "pending_approvals": approvals,
                    "pending_questions": questions
                }))
            }
            ("GET", p) if extract_session_subaction(p, "transcript", "ops").is_some() => {
                let session_id = extract_session_subaction(p, "transcript", "ops").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let agent_id = req
                    .query_param("agent_id")
                    .unwrap_or_else(|| "main".to_string());
                let since_seq = req
                    .query_param("since_seq")
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(0);
                let latest_seq = self.store.latest_wire_event_seq(session_id).unwrap_or(0);

                let complete = latest_seq > 0 && since_seq <= latest_seq;
                let batches: Vec<Value> = if latest_seq > since_seq {
                    let history = self
                        .store
                        .load_session_history(session_id)
                        .unwrap_or_default();
                    let items =
                        crate::server::transcript::cold_items(&self.store, session_id, &history);
                    let snapshot = json!({
                        "items": items,
                        "tasks": crate::server::transcript::cold_tasks(
                            &self.store,
                            session_id,
                            self.prompt_queue
                                .active_turn_number(session_id)
                                .map(i64::from)
                        ),
                        "interactions": [],
                        "attachments": [],
                        "todos": [],
                        "prompts": crate::server::transcript::cold_prompts(
                            &self.store,
                            session_id,
                            self.prompt_queue
                                .active_turn_number(session_id)
                                .map(i64::from)
                        ),
                        "meta": {},
                    });

                    vec![json!({
                        "seq": latest_seq,
                        "ops": [{
                            "op": "reset",
                            "agentId": agent_id,
                            "snapshot": snapshot,
                        }],
                    })]
                } else {
                    Vec::new()
                };
                HttpResponse::ok(&json!({
                    "agent_id": agent_id,
                    "batches": batches,
                    "latest_seq": latest_seq,
                    "complete": complete,
                }))
            }
            ("GET", p) if extract_session_subaction(p, "transcript", "user-messages").is_some() => {
                let session_id =
                    extract_session_subaction(p, "transcript", "user-messages").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let agent_id = req
                    .query_param("agent_id")
                    .unwrap_or_else(|| "main".to_string());
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let items =
                    crate::server::transcript::cold_items(&self.store, session_id, &history);
                let messages = crate::server::transcript::project_user_messages(&items);
                HttpResponse::ok(&json!({
                    "agents": [{
                        "agent_id": agent_id,
                        "messages": messages,
                        "attachments": [],
                    }],
                }))
            }
            ("GET", p) if extract_session_subaction(p, "transcript", "plan").is_some() => {
                let session_id = extract_session_subaction(p, "transcript", "plan").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let agent_id = req
                    .query_param("agent_id")
                    .unwrap_or_else(|| "main".to_string());
                let tool_call_id = req.query_param("tool_call_id");
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let items =
                    crate::server::transcript::cold_items(&self.store, session_id, &history);
                let plans =
                    crate::server::transcript::project_plans(&items, tool_call_id.as_deref());
                if tool_call_id.is_some() && plans.is_empty() {
                    let requested = tool_call_id.unwrap_or_default();
                    return HttpResponse::json(
                        404,
                        &json!({
                            "code": crate::server::envelope::error_codes::SESSION_NOT_FOUND,
                            "msg": format!("no ExitPlanMode tool call found for tool_call_id: {requested}"),
                        }),
                    );
                }
                HttpResponse::ok(&json!({ "agent_id": agent_id, "plans": plans }))
            }
            ("GET", p) if extract_session_action(p, "transcript").is_some() => {
                let session_id = extract_session_action(p, "transcript").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let agent_id = req
                    .query_param("agent_id")
                    .unwrap_or_else(|| "main".to_string());
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let items =
                    crate::server::transcript::cold_items(&self.store, session_id, &history);
                let page = crate::server::transcript::TurnPageQuery {
                    before_turn: req.query_param("before_turn"),
                    after_turn: req.query_param("after_turn"),
                    page_size: req
                        .query_param("page_size")
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(crate::server::transcript::DEFAULT_PAGE_SIZE),
                };
                let (page_items, has_more) =
                    crate::server::transcript::paginate_turns(&items, &page);
                HttpResponse::ok(&json!({
                    "agent_id": agent_id,
                    "items": page_items,
                    "has_more": has_more,
                    "tasks": [],
                    "interactions": [],
                    "attachments": [],
                    "todos": [],
                    "prompts": crate::server::transcript::cold_prompts(
                        &self.store,
                        session_id,
                        self.prompt_queue
                            .active_turn_number(session_id)
                            .map(i64::from)
                    ),
                    "meta": {},
                    "agents": [{
                        "agentId": agent_id,
                        "type": if agent_id == "main" { "main" } else { "sub" },
                    }],
                    "pending_interactions": [],
                }))
            }
            ("POST", p) if extract_session_action(p, "abort").is_some() => {
                let session_id = extract_session_action(p, "abort").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                self.interaction_manager.cancel_session(session_id);
                let aborted = self
                    .engine
                    .as_ref()
                    .map(|e| e.cancel_turn(session_id))
                    .unwrap_or(false);
                HttpResponse::ok(&json!({ "aborted": aborted, "sessionId": session_id }))
            }
            ("POST", p) if extract_session_action(p, "fork").is_some() => {
                let session_id = extract_session_action(p, "fork").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }

                let _source_lease = match crate::session::lock::SESSION_LOCKS.lease(session_id) {
                    Ok(lease) => lease,
                    Err(error) => {
                        return HttpResponse::json(
                            409,
                            &json!({
                                "code": crate::server::envelope::error_codes::SESSION_OCCUPIED,
                                "msg": error.to_string(),
                            }),
                        );
                    }
                };

                let body: Value = if req.body.is_empty() {
                    json!({})
                } else {
                    match serde_json::from_slice(&req.body) {
                        Ok(v) => v,
                        Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                    }
                };
                let new_session_id = format!("sess-{}", fastrand::u64(..));
                let title = body.get("title").and_then(|v| v.as_str());

                let turn_index = match body.get("turnIndex") {
                    None | Some(Value::Null) => None,
                    Some(raw) => match raw.as_u64() {
                        Some(index) => Some(index),
                        None => {
                            return HttpResponse::bad_request(
                                "forkSession turnIndex must be a non-negative safe integer",
                            );
                        }
                    },
                };

                match self.store.fork_session_with_metadata(
                    session_id,
                    &new_session_id,
                    title,
                    turn_index,
                    body.get("metadata"),
                ) {
                    Ok(true) => {
                        let created = self.store.get_session(&new_session_id).ok().flatten();
                        let wire = match created {
                            Some(ref session) => format_wire_session(
                                session,
                                &self.store,
                                self.engine.as_ref(),
                                self.default_workspace_root().as_deref(),
                            ),
                            None => json!({}),
                        };
                        HttpResponse::ok(&wire)
                    }
                    Ok(false) => HttpResponse::not_found(),

                    Err(crate::session::sqlite_store::ForkError::Store(e)) => {
                        HttpResponse::internal_error(format!("Database error: {e}"))
                    }
                    Err(e) => HttpResponse::bad_request(e.to_string()),
                }
            }
            ("POST", p) if extract_session_action(p, "restore").is_some() => {
                let session_id = extract_session_action(p, "restore").unwrap();
                match self.store.restore_session(session_id) {
                    Ok(true) => {
                        self.clear_reclaimed(session_id);
                        let summary = self.store.get_session(session_id).ok().flatten();
                        let wire = match summary {
                            Some(ref session) => format_wire_session(
                                session,
                                &self.store,
                                self.engine.as_ref(),
                                self.default_workspace_root().as_deref(),
                            ),
                            None => json!({}),
                        };
                        HttpResponse::ok(&wire)
                    }
                    Ok(false) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", p) if extract_session_action(p, "archive").is_some() => {
                let session_id = extract_session_action(p, "archive").unwrap();
                match self.store.archive_session(session_id) {
                    Ok(true) => {
                        self.hub.evict_lane(session_id);
                        self.clear_reclaimed(session_id);
                        if let Some(engine) = self.engine.as_ref() {
                            engine.forget_change_notifier(session_id);
                            engine.forget_mcp_status(session_id);
                        }
                        HttpResponse::ok(&json!({ "archived": true }))
                    }
                    Ok(false) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", p) if extract_session_action(p, "delete").is_some() => {
                let session_id = extract_session_action(p, "delete").unwrap();
                let session = match self.store.get_session(session_id) {
                    Ok(Some(session)) => session,
                    Ok(None) => return session_not_found(session_id),
                    Err(e) => {
                        return HttpResponse::internal_error(format!("Database error: {e}"));
                    }
                };
                let workspace_id = session.workspace_id.clone();

                self.interaction_manager.cancel_session(session_id);
                if let Some(engine) = self.engine.as_ref() {
                    engine.cancel_turn(session_id);

                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    while engine.is_busy(session_id) && std::time::Instant::now() < deadline {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                crate::session::lock::SESSION_LOCKS.release(session_id);

                if let Err(error) = self.run_session_cleanup_hooks(session_id) {
                    return HttpResponse::json(
                        500,
                        &json!({
                            "code": crate::server::envelope::error_codes::INTERNAL_ERROR,
                            "msg": format!(
                                "Could not clean up session \"{session_id}\": {error}"
                            ),
                        }),
                    );
                }
                match self.store.delete_session(session_id) {
                    Ok(true) => {
                        self.clear_reclaimed(session_id);
                        if let Some(engine) = self.engine.as_ref() {
                            engine.forget_journal_head(session_id);
                            engine.forget_change_notifier(session_id);
                            engine.forget_mcp_status(session_id);
                        }

                        let session_cwd = workspace_id
                            .as_deref()
                            .and_then(|wid| self.store.get_workspace(wid).ok().flatten())
                            .map(|w| w.root);
                        let guard = crate::tools::external_hooks::HookGuard::new(
                            self.current_hooks().await,
                        )
                        .with_session_cwd(session_cwd);
                        guard
                            .notify_session_lifecycle(
                                "SessionEnd",
                                "archive",
                                json!({ "reason": "archive", "session_title": "" }),
                            )
                            .await;
                        let del_event = json!({
                            "type": "event.session.deleted",
                            "sessionId": session_id,
                            "workspaceId": workspace_id,
                        });
                        self.hub
                            .bus_for("global")
                            .publish(&crate::events::EngineEvent::Custom(del_event));

                        self.hub.evict_lane(session_id);
                        HttpResponse::ok(&json!({ "deleted": true, "sessionId": session_id }))
                    }
                    Ok(false) => session_not_found(session_id),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/children") => {
                let session_id = p
                    .strip_prefix("/api/v1/sessions/")
                    .and_then(|rest| rest.strip_suffix("/children"))
                    .unwrap_or_default();
                match self.store.list_children(session_id) {
                    Ok(children) => {
                        let items: Vec<Value> = children
                            .into_iter()
                            .map(|s| {
                                format_wire_session(
                                    &s,
                                    &self.store,
                                    self.engine.as_ref(),
                                    self.default_workspace_root().as_deref(),
                                )
                            })
                            .collect();

                        HttpResponse::ok(&json!({ "items": items, "has_more": false }))
                    }
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/children") => {
                let parent_session_id = p
                    .strip_prefix("/api/v1/sessions/")
                    .and_then(|rest| rest.strip_suffix("/children"))
                    .unwrap_or_default();
                let parent = match self.store.get_session(parent_session_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => return HttpResponse::not_found(),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let child_session_id = format!("sess-{}", fastrand::u64(..));

                let parent_title = parent
                    .title
                    .as_deref()
                    .filter(|title| !crate::session::sqlite_store::is_untitled(Some(title)))
                    .unwrap_or(parent_session_id);
                let title = body
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Child: {parent_title}"));
                let mut metadata = body.get("metadata").cloned().unwrap_or_else(|| json!({}));
                if let Some(object) = metadata.as_object_mut() {
                    object.insert("parentSessionId".into(), json!(parent_session_id));
                    object.insert("sessionKind".into(), json!("child"));
                }

                match self.store.fork_session_with_metadata(
                    parent_session_id,
                    &child_session_id,
                    Some(&title),
                    None,
                    Some(&metadata),
                ) {
                    Ok(true) => {}
                    Ok(false) => return session_not_found(parent_session_id),
                    Err(crate::session::sqlite_store::ForkError::Store(e)) => {
                        return HttpResponse::internal_error(format!("Database error: {e}"));
                    }
                    Err(e) => return HttpResponse::bad_request(e.to_string()),
                }
                let created_session = self.store.get_session(&child_session_id).ok().flatten();
                let session_val = if let Some(ref s) = created_session {
                    format_wire_session(
                        s,
                        &self.store,
                        self.engine.as_ref(),
                        self.default_workspace_root().as_deref(),
                    )
                } else {
                    json!(created_session)
                };

                HttpResponse::ok(&session_val)
            }
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/warnings") => {
                let session_id = p
                    .strip_prefix("/api/v1/sessions/")
                    .and_then(|rest| rest.strip_suffix("/warnings"))
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }

                let warnings = self.mcp_manager.session_warnings().await;
                HttpResponse::ok(&json!({ "warnings": warnings }))
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/title/generate") => {
                let session_id = p
                    .strip_prefix("/api/v1/sessions/")
                    .and_then(|rest| rest.strip_suffix("/title/generate"))
                    .unwrap_or_default();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let body: Value = if req.body.is_empty() {
                    json!({})
                } else {
                    serde_json::from_slice(&req.body).unwrap_or(json!({}))
                };
                let source = body.get("source").and_then(|v| v.as_str());
                let force = body.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

                let existing = self.store.get_session(session_id).ok().flatten();
                let existing_kind = existing
                    .as_ref()
                    .and_then(|session| session.normalized_title_kind());
                if !force && matches!(existing_kind, Some("custom") | Some("generated")) {
                    return HttpResponse::ok(&json!({
                        "title": existing
                            .as_ref()
                            .and_then(|session| session.title.clone())
                            .unwrap_or_default(),
                    }));
                }
                let Some(engine) = self.engine.as_ref() else {
                    return HttpResponse::ok(&json!({ "title": null }));
                };
                let Some(llm) = engine.session_llm(session_id).await else {
                    return HttpResponse::ok(&json!({ "title": null }));
                };

                let _slot = crate::session::title::title_generation_slot(session_id).await;
                let refreshed = self.store.get_session(session_id).ok().flatten();
                if !force
                    && matches!(
                        refreshed
                            .as_ref()
                            .and_then(|session| session.normalized_title_kind()),
                        Some("custom") | Some("generated")
                    )
                {
                    return HttpResponse::ok(&json!({
                        "title": refreshed
                            .as_ref()
                            .and_then(|session| session.title.clone())
                            .unwrap_or_default(),
                    }));
                }
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let emit_title_request = |event: serde_json::Value| {
                    self.hub
                        .bus_for(session_id)
                        .publish(&crate::events::EngineEvent::Custom(event));
                };
                match crate::session::title::generate_session_title_with_events(
                    &history,
                    source,
                    llm.as_ref(),
                    crate::callbacks::MAIN_AGENT_ID,
                    Some(&emit_title_request),
                )
                .await
                {
                    Ok(Some(title)) => {
                        match self
                            .store
                            .set_generated_title_if_uncustomized(session_id, &title, force)
                        {
                            Ok(true) => {
                                let patch = json!({
                                    "title": title,
                                    "title_kind": "generated",
                                    "titleKind": "generated",
                                    "is_custom_title": false,
                                    "isCustomTitle": false,
                                });
                                let inverse = json!({
                                    "title": existing
                                        .as_ref()
                                        .and_then(|session| session.title.clone()),
                                    "title_kind": existing_kind,
                                    "titleKind": existing_kind,
                                    "is_custom_title": existing_kind == Some("custom"),
                                    "isCustomTitle": existing_kind == Some("custom"),
                                });
                                self.hub.bus_for(session_id).publish(
                                    &crate::events::EngineEvent::Custom(json!({
                                        "type": "session.meta.updated",
                                        "agentId": crate::callbacks::MAIN_AGENT_ID,
                                        "sessionId": session_id,
                                        "title": title,
                                        "patch": patch,
                                        "inverse": inverse,
                                    })),
                                );
                                HttpResponse::ok(&json!({ "title": title }))
                            }
                            Ok(false) => HttpResponse::ok(&json!({
                                "title": self
                                    .store
                                    .get_session(session_id)
                                    .ok()
                                    .flatten()
                                    .and_then(|session| session.title)
                                    .unwrap_or_default(),
                            })),
                            Err(e) => HttpResponse::internal_error(format!(
                                "Failed to persist the generated title: {e}"
                            )),
                        }
                    }
                    Ok(None) => HttpResponse::ok(&json!({ "title": null })),
                    Err(e) => HttpResponse::bad_request(e),
                }
            }
            ("POST", p) if extract_session_action(p, "compact").is_some() => {
                let session_id = extract_session_action(p, "compact").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }

                let _occupancy = match crate::session::lock::SESSION_LOCKS.hold_guard(session_id) {
                    Ok(guard) => guard,
                    Err(error) => {
                        return HttpResponse::json(
                            409,
                            &json!({
                                "code": crate::server::envelope::error_codes::SESSION_OCCUPIED,
                                "msg": error.to_string(),
                            }),
                        );
                    }
                };

                if let Some(engine) = self.engine.as_ref()
                    && engine.is_busy(session_id)
                {
                    return HttpResponse::json(
                        409,
                        &json!({
                            "code": "SESSION_BUSY",
                            "error": "compaction refused: a turn is in flight",
                        }),
                    );
                }
                let body: Value = if req.body.is_empty() {
                    json!({})
                } else {
                    serde_json::from_slice(&req.body).unwrap_or(json!({}))
                };

                let instruction = body
                    .get("instruction")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty());

                let engine = match self.engine.as_ref() {
                    Some(engine) => engine,
                    None => {
                        return HttpResponse::json(
                            503,
                            &json!({ "error": "no engine configured for this server" }),
                        );
                    }
                };
                match engine.compact_session(session_id, instruction).await {
                    Ok(report) => {
                        engine.observe_journal_head(session_id);
                        HttpResponse::ok(&json!({
                            "compacted": true,
                            "removed": report.removed,
                            "sessionId": session_id,
                            "compactedCount": report.compacted_count,
                            "tokensBefore": report.tokens_before,
                            "tokensAfter": report.tokens_after,
                        }))
                    }
                    Err(crate::server::engine::EngineError::NoModel(message)) => {
                        HttpResponse::json(503, &json!({ "error": message }))
                    }
                    Err(error) => HttpResponse::internal_error(error.to_string()),
                }
            }
            ("POST", p) if extract_session_action(p, "takeover").is_some() => {
                let session_id = extract_session_action(p, "takeover").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }

                crate::session::lock::SESSION_LOCKS.takeover(session_id);
                HttpResponse::ok(&json!({ "taken_over": true }))
            }
            ("POST", p) if extract_session_action(p, "undo").is_some() => {
                let session_id = extract_session_action(p, "undo").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }

                let _occupancy = match crate::session::lock::SESSION_LOCKS.hold_guard(session_id) {
                    Ok(guard) => guard,
                    Err(error) => {
                        return HttpResponse::json(
                            409,
                            &json!({
                                "code": crate::server::envelope::error_codes::SESSION_OCCUPIED,
                                "msg": error.to_string(),
                            }),
                        );
                    }
                };
                let body: Value = if req.body.is_empty() {
                    json!({})
                } else {
                    serde_json::from_slice(&req.body).unwrap_or(json!({}))
                };
                let count = body.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                let explicit_revert = body.get("revert_files").and_then(|v| v.as_bool());
                let revert_files = explicit_revert.unwrap_or(true);

                if let Some(engine) = self.engine.as_ref()
                    && engine.is_busy(session_id)
                {
                    return HttpResponse::json(
                        409,
                        &json!({
                            "code": "SESSION_BUSY",
                            "error": "undo refused: a turn is in flight",
                        }),
                    );
                }

                let turn_numbers = match self.store.plan_undo_turns(session_id, count) {
                    Ok(numbers) => numbers,
                    Err(e) if e.contains("undo refused") => return HttpResponse::bad_request(e),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };

                let mut restored_files: Vec<String> = Vec::new();

                if revert_files {
                    let workdir = fs_routes::resolve_session_workdir(&self.store, session_id);
                    if workdir.is_none() && explicit_revert == Some(true) {
                        return HttpResponse::bad_request(
                            "undo requested `revert_files: true` but this session has no resolvable working directory, so its files cannot be restored. Retry with `revert_files: false` to undo history only.",
                        );
                    }
                    if crate::features::operationLog::operationService::operation_log_enabled() {
                        if let Some(workdir) = workdir
                            && let Some(from_turn_id) = turn_numbers.last()
                        {
                            let operations = crate::features::operationLog::operationService::AgentOperationService::new(
                                workdir.to_string_lossy(),
                                session_id,
                                crate::callbacks::MAIN_AGENT_ID,
                            );
                            if let Err(error) = operations
                                .restore_before_turn(*from_turn_id as u64)
                                .await
                            {
                                return HttpResponse::internal_error(format!(
                                    "undo refused: the workspace files could not be restored ({}). No history was removed.",
                                    error.message
                                ));
                            }
                        }
                    } else if let Some(workdir) = workdir {
                        let mut revert_failures: Vec<String> = Vec::new();
                        let mut file_conflicts: Vec<String> = Vec::new();
                        for turn_number in &turn_numbers {
                            match self.store.revert_turn_file_changes(
                                session_id,
                                *turn_number as usize,
                                &workdir,
                            ) {
                                Ok(paths) => restored_files.extend(paths),

                                Err(crate::session::sqlite_store::FileRevertError::Conflict {
                                    path,
                                    reason,
                                }) => {
                                    file_conflicts.push(format!("{path} ({reason})"));
                                }
                                Err(error) => {
                                    revert_failures.push(format!("turn {turn_number}: {error}"));
                                }
                            }
                        }
                        if !file_conflicts.is_empty() {
                            return HttpResponse::json(
                                409,
                                &json!({
                                    "code": "UNDO_FILE_CONFLICT",
                                    "error": format!(
                                        "undo refused: {} changed since the tool wrote it. No history was removed. Retry with `revert_files: false` to undo history only, or restore the file yourself first.",
                                        file_conflicts.join(", ")
                                    ),
                                }),
                            );
                        }
                        if !revert_failures.is_empty() {
                            return HttpResponse::internal_error(format!(
                                "undo refused: the workspace files of {} could not be restored ({}). No history was removed.",
                                turn_numbers
                                    .iter()
                                    .map(|n| n.to_string())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                revert_failures.join("; ")
                            ));
                        }
                    }
                }
                match self.store.undo_turns(session_id, count) {
                    Ok(undone) => {
                        if let Err(e) = self.rollback_state_for_undo(session_id, count) {
                            return HttpResponse::internal_error(format!(
                                "undo removed {undone} turn(s) but the workspace state could not be restored ({e}). The transcript and the todo/plan state now disagree; re-run /undo or restart the session."
                            ));
                        }

                        if let Some(engine) = self.engine.as_ref() {
                            engine.record_context_truncation(session_id);
                            engine.observe_journal_head(session_id);
                        }
                        match self.store.refresh_last_prompt_after_undo(session_id) {
                            Ok(last_prompt) => {
                                let title = self
                                    .store
                                    .get_session(session_id)
                                    .ok()
                                    .flatten()
                                    .and_then(|session| session.title);
                                self.hub.bus_for(session_id).publish(
                                    &crate::events::EngineEvent::Custom(json!({
                                        "type": "session.meta.updated",
                                        "agentId": crate::callbacks::MAIN_AGENT_ID,
                                        "sessionId": session_id,
                                        "title": title,
                                        "patch": { "lastPrompt": last_prompt },
                                    })),
                                );
                            }
                            Err(e) => {
                                tracing::warn!(
                                    %session_id,
                                    %e,
                                    "failed to reconcile lastPrompt at undo"
                                );
                            }
                        }

                        if let Err(e) = self.store.set_last_turn_reason(session_id, None) {
                            tracing::warn!(%session_id, %e, "failed to clear last_turn_reason at undo");
                        }
                        if crate::session::wire_journal::journal_authoritative(
                            &self.store,
                            session_id,
                        ) {
                            let plan = match crate::session::wire_journal::undo_switch_plan(
                                &self.store,
                                session_id,
                                count,
                            ) {
                                Some(plan) => plan,
                                None => {
                                    return HttpResponse::bad_request(
                                        "undo refused: insufficient".to_string(),
                                    );
                                }
                            };
                            self.hub.bus_for(session_id).publish(
                                &crate::events::EngineEvent::Custom(json!({
                                    "type": "agent.switched",
                                    "agentId": crate::callbacks::MAIN_AGENT_ID,
                                    "branch": plan.branch,
                                    "reason": "undo",
                                    "base": { "branch": plan.base_branch, "line": plan.base_line },
                                    "turns": count,
                                    "legacyUndoLine": plan.legacy_undo_line,
                                })),
                            );
                            self.hub.bus_for(session_id).publish(
                                &crate::events::EngineEvent::Custom(json!({
                                    "type": "context.undo",
                                    "agentId": crate::callbacks::MAIN_AGENT_ID,
                                    "count": count,
                                })),
                            );
                            self.hub.bus_for(session_id).publish(
                                &crate::events::EngineEvent::Custom(json!({
                                    "type": "context.undone",
                                    "sessionId": session_id,
                                    "undone": undone,
                                    "turnNumbers": turn_numbers,
                                    "agentId": crate::callbacks::MAIN_AGENT_ID,
                                    "turns": count,
                                    "fromTurnId": Value::Null,
                                })),
                            );
                        }

                        if !restored_files.is_empty() {
                            self.hub.bus_for(session_id).publish(
                                &crate::events::EngineEvent::Custom(json!({
                                    "type": "checkpoint.restored",
                                    "sessionId": session_id,
                                    "restored": restored_files,
                                    "conflicts": [],
                                })),
                            );
                        }
                        HttpResponse::ok(&json!({
                            "undone": undone,
                            "sessionId": session_id
                        }))
                    }

                    Err(e) if e.contains("undo refused") => HttpResponse::bad_request(e),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("POST", p) if extract_session_action(p, "btw").is_some() => {
                let session_id = extract_session_action(p, "btw").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let history = self
                    .store
                    .load_session_history(session_id)
                    .unwrap_or_default();
                let subagent_mgr = self.subagent_manager();
                match crate::subagent::start_btw(&subagent_mgr, &history).await {
                    Ok(agent_id) => {
                        let data = json!({ "agent_id": agent_id });
                        if req.wants_envelope() {
                            HttpResponse::envelope_ok(&data, &req.request_id())
                        } else {
                            HttpResponse::ok(&data)
                        }
                    }
                    Err(e) => HttpResponse::internal_error(format!("Failed to start btw: {e}")),
                }
            }
            ("GET", p) if extract_session_action(p, "messages").is_some() => {
                let session_id = extract_session_action(p, "messages").unwrap();
                match self.store.get_session(session_id) {
                    Ok(Some(_)) => {
                        let history = self
                            .store
                            .load_session_history(session_id)
                            .unwrap_or_default();

                        let items: Vec<Value> = history
                            .iter()
                            .enumerate()
                            .map(|(index, message)| {
                                project_wire_message(session_id, index, message)
                            })
                            .collect();
                        HttpResponse::ok(&json!({
                            "items": items,
                            "has_more": false,
                            "sessionId": session_id,
                            "messages": history,
                        }))
                    }
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("PATCH", p) if extract_session_action(p, "goal").is_some() => {
                let session_id = extract_session_action(p, "goal").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let Some(objective) = body.get("objective").and_then(|v| v.as_str()) else {
                    return HttpResponse::bad_request("objective must be a string");
                };

                let Some(result) = self
                    .engine
                    .as_ref()
                    .and_then(|engine| engine.edit_session_goal_objective(session_id, objective))
                else {
                    return HttpResponse::not_found();
                };
                match result {
                    Ok(snapshot) if snapshot.is_null() => HttpResponse::not_found(),
                    Ok(snapshot) => HttpResponse::ok(&snapshot),
                    Err(error) => HttpResponse::bad_request(error),
                }
            }
            ("GET", p) if extract_session_action(p, "goal").is_some() => {
                let session_id = extract_session_action(p, "goal").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return session_not_found(session_id);
                }

                let goal_val = self
                    .engine
                    .as_ref()
                    .and_then(|engine| engine.session_goal_snapshot(session_id))
                    .or_else(|| {
                        self.store
                            .get_state("goal", session_id)
                            .unwrap_or(None)
                            .filter(|goal| !goal.is_null())
                    })
                    .unwrap_or(Value::Null);

                HttpResponse::ok(&goal_val)
            }
            ("GET", p) if extract_session_action(p, "skills").is_some() => {
                let session_id = extract_session_action(p, "skills").unwrap();
                let session = match self.store.get_session(session_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => return HttpResponse::not_found(),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                let ws_root = if let Some(ws_id) = session.workspace_id.as_deref() {
                    self.store
                        .get_workspace(ws_id)
                        .ok()
                        .flatten()
                        .map(|w| std::path::PathBuf::from(w.root))
                } else {
                    None
                };
                let config = self.config().await;
                let merge = config.resolve_merge_all_available_skills();
                let extra = config.extra_skill_dirs_paths();
                let plugin_dirs = self.plugin_manager.plugin_skill_dirs();
                let skills = crate::skills::scan_all_skills_with_extra_merge_and_plugins(
                    ws_root.as_deref(),
                    &extra,
                    &plugin_dirs,
                    merge,
                );
                HttpResponse::ok(&json!({
                    "sessionId": session_id,
                    "skills": skills,
                }))
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/skills/")
                    && p.ends_with(":activate") =>
            {
                let remainder = &p["/api/v1/sessions/".len()..];
                let (session_id, rest) = match remainder.split_once("/skills/") {
                    Some(pair) => pair,
                    None => return HttpResponse::not_found(),
                };
                let skill_name = match rest.strip_suffix(":activate") {
                    Some(name) if !name.is_empty() => name,
                    _ => return HttpResponse::not_found(),
                };
                let session = match self.store.get_session(session_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => return HttpResponse::not_found(),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                let ws_root = if let Some(ws_id) = session.workspace_id.as_deref() {
                    self.store
                        .get_workspace(ws_id)
                        .ok()
                        .flatten()
                        .map(|w| std::path::PathBuf::from(w.root))
                } else {
                    None
                };
                let config = self.config().await;
                let merge = config.resolve_merge_all_available_skills();
                let extra = config.extra_skill_dirs_paths();
                let plugin_dirs = self.plugin_manager.plugin_skill_dirs();
                let skills = crate::skills::scan_all_skills_with_extra_merge_and_plugins(
                    ws_root.as_deref(),
                    &extra,
                    &plugin_dirs,
                    merge,
                );
                if !skills.iter().any(|s| s.name == skill_name) {
                    return HttpResponse::json(
                        404,
                        &json!({ "error": "skill.not_found", "code": 40415 }),
                    );
                }
                HttpResponse::ok(&json!({ "activated": true, "skill_name": skill_name }))
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
