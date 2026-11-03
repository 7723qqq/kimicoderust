use super::catalog_routes::format_wire_session;
use super::*;

impl HttpServer {
    pub(super) async fn route_prompts(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("POST", "/api/v1/sessions") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let session_id = format!("sess-{}", fastrand::u64(..));
                let title = body.get("title").and_then(|v| v.as_str());
                let workspace_id = body
                    .get("workspaceId")
                    .or_else(|| body.get("workspace_id"))
                    .and_then(|v| v.as_str());

                match self
                    .store
                    .create_session_with_workspace(&session_id, title, workspace_id)
                {
                    Ok(_) => {
                        let created_session = self.store.get_session(&session_id).ok().flatten();
                        let session_val = if let Some(ref s) = created_session {
                            format_wire_session(s, &self.store, self.engine.as_ref())
                        } else {
                            json!(created_session)
                        };
                        let event = json!({
                            "type": "event.session.created",
                            "sessionId": session_id,
                            "session": session_val,
                        });
                        self.hub
                            .bus_for(&session_id)
                            .publish(&crate::events::EngineEvent::Custom(event));

                        // v2 `sessionExternalHooksService.triggerSessionStart`:
                        // SessionStart hooks fire on creation (source
                        // `startup`; the fork route never triggers one
                        // upstream). Observational only. Session-level hooks
                        // run in the session's workspace (v2 #4242's session
                        // cwd, which session hooks always had upstream).
                        let session_cwd = workspace_id
                            .and_then(|wid| self.store.get_workspace(wid).ok().flatten())
                            .map(|w| w.root);
                        let guard = crate::tools::external_hooks::HookGuard::new(
                            self.config().await.hooks.clone(),
                        )
                        .with_session_cwd(session_cwd);
                        guard
                            .notify_session_lifecycle(
                                "SessionStart",
                                "startup",
                                json!({
                                    "source": "startup",
                                    "session_title": title.unwrap_or(""),
                                }),
                            )
                            .await;

                        // v2 answers the created session as the bare
                        // `sessionSchema` with no `statusCode` override
                        // (kap-server/src/routes/sessions.ts:181-184), so this is
                        // HTTP 200 and the document itself — not 201, and not a
                        // `{sessionId}` stub.
                        HttpResponse::ok(&session_val)
                    }
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/prompts") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let (active, queued) = self.prompt_queue.snapshot(session_id);
                HttpResponse::ok(&json!({ "active": active, "queued": queued }))
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/prompts:steer") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(value) => value,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                // Contract shape: `{ prompt_ids: [...] }`. A legacy `{ prompt }`
                // string steers one fresh message without a queued record.
                let ids: Vec<String> = body
                    .get("prompt_ids")
                    .and_then(|value| value.as_array())
                    .map(|ids| {
                        ids.iter()
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    let prompt = body
                        .get("prompt")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    if prompt.is_empty() {
                        return HttpResponse::bad_request("prompt_ids must not be empty");
                    }
                    let Some(engine) = self.engine.as_ref() else {
                        return HttpResponse::json(
                            503,
                            &json!({ "error": "no engine configured for this server" }),
                        );
                    };
                    let steered_id = format!("prompt-{}", fastrand::u64(..));
                    let accepted = engine.enqueue_steer(
                        session_id,
                        crate::turn_loop::types::LLMMessage::user(prompt),
                    );
                    return if accepted {
                        HttpResponse::ok(&json!({ "steered": true, "prompt_ids": [steered_id] }))
                    } else {
                        HttpResponse::json(
                            404,
                            &json!({
                                "code": crate::server::envelope::error_codes::PROMPT_NOT_FOUND,
                                "msg": "no active turn to steer",
                            }),
                        )
                    };
                }
                if !ids
                    .iter()
                    .all(|id| self.prompt_queue.contains(session_id, id))
                {
                    return HttpResponse::json(
                        404,
                        &json!({
                            "code": crate::server::envelope::error_codes::PROMPT_NOT_FOUND,
                            "msg": "no queued prompt with the requested id",
                        }),
                    );
                }
                let Some(engine) = self.engine.as_ref() else {
                    return HttpResponse::json(
                        503,
                        &json!({ "error": "no engine configured for this server" }),
                    );
                };
                let mut steered_items: Vec<Value> = Vec::new();
                let mut unsteered = Vec::new();
                for (item, prompt, blocks, origin) in
                    self.prompt_queue.take_queued(session_id, &ids)
                {
                    // v2 #3906: the steered message reuses the queued prompt's
                    // id (`children[0].waiter.id`), so the projection keeps it
                    // inside the host turn and undo targets the host prompt.
                    let prompt_id = item["prompt_id"].as_str().map(str::to_string);
                    let message = crate::turn_loop::types::LLMMessage {
                        role: "user".to_string(),
                        content: prompt.clone(),
                        blocks: blocks.clone(),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        prompt_id,
                        origin: None,
                    };
                    if engine.enqueue_steer(session_id, message) {
                        // v2 `turnSteerSchema` (turnOps.ts:51): the fold
                        // pairs the steer to its turn and prompt ids; the
                        // payload keeps the envelope fields the Rust hub
                        // adds, which the interface does not declare.
                        // `agentId` is the literal `main` because a steer
                        // targets the session's main turn by definition — see
                        // the note in `prompt_queue.rs` on why this is not the
                        // subagent identity plumbing.
                        let steer_prompt_id =
                            item["prompt_id"].as_str().unwrap_or_default().to_string();
                        let mut steer = json!({
                            "type": "turn.steer",
                            "agentId": "main",
                            "sessionId": session_id,
                            "input": item.get("content").cloned().unwrap_or_else(|| json!([])),
                            "origin": origin.clone()
                                .unwrap_or_else(|| json!({ "kind": "user" })),
                            "promptIds": [steer_prompt_id],
                            "messageId": steer_prompt_id,
                        });
                        if let Some(turn) = self.prompt_queue.active_turn_number(session_id) {
                            steer["turnId"] = json!(turn);
                        }
                        crate::server::prompt_queue::publish_prompt_event(
                            &self.hub, session_id, steer,
                        );
                        steered_items.push(item);
                    } else {
                        unsteered.push((item, prompt, blocks, origin));
                    }
                }
                if !steered_items.is_empty() {
                    let active_prompt_id = self
                        .prompt_queue
                        .active_item(session_id)
                        .and_then(|item| item["prompt_id"].as_str().map(str::to_string))
                        .unwrap_or_default();
                    let prompt_ids: Vec<String> = steered_items
                        .iter()
                        .map(|item| item["prompt_id"].as_str().unwrap_or_default().to_string())
                        .collect();
                    // Each item's `content` is itself a part array; flat-map
                    // so the steered event carries one flat part list.
                    let content: Vec<Value> = steered_items
                        .iter()
                        .flat_map(|item| {
                            item.get("content")
                                .and_then(|value| value.as_array())
                                .cloned()
                                .unwrap_or_default()
                        })
                        .collect();
                    crate::server::prompt_queue::publish_prompt_event(
                        &self.hub,
                        session_id,
                        json!({
                            "type": "prompt.steered",
                            "agentId": "main",
                            "sessionId": session_id,
                            "activePromptId": active_prompt_id,
                            "promptIds": prompt_ids,
                            "content": content,
                            "steeredAt": chrono::Utc::now().to_rfc3339(),
                        }),
                    );
                }
                // A steer that found no running turn must not drop the prompt:
                // run it (or queue behind whatever is actually active).
                for (item, prompt, blocks, origin) in unsteered {
                    self.run_or_queue_prompt(engine, session_id, item, prompt, blocks, origin);
                }
                HttpResponse::ok(&json!({ "steered": true, "prompt_ids": ids }))
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/prompts/")
                    && p.ends_with(":steer") =>
            {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 7 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let prompt_id = segments[6].trim_end_matches(":steer").to_string();
                let Some(engine) = self.engine.as_ref() else {
                    return HttpResponse::json(
                        503,
                        &json!({ "error": "no engine configured for this server" }),
                    );
                };
                if !self.prompt_queue.contains(session_id, &prompt_id) {
                    return HttpResponse::json(
                        404,
                        &json!({
                            "code": crate::server::envelope::error_codes::PROMPT_NOT_FOUND,
                            "msg": "no prompt with the requested id",
                        }),
                    );
                }
                let active_prompt_id = self
                    .prompt_queue
                    .active_item(session_id)
                    .and_then(|item| item["prompt_id"].as_str().map(str::to_string))
                    .unwrap_or_default();
                let mut unsteered = Vec::new();
                for (item, prompt, blocks, origin) in self
                    .prompt_queue
                    .take_queued(session_id, std::slice::from_ref(&prompt_id))
                {
                    let message = crate::turn_loop::types::LLMMessage {
                        role: "user".to_string(),
                        content: prompt.clone(),
                        blocks: blocks.clone(),
                        tool_calls: Vec::new(),
                        tool_call_id: None,

                        prompt_id: Some(prompt_id.clone()),
                        origin: None,
                    };
                    if engine.enqueue_steer(session_id, message) {
                        // v2 `turnSteerSchema` (turnOps.ts:51); see the
                        // `prompt_ids` route's emission above.
                        let mut steer = json!({
                            "type": "turn.steer",
                            "agentId": "main",
                            "sessionId": session_id,
                            "input": item.get("content").cloned().unwrap_or_else(|| json!([])),
                            "origin": origin.clone()
                                .unwrap_or_else(|| json!({ "kind": "user" })),
                            "promptIds": [prompt_id],
                            "messageId": prompt_id,
                        });
                        if let Some(turn) = self.prompt_queue.active_turn_number(session_id) {
                            steer["turnId"] = json!(turn);
                        }
                        crate::server::prompt_queue::publish_prompt_event(
                            &self.hub, session_id, steer,
                        );
                        crate::server::prompt_queue::publish_prompt_event(
                            &self.hub,
                            session_id,
                            json!({
                                "type": "prompt.steered",
                                "agentId": "main",
                                "sessionId": session_id,
                                "activePromptId": active_prompt_id,
                                "promptIds": [prompt_id],
                                "content": item.get("content").cloned()
                                    .unwrap_or_else(|| json!([])),
                                "steeredAt": chrono::Utc::now().to_rfc3339(),
                            }),
                        );
                    } else {
                        unsteered.push((item, prompt, blocks, origin));
                    }
                }
                for (item, prompt, blocks, origin) in unsteered {
                    self.run_or_queue_prompt(engine, session_id, item, prompt, blocks, origin);
                }
                HttpResponse::ok(&json!({ "steered": true, "prompt_ids": [prompt_id] }))
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/prompts/")
                    && p.ends_with(":abort") =>
            {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 7 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let prompt_id = segments[6].trim_end_matches(":abort");
                // v2's `cancelWaiter` (loopService.ts:735-750): a queued prompt
                // is removed from the queue so the driver never promotes it,
                // and the active prompt's turn is cancelled. Either way the
                // item is settled exactly once — the `prompt.aborted` below is
                // its only terminal event, so the driver publishes no
                // `prompt.completed` on top of it.
                //
                // The web UI's abort button sends the live
                // `event.message.created` id (`msg-u{turn}`), which names the
                // turn's user message rather than the prompt; resolving it
                // here is what keeps the button working (ROADMAP §7.6).
                let Some((prompt_id, outcome)) = self
                    .prompt_queue
                    .cancel(session_id, prompt_id)
                    .map(|outcome| (prompt_id.to_string(), outcome))
                    .or_else(|| {
                        self.prompt_queue
                            .cancel_by_user_message_id(session_id, prompt_id)
                    })
                else {
                    // An unknown / already-settled prompt is not in the queue,
                    // so the abort has no target: report it as a missing
                    // prompt rather than a false success.
                    return HttpResponse::json(
                        404,
                        &json!({
                            "code": crate::server::envelope::error_codes::PROMPT_NOT_FOUND,
                            "msg": "no prompt with the requested id",
                        }),
                    );
                };
                if matches!(outcome, crate::server::prompt_queue::CancelOutcome::Active)
                    && let Some(engine) = self.engine.as_ref()
                {
                    engine.cancel_turn(session_id);
                }
                crate::server::prompt_queue::publish_prompt_event(
                    &self.hub,
                    session_id,
                    json!({
                        "type": "prompt.aborted",
                        "agentId": "main",
                        "sessionId": session_id,
                        "promptId": prompt_id,
                        "abortedAt": chrono::Utc::now().to_rfc3339(),
                    }),
                );
                HttpResponse::ok(&json!({ "aborted": true }))
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/prompts") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(value) => value,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                self.submit_session_prompt(segments[4], body).await
            }
            ("POST", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/prompt") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];

                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let prompt = match body.get("prompt").and_then(|v| v.as_str()) {
                    Some(p) => p,
                    None => return HttpResponse::bad_request("Missing 'prompt' field in payload"),
                };

                let Some(engine) = self.engine.as_ref() else {
                    // No engine attached. Refusing is the honest answer: this
                    // route used to reply `Processed: {prompt}` without running
                    // anything, which a client cannot tell from a real turn.
                    return HttpResponse::json(
                        503,
                        &json!({ "error": "no engine configured for this server" }),
                    );
                };

                let known = self
                    .store
                    .list_sessions()
                    .map(|sessions| sessions.iter().any(|s| s.session_id == session_id))
                    .unwrap_or(false);
                if !known {
                    return HttpResponse::not_found();
                }

                // The synchronous single-prompt route refuses to start a second
                // concurrent turn; queued prompts go through `/prompts`.
                if self.prompt_queue.active_item(session_id).is_some() || engine.is_busy(session_id)
                {
                    return HttpResponse::json(
                        409,
                        &json!({
                            "code": crate::server::envelope::error_codes::SESSION_BUSY,
                            "msg": "session is already running a turn",
                        }),
                    );
                }

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
                        "sessionId": session_id,
                        "turnId": report.turn_id,
                        "turnNumber": turn_number,
                        "status": "completed",
                        "stopReason": report.stop_reason,
                        "content": report.reply,
                        "steps": report.steps,
                        "llmTransport": report.llm_transport,
                        "eventsEmitted": report.events_emitted,
                        "nativeToolCalls": report.native_tool_calls,
                        "usage": report.usage,
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
                        other => HttpResponse::internal_error(other.to_string()),
                    },
                }
            }
            _ => HttpResponse::not_found(),
        };
        finalize_response(req, resp)
    }
}
