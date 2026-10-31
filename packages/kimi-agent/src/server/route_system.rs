use super::*;

impl HttpServer {
    pub(super) async fn route_system(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v1/health")
            | ("GET", "/health")
            | ("GET", "/healthz")
            | ("GET", "/api/v1/healthz") => HttpResponse::ok(&json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "engine": "kimi-agent-rust",
            })),
            ("GET", "/api/v1/meta") => {
                let dangerous_bypass_auth = self.auth.is_disabled();
                HttpResponse::ok(&json!({
                    "server_version": env!("CARGO_PKG_VERSION"),
                    "capabilities": {
                        "websocket": true,
                        "file_upload": true,
                        "fs_query": true,
                        "mcp": true,
                        "tasks": true,
                        "terminal": true,
                        "subagents": true,
                    },
                    "server_id": self.server_id,
                    "started_at": self.started_at,
                    "open_in_apps": [],
                    "dangerous_bypass_auth": dangerous_bypass_auth,
                    "backend": "rust",
                    "web_title": "Kimi Code",
                    // The user's `[experimental]` flags, not a constant:
                    // clients probe these to gate UI features.
                    "experimental_flags": self.config().await.experimental,
                }))
            }
            ("GET", "/api/v1/config") => {
                let config = self
                    .config_override
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_else(|| {
                        crate::config::KimiConfig::discover()
                            .map(|(c, _)| c)
                            .unwrap_or_default()
                    });
                HttpResponse::ok(&format_config_response(&config))
            }
            ("POST", "/api/v1/config") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let mut config = self
                    .config_override
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_else(|| {
                        crate::config::KimiConfig::discover()
                            .map(|(c, _)| c)
                            .unwrap_or_default()
                    });
                if let Some(dm) = body.get("default_model").and_then(|v| v.as_str()) {
                    config.default_model = Some(dm.to_string());
                }
                // v2 folds `yolo` into the permission mode rather than keeping a
                // separate flag: `patchConfigRequestSchema` carries both
                // `yolo` and `default_permission_mode`, and the handler does
                // `if (yolo === true) defaultPermissionMode = 'yolo'` then drops
                // the key (kap-server/src/routes/config.ts:63-68). Read
                // `default_permission_mode` first so an explicit mode wins.
                let default_permission_mode =
                    body.get("default_permission_mode").and_then(|v| v.as_str());
                let yolo_flag = body.get("yolo").and_then(|v| v.as_bool());
                if let Some(mode) = default_permission_mode {
                    config.default_permission_mode = Some(mode.to_string());
                } else if yolo_flag == Some(true) {
                    config.default_permission_mode = Some("yolo".to_string());
                }
                if yolo_flag == Some(true) {
                    config.agent.yolo = Some(true);
                } else if yolo_flag == Some(false) {
                    config.agent.yolo = Some(false);
                }
                if let Some(plan_mode) = body.get("plan_mode").and_then(|v| v.as_bool()) {
                    config.plan_mode = Some(plan_mode);
                }
                if let Some(default_plan_mode) =
                    body.get("default_plan_mode").and_then(|v| v.as_bool())
                {
                    config.default_plan_mode = Some(default_plan_mode);
                }
                // The Web settings panel posts the whole `patchConfigRequestSchema`
                // and used to have all but four keys accepted with a 200 and
                // dropped — so an experimental-feature toggle could never be
                // persisted. Apply every section the schema declares.
                if let Some(provider) = body.get("default_provider").and_then(|v| v.as_str()) {
                    config.default_provider = Some(provider.to_string());
                }
                if let Some(permission) = body.get("permission") {
                    match serde_json::from_value::<crate::config::PermissionConfig>(
                        permission.clone(),
                    ) {
                        Ok(parsed) => config.permission = Some(parsed),
                        Err(error) => {
                            // Never silently drop a section the user just
                            // edited: report it so the UI can surface a form
                            // error instead of pretending it saved.
                            return HttpResponse::bad_request(format!(
                                "Invalid [permission] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(mode) = body.get("default_permission_mode").and_then(|v| v.as_str()) {
                    config.default_permission_mode = Some(mode.to_string());
                }
                if let Some(thinking) = body.get("thinking") {
                    match serde_json::from_value::<crate::config::ThinkingConfig>(thinking.clone())
                    {
                        Ok(parsed) => config.thinking = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid [thinking] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(loop_control) = body.get("loop_control") {
                    match serde_json::from_value::<crate::config::LoopControlConfig>(
                        loop_control.clone(),
                    ) {
                        Ok(parsed) => config.loop_control = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid [loop_control] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(background) = body.get("background") {
                    match serde_json::from_value::<crate::config::BackgroundConfig>(
                        background.clone(),
                    ) {
                        Ok(parsed) => config.background = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid [background] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(hooks) = body.get("hooks") {
                    match serde_json::from_value::<Vec<crate::permission::HookDef>>(hooks.clone()) {
                        Ok(parsed) => config.hooks = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid [hooks] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(services) = body.get("services") {
                    match serde_json::from_value::<crate::config::ServicesConfig>(services.clone())
                    {
                        Ok(parsed) => config.services = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid [services] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(experimental) = body.get("experimental") {
                    match serde_json::from_value::<
                        std::collections::HashMap<String, crate::config::ExperimentalValue>,
                    >(experimental.clone())
                    {
                        Ok(parsed) => config.experimental = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid [experimental] section: {error}"
                            ));
                        }
                    }
                }
                if let Some(merge) = body
                    .get("merge_all_available_skills")
                    .and_then(|v| v.as_bool())
                {
                    config.merge_all_available_skills = Some(merge);
                }
                if let Some(dirs) = body.get("extra_skill_dirs") {
                    match serde_json::from_value::<Vec<String>>(dirs.clone()) {
                        Ok(parsed) => config.extra_skill_dirs = parsed,
                        Err(error) => {
                            return HttpResponse::bad_request(format!(
                                "Invalid extra_skill_dirs: {error}"
                            ));
                        }
                    }
                }
                if let Some(telemetry) = body.get("telemetry").and_then(|v| v.as_bool()) {
                    config.telemetry = Some(telemetry);
                }
                *self.config_override.lock().await = Some(config.clone());
                HttpResponse::ok(&format_config_response(&config))
            }
            ("POST", "/api/v1/config:reload") | ("POST", "/api/v1/config/reload") => {
                *self.config_override.lock().await = None;
                // Re-read the file the server pinned when it has one — the same
                // precedence the provider routes use (`provider_write.rs`
                // `effective_config`, `provider_refresh.rs`) — so a reload picks
                // up the file this server is actually using rather than a
                // re-discovery that could land on a different one.
                let config = match self.config_write_path() {
                    Some(path) => crate::config::KimiConfig::from_file(&path).unwrap_or_default(),
                    None => crate::config::KimiConfig::discover()
                        .map(|(c, _)| c)
                        .unwrap_or_default(),
                };
                // Published even when the list is empty: the entity is an upsert,
                // so `warnings: []` is what clears the advisory a previously
                // broken `config.toml` left on the client.
                self.publish_config_warnings(&config.config_warnings);
                HttpResponse::ok(&json!({
                    "status": "reloaded",
                    "config": format_config_response(&config)
                }))
            }
            ("GET", "/api/v1/runtime") | ("GET", "/api/v1/runtime/info") => {
                HttpResponse::ok(&json!({
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                    "family": std::env::consts::FAMILY,
                    "platform": std::env::consts::OS,
                    "backend": "rust",
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": std::process::id(),
                }))
            }
            // Static advertisement, not a probe: the list is a compile-time
            // constant, so it does not reflect per-process availability (e.g. a
            // build without the workflow engine still reports the same set).
            // The detail route below answers from the same list.
            // The bundle's `listCapabilities` feeds these straight into
            // `capabilities.filter(c => c.supported)`, so the elements have to be
            // the same objects the detail route serves — a bare string array made
            // `supported` undefined and the plugin panel's capability rows
            // rendered empty.
            ("GET", "/api/v1/capabilities") => {
                let capabilities: Vec<Value> = NATIVE_CAPABILITY_IDS
                    .iter()
                    .map(|id| native_capability_wire(id))
                    .collect();
                HttpResponse::ok(&json!({ "capabilities": capabilities }))
            }
            // One capability's readiness. Native built-ins are compiled in, so
            // a known id is genuinely `ready`; an unknown id is a 404
            // (kap-server's `CAPABILITY_NOT_FOUND`), not a fabricated record.
            ("GET", p) if p.starts_with("/api/v1/capabilities/") => {
                let id = p.trim_start_matches("/api/v1/capabilities/");
                if NATIVE_CAPABILITY_IDS.contains(&id) {
                    HttpResponse::ok(&native_capability_wire(id))
                } else {
                    HttpResponse::not_found()
                }
            }
            // kap-server's install action: the native engine has nothing to
            // install (every capability ships compiled in), so the honest
            // answer is "unsupported", not a fake install that claims to run.
            ("POST", p) if p.starts_with("/api/v1/capabilities/") => HttpResponse::bad_request(
                "CAPABILITY_UNSUPPORTED: this standalone engine ships every capability built in; there is no installer to run",
            ),
            ("GET", "/api/v1/connections") => {
                let connections: Vec<Value> = self
                    .hub
                    .connections()
                    .into_iter()
                    .map(|conn| {
                        let connected_at =
                            chrono::DateTime::from_timestamp_millis(conn.connected_at as i64)
                                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                                .unwrap_or_else(|| self.started_at.clone());
                        json!({
                            "id": format!("conn_{}", conn.id),
                            "connected_at": connected_at,
                            "remote_address": conn.remote_address,
                            "user_agent": conn.user_agent,
                            "has_client_hello": conn.has_client_hello,
                            "subscriptions": conn.subscriptions,
                        })
                    })
                    .collect();
                HttpResponse::ok(&json!({ "connections": connections }))
            }
            // Unregistered (so a 404) on a non-loopback bind unless the server
            // was started with `--allow-remote-shutdown`.
            ("POST", "/api/v1/shutdown") if self.shutdown_enabled() => {
                self.request_shutdown();
                HttpResponse::ok(&json!({
                    "status": "shutting_down",
                    "message": "Kimi agent native server is shutting down"
                }))
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
