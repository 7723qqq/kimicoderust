use super::catalog_routes::format_wire_session;
use super::*;

/// v2 `SESSION_NOT_FOUND` (`session/errors.ts`), which kap-server maps to
/// HTTP 404 / wire code 40401.
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
                    // No engine attached: still hand back the analyzed prompt so
                    // a caller can run it elsewhere.
                    return HttpResponse::ok(&json!({
                        "status": "initiated",
                        "sessionId": session_id,
                        "prompt": prompt,
                    }));
                };
                let history = match self.store.load_session_history(session_id) {
                    Ok(messages) => messages,
                    Err(error) => {
                        // v2 `session_load_failed`: a session whose stored
                        // history cannot be read is the failure a user hits as
                        // "my conversation is gone", and until now it left no
                        // event behind — only a 500 in the access log. Both
                        // sites here are the same failure mode: a turn cannot
                        // start, so the user sees the same nothing.
                        //
                        // v2's `SessionLoadFailedEvent` declares exactly one
                        // property, `reason`. `session_id` and `stage` were this
                        // engine's own invention: a consumer written against v2
                        // would not look for them, and a strict one would reject
                        // them.
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
                // Read the session's own profile. `permission` / `thinking_level`
                // used to be string literals ("auto" / "medium"), so a client that
                // read the REST status once saw the defaults no matter what the
                // user had configured — and disagreed with the same engine's
                // `agent.status.updated` event, which reads `agent_config`.
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
                // Same two homes as `ServerEngine::status_payload`: the profile
                // route writes `agent_config`, the engine's per-turn resolution
                // reads `metadata`.
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
                // Swarm / tower ride the session profile (`agent_config`), which
                // the profile route writes; tower additionally follows the
                // engine-wide experimental gate.
                let swarm_mode = agent_config
                    .get("swarm_mode")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let tower_mode = crate::tools::tower::paths::tower_enabled(
                    crate::tools::tower::paths::tower_env_switch(),
                    None,
                );
                HttpResponse::ok(&json!({
                    // v2 `sessionStatusResponseSchema`
                    // (agent-core-v2/src/app/sessionLegacy/sessionProtocol.ts:74):
                    // exactly these keys, answered bare. `sessionId` /
                    // `total_turns` / `created_at` were fork additions.
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
                // The swarm identity a reconnecting client needs to rebuild the
                // members under their `AgentSwarm` call instead of drawing each
                // one as an ordinary subagent (v2 `role: 'member'`;
                // `snapshotSubagentSchema`). `swarm_index` is absent for a plain
                // `Agent` child, which is exactly the discriminator, so both
                // keys are omitted rather than nulled when there is no swarm.
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
                // `complete` mirrors the transcript contract: the persisted
                // journal covers the cursor only once it holds events; an empty
                // journal signals the caller to do a full refresh instead.
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
                    // A single idempotent `reset` op rebuilds the client from the
                    // authoritative snapshot —correct for any lag, at the cost
                    // of not being a minimal delta.
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
                // v2 `fork` (sessionLifecycleService.ts:625) takes the source
                // session's lease for the whole copy rather than refusing a
                // running turn: a foreign process's publish denies the fork,
                // while this process's own turn holds the same publish and the
                // lease re-enters it.
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
                // A running turn does not block the fork. v2 takes
                // `tryAcquireHistorySnapshot()` and copies only the records
                // before the pending work (`sliceRecordsBeforePendingWork`);
                // this store's copy reads `turns` + `messages`, which the
                // active turn has not written yet — its rows land at turn end
                // — so the copy is the same committed prefix, and the source
                // turn keeps running.
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
                // v2 `forkSession`'s `opts.turnIndex` (`sessionLifecycle.ts:25`,
                // validated by `assertForkTurnIndex`): a non-negative safe
                // integer naming a user-visible turn to cut at. A present but
                // non-integer / negative / fractional value is a client error,
                // not a silent "copy everything".
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
                        // v2 `forkSessionResponseSchema = sessionSchema`
                        // (kap-server/src/protocol/rest-session.ts:103) answered as
                        // the bare wire session via `okEnvelope` — the new
                        // session's document, nothing wrapped around it.
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
                    // v2 raises `REQUEST_INVALID` for both a malformed index
                    // and one naming a turn the session does not have, so both
                    // are 400s here rather than a store 500.
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
                        // v2 `restoreSessionAction` answers the restored session
                        // via `okEnvelope(session, …)`
                        // (kap-server/src/routes/sessions.ts:976).
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
                        // v2 #4281: archiving evicts the session's event state —
                        // the lane's numbering restarts under a fresh epoch, so
                        // no client can resume the archived session's stream.
                        self.hub.evict_lane(session_id);
                        self.clear_reclaimed(session_id);
                        if let Some(engine) = self.engine.as_ref() {
                            // v2 disposes the change-notifier snapshot with the
                            // agent; an archived session has no further turns,
                            // and a later restore must seed a fresh baseline.
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
                // v2 `sessionManagerService.delete` (`:366-392`) closes the
                // session before the cleanup hooks run, so an in-flight turn
                // cannot re-create rows between cleanup and the row delete.
                self.interaction_manager.cancel_session(session_id);
                if let Some(engine) = self.engine.as_ref() {
                    engine.cancel_turn(session_id);
                    // The close awaits the session's lifecycle chain; a turn
                    // that is slow to notice cancellation is bounded at the
                    // same 3s the lock release grace uses.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                    while engine.is_busy(session_id) && std::time::Instant::now() < deadline {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                crate::session::lock::SESSION_LOCKS.release(session_id);
                // v2 runs every `onWillDeleteSession` cleanup and rethrows the
                // first failure, leaving the session intact.
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
                        // Session-level hooks run in the session's workspace
                        // (v2 #4242's session cwd, which session hooks always
                        // had upstream).
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
                        // v2 #4281: deletion evicts the session's event state
                        // (after the deletion notice went out).
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
                        // v2 answers the paged `{items, has_more}` contract
                        // (kap-server routes/sessions.ts:694,
                        // pageResponseSchema(sessionSchema)); the web client
                        // reads `.items`, so a `children` key leaves it
                        // mapping over undefined.
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
                // v2 `createChild` (`sessionLifecycleService.ts:831-846`): the
                // default title names the parent, and the metadata document
                // carries the parent/kind keys on top of the caller's patch.
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
                // v2 `createChild` (`sessionLifecycleService.ts:831-846`) is a
                // `fork` with a fixed title and the parent/kind metadata, so the
                // child inherits titleKind/agents/custom and the source history.
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
                // v2 has no `statusCode` on create routes, so the envelope ships
                // as HTTP 200 (`success: { data: sessionSchema }`,
                // kap-server/src/routes/sessions.ts:181-184 for the parent,
                // :707-710 for the child) — not 201.
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
                // The engine's own degradations: MCP servers it could not
                // connect, and servers waiting on the user's authorization.
                // A healthy roster produces an empty list.
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
                // v2 `generateTitleOnce`: a non-forced call is a no-op while the
                // title is host-custom or already generated. Answering the
                // existing title (rather than regenerating) is what keeps a
                // custom title from being overwritten by the automatic pass.
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
                    // v2 `generateTitle` answers `undefined` when there is no
                    // main agent to ask — a normal outcome, not a request error.
                    return HttpResponse::ok(&json!({ "title": null }));
                };
                let Some(llm) = engine.session_llm(session_id).await else {
                    return HttpResponse::ok(&json!({ "title": null }));
                };
                // One attempt per session at a time: a concurrent caller waits
                // and then re-reads the applied title instead of issuing a
                // second model request (v2 `_shared`).
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
                match crate::session::title::generate_session_title(&history, source, llm.as_ref())
                    .await
                {
                    Ok(Some(title)) => {
                        match self
                            .store
                            .set_generated_title_if_uncustomized(session_id, &title, force)
                        {
                            Ok(true) => {
                                // Mirror v2 `SessionMetaUpdated` on the existing
                                // `session.meta.updated` contract: a patch plus
                                // its inverse, so an undo restores the previous
                                // title and clients fold one shape.
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
                // v2 `runGate.begin()` at compaction (fullCompactionService,
                // upstream #4246): the history rewrite runs under this
                // process's occupancy hold.
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
                // Never rewrite history under a running turn (v2 raises
                // SESSION_BUSY before beginning a compaction).
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
                // v2 `rest-session.ts:131-137`: the caller may pass a
                // summarization hint with the request.
                let instruction = body
                    .get("instruction")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.is_empty());
                // One owner for manual compaction: the engine resolves the
                // model, summarizes, persists, re-baselines the counting
                // state and publishes the start/finish facts. This route used
                // to re-implement that sequence, and the copy had drifted —
                // it never re-baselined the token counter after the cut.
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
                        // The rewrite shrank the journal; adopt the new head so
                        // the next turn does not read our own compaction as a
                        // foreign modification.
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
                // v2 `takeoverSessionAction` (upstream #4246): evict a foreign
                // holder so the next claim succeeds; a hold of ours is left
                // alone.
                crate::session::lock::SESSION_LOCKS.takeover(session_id);
                HttpResponse::ok(&json!({ "taken_over": true }))
            }
            ("POST", p) if extract_session_action(p, "undo").is_some() => {
                let session_id = extract_session_action(p, "undo").unwrap();
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                // v2 `runGate.begin()` at undo (undoService, upstream #4246):
                // the row delete and state rollback run under this process's
                // occupancy hold.
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
                let revert_files = body
                    .get("revert_files")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // Never delete rows under a running turn (v2 raises
                // SESSION_BUSY before undoing).
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
                // Resolve the turns first: a compaction-boundary refusal must
                // happen before any workspace file is touched, and file
                // history is keyed by turn *number*, not by the undo count.
                let turn_numbers = match self.store.plan_undo_turns(session_id, count) {
                    Ok(numbers) => numbers,
                    Err(e) if e.contains("undo refused") => return HttpResponse::bad_request(e),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                // Paths restored by `revert_files`, reported as v2's
                // `checkpoint.restored` payload once the undo commits.
                let mut restored_files: Vec<String> = Vec::new();
                // Revert the workspace files *before* the turn rows are deleted:
                // if a file cannot be restored we must not report a successful
                // undo, and the history must stay intact so the operation can be
                // retried instead of silently diverging from the workspace.
                if revert_files {
                    let Some(workdir) = fs_routes::resolve_session_workdir(&self.store, session_id)
                    else {
                        return HttpResponse::bad_request(
                            "undo requested `revert_files` but this session has no resolvable working directory, so its files cannot be restored. Retry without `revert_files` to undo history only.",
                        );
                    };
                    let mut revert_failures: Vec<String> = Vec::new();
                    let mut file_conflicts: Vec<String> = Vec::new();
                    for turn_number in &turn_numbers {
                        match self.store.revert_turn_file_changes(
                            session_id,
                            *turn_number as usize,
                            &workdir,
                        ) {
                            Ok(paths) => restored_files.extend(paths),
                            // A file that moved on since the tool wrote it is
                            // the user's to keep: v2 reports `manual_edit` /
                            // `deleted` and leaves the file alone. Undo is
                            // refused here (the fork's rule: no partial undo)
                            // and the caller is told how to proceed.
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
                match self.store.undo_turns(session_id, count) {
                    Ok(undone) => {
                        // Restore the engine-owned state domains (`todo`,
                        // `plan`, …) to the anchor each turn pushed. Without
                        // this the transcript loses turns while `todo.json`
                        // still lists work the model can no longer see, and the
                        // next turn inherits a stale plan.
                        //
                        // Runs *after* the rows are deleted so that the
                        // compaction-boundary refusal above (`plan_undo_turns`)
                        // happens before any state changes; a failure here is
                        // therefore a partial application and must be reported
                        // rather than swallowed — a silent divergence is worse
                        // than a visible error.
                        if let Err(e) = self.rollback_state_for_undo(session_id, count) {
                            return HttpResponse::internal_error(format!(
                                "undo removed {undone} turn(s) but the workspace state could not be restored ({e}). The transcript and the todo/plan state now disagree; re-run /undo or restart the session."
                            ));
                        }
                        // The transcript shrank: anchors that measured the cut
                        // messages no longer apply to the session's context.
                        if let Some(engine) = self.engine.as_ref() {
                            engine.record_context_truncation(session_id);
                            // Undo shrank the journal through this process's
                            // own hold; adopt the new head so the next turn
                            // does not read it as a foreign modification.
                            engine.observe_journal_head(session_id);
                        }
                        // v2 `sessionOutcomeMirrorService.clearIfMatches` at
                        // undo: removing the turn that produced the recorded
                        // outcome invalidates it. Leaving it in place would
                        // report the outcome of a turn the user just discarded.
                        if let Err(e) = self.store.set_last_turn_reason(session_id, None) {
                            tracing::warn!(%session_id, %e, "failed to clear last_turn_reason at undo");
                        }
                        // Online transcripts must learn about the cut, exactly
                        // like delete/patch publish their own events.
                        self.hub
                            .bus_for(session_id)
                            .publish(&crate::events::EngineEvent::Custom(json!({
                                "type": "context.undone",
                                "sessionId": session_id,
                                "undone": undone,
                                "turnNumbers": turn_numbers,
                            })));
                        // v2 `CheckpointRestored` (`checkpointService.ts:27-31`):
                        // the files the undo put back, emitted only when the
                        // caller asked for a workspace revert and something was
                        // actually restored. Conflicts refuse the undo above, so
                        // the payload's conflict list is always empty here.
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
                    // Crossing the compaction boundary is a client error, not
                    // a database failure —surface the refusal verbatim.
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
                        // v2 `listMessagesResponseSchema` is
                        // `{ items: messageSchema[], has_more }`
                        // (kap-server/src/protocol/rest-message.ts:14). The fork
                        // answered `{sessionId, messages}` with raw `LLMMessage`
                        // rows, so `items` was missing and each element lacked
                        // the `id`/`session_id`/`content[]` block projection the
                        // schema requires. `messages` stays for the in-tree
                        // consumers that predate the paged contract.
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
                // v2 #4568: edit the objective of the current goal in place.
                // The engine owns the goal store, so the write and the
                // returned snapshot both come from there (same store the GET
                // route reads). Without an attached engine, answer 404 — this
                // server has no goal state to edit.
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
                // The engine's goal store is the authority: the turn settle
                // folds / pauses / blocks there, and the REST read must see
                // that snapshot (with its budget report). The sqlite state row
                // remains the fallback for a server running without an engine.
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
                // v2 answers the bare `goalSnapshotSchema` or `null`
                // (kap-server/src/routes/sessions.ts:798 →
                // `getSessionGoalResponseSchema = goalSnapshotSchema.nullable()`);
                // the fork's `{sessionId, goal}` wrapper made every client read
                // `status` off a level that never had it.
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
