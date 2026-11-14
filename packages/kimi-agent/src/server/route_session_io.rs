use super::catalog_routes::format_wire_session;
use super::*;

impl HttpServer {
    pub(super) async fn route_session_io(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("POST", p) if extract_session_action(p, "export").is_some() => {
                let session_id = extract_session_action(p, "export").unwrap();
                let options = SessionExportOptions {
                    include_global_log: req
                        .query_param("includeGlobalLog")
                        .is_some_and(|v| v == "true" || v == "1"),
                };
                match self.store.export_session(session_id) {
                    Ok(Some(export)) => {
                        let session_log = self
                            .engine
                            .as_ref()
                            .and_then(|engine| engine.session_log_path(session_id));
                        match build_session_export_zip(&export, &options, session_log.as_deref()) {
                            Ok(archive) => HttpResponse::bytes(200, "application/zip", archive)
                                .with_header(
                                    "Content-Disposition",
                                    format!(
                                        "attachment; filename=\"kimi-debug-{}-{}.zip\"",
                                        session_id.chars().take(8).collect::<String>(),
                                        chrono::Utc::now().format("%Y%m%d-%H%M%S")
                                    ),
                                ),
                            Err(e) => HttpResponse::internal_error(format!(
                                "Failed to build the export archive: {e}"
                            )),
                        }
                    }
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if extract_session_action(p, "export").is_some() => {
                let session_id = extract_session_action(p, "export").unwrap();
                match self.store.export_session(session_id) {
                    Ok(Some(export)) => HttpResponse::ok(&json!(export)),
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                }
            }
            ("GET", p) if extract_session_action(p, "profile").is_some() => {
                let session_id = extract_session_action(p, "profile").unwrap();
                let session = match self.store.get_session(session_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => return HttpResponse::not_found(),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };

                let wire_session = format_wire_session(
                    &session,
                    &self.store,
                    self.engine.as_ref(),
                    self.default_workspace_root().as_deref(),
                );
                HttpResponse::ok(&wire_session)
            }
            ("POST", p) if extract_session_action(p, "profile").is_some() => {
                let session_id = extract_session_action(p, "profile").unwrap();
                let session = match self.store.get_session(session_id) {
                    Ok(Some(s)) => s,
                    Ok(None) => return HttpResponse::not_found(),
                    Err(e) => return HttpResponse::internal_error(format!("Database error: {e}")),
                };
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                if let Some(new_title) = body.get("title").and_then(|v| v.as_str())
                    && let Err(e) = self
                        .store
                        .update_session_title(session_id, Some(new_title.trim()))
                {
                    return HttpResponse::internal_error(format!(
                        "Failed to persist the session title: {e}"
                    ));
                }
                if let Some(cfg) = body.get("agent_config") {
                    let mut current = self
                        .store
                        .get_state("agent_config", session_id)
                        .unwrap_or(None)
                        .unwrap_or_else(|| json!({}));
                    if let (Some(cur_obj), Some(new_obj)) =
                        (current.as_object_mut(), cfg.as_object())
                    {
                        for (k, v) in new_obj {
                            cur_obj.insert(k.clone(), v.clone());
                        }
                    } else {
                        current = cfg.clone();
                    }

                    if let Some(swarm_mode) = cfg.get("swarm_mode").and_then(Value::as_bool) {
                        let registry = crate::swarm::mode::swarm_mode_registry();
                        let changed = if swarm_mode {
                            registry.enter(
                                crate::callbacks::MAIN_AGENT_ID,
                                swarm_trigger_from(
                                    cfg.get("swarm_mode_trigger").and_then(Value::as_str),
                                ),
                            )
                        } else {
                            registry.exit(crate::callbacks::MAIN_AGENT_ID).is_some()
                        };
                        if changed {
                            self.emit_session_telemetry_named(if swarm_mode {
                                "swarm_mode_entered"
                            } else {
                                "swarm_mode_exited"
                            });
                        }
                    }
                    if let Some(hello_world_mode) =
                        cfg.get("hello_world_mode").and_then(Value::as_bool)
                    {
                        let agent_id = crate::callbacks::MAIN_AGENT_ID;
                        if let Some(engine) = self.engine.as_ref() {
                            let callbacks = engine.session_callbacks_for(session_id);
                            if hello_world_mode {
                                let workdir =
                                    fs_routes::resolve_session_workdir(&self.store, session_id)
                                        .unwrap_or_else(|| {
                                            std::env::current_dir().unwrap_or_default()
                                        });
                                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::enter_scoped(
                                    callbacks.as_ref(),
                                    crate::features::helloWorld::agent::helloWorldService::HelloWorldScope {
                                        agent_id,
                                        session_id,
                                        workspace_root: &workdir,
                                    },
                                    crate::features::helloWorld::agent::helloWorld::HelloWorldModeTrigger::Manual,
                                    None,
                                )
                                .await
                            } else {
                                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::exit(
                                    callbacks.as_ref(),
                                    agent_id,
                                )
                            }
                        } else {
                            let events = if hello_world_mode {
                                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::enter_events(
                                    agent_id,
                                    crate::features::helloWorld::agent::helloWorld::HelloWorldModeTrigger::Manual,
                                    None,
                                )
                            } else {
                                crate::features::helloWorld::agent::helloWorldService::HelloWorldService::exit_events(
                                    agent_id,
                                )
                                .map(|(_, events)| events)
                            };
                            let changed = events.is_some();
                            for event in events.into_iter().flatten() {
                                self.hub
                                    .bus_for(session_id)
                                    .publish(&crate::events::EngineEvent::Custom(event));
                            }
                            if changed {
                                self.emit_session_telemetry_named(if hello_world_mode {
                                    "hello_world_mode_entered"
                                } else {
                                    "hello_world_mode_exited"
                                });
                            }
                            changed
                        };
                    }
                    if let Err(e) = self.store.put_session_state(
                        "agent_config",
                        session_id,
                        session_id,
                        &current,
                    ) {
                        return HttpResponse::internal_error(format!(
                            "Failed to persist the agent config: {e}"
                        ));
                    }
                }
                let updated_session = self
                    .store
                    .get_session(session_id)
                    .ok()
                    .flatten()
                    .unwrap_or(session);

                let mut meta_patch = body.clone();
                if let Some(patch_object) = meta_patch.as_object_mut() {
                    patch_object.insert(
                        "isCustomTitle".to_string(),
                        json!(updated_session.is_custom_title()),
                    );
                    if let Some(prompt) = &updated_session.last_prompt {
                        patch_object
                            .entry("lastPrompt".to_string())
                            .or_insert_with(|| json!(prompt));
                    }
                    if let Some(kind) = updated_session.normalized_title_kind() {
                        patch_object
                            .entry("titleKind".to_string())
                            .or_insert_with(|| json!(kind));
                    }
                }
                let meta_event = json!({
                    "type": "session.meta.updated",
                    "agentId": crate::callbacks::MAIN_AGENT_ID,
                    "sessionId": session_id,
                    "title": updated_session.title,
                    "patch": meta_patch,
                });
                self.hub
                    .bus_for(session_id)
                    .publish(&crate::events::EngineEvent::Custom(meta_event));

                if let Some(engine) = self.engine.as_ref() {
                    engine.publish_status_updated(session_id).await;
                }

                let wire_session = format_wire_session(
                    &updated_session,
                    &self.store,
                    self.engine.as_ref(),
                    self.default_workspace_root().as_deref(),
                );

                HttpResponse::ok(&wire_session)
            }
            ("POST", p) if extract_session_fs_action(p).is_some() => {
                let (session_id, action) = extract_session_fs_action(p).unwrap();
                let work_dir = match fs_routes::resolve_session_workdir(&self.store, session_id) {
                    Some(d) => d,
                    None => return HttpResponse::not_found(),
                };

                let body: Value = if req.body.iter().all(|b| b.is_ascii_whitespace()) {
                    json!({})
                } else {
                    match serde_json::from_slice(&req.body) {
                        Ok(v) => v,
                        Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                    }
                };
                match action {
                    "git_status" | "gitStatus" => fs_routes::handle_git_status(&work_dir, &body),
                    "git_status:batch" | "gitStatus:batch" => {
                        fs_routes::handle_git_status_batch(&work_dir, &body)
                    }
                    "pull_request" | "pullRequest" => {
                        fs_routes::handle_pull_request(&work_dir, &body)
                    }
                    "pull_request:batch" | "pullRequest:batch" => {
                        fs_routes::handle_pull_request_batch(&work_dir, &body)
                    }
                    "diff" => fs_routes::handle_diff(&work_dir, &body),
                    "stat" => fs_routes::handle_stat(&work_dir, &body),
                    "stat_many" | "statMany" => fs_routes::handle_stat_many(&work_dir, &body),
                    "mkdir" => fs_routes::handle_mkdir(&work_dir, &body),
                    "list" => fs_routes::handle_list(&work_dir, &body),
                    "list_many" | "listMany" => fs_routes::handle_list_many(&work_dir, &body),
                    "read" => fs_routes::handle_read(&work_dir, &body),
                    "search" => fs_routes::handle_search(&work_dir, &body),
                    "grep" => fs_routes::handle_grep(&work_dir, &body),
                    "open" => fs_routes::handle_open(&work_dir, &body),
                    "reveal" => fs_routes::handle_reveal(&work_dir, &body),
                    "open-in" | "openIn" => fs_routes::handle_open_in(&work_dir, &body),
                    "open-in-apps" | "openInApps" => fs_routes::handle_open_in_apps(&body),
                    _ => HttpResponse::bad_request(format!(
                        "Unsupported filesystem action: {action}"
                    )),
                }
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
