use super::*;

impl HttpServer {
    pub(super) async fn route_plugins_oauth(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v1/plugins/marketplace") => {
                let entries = self.plugin_manager.list_marketplace();
                HttpResponse::ok(&json!({ "entries": entries }))
            }
            ("GET", "/api/v1/plugins") => {
                let plugins = self.plugin_manager.list_plugins();
                HttpResponse::ok(&json!({ "plugins": plugins }))
            }
            ("POST", "/api/v1/plugins") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let id = body
                    .get("id")
                    .or_else(|| body.get("name"))
                    // The official Web bundle's `installPlugin` posts the
                    // catalog entry's `source` under a `source` key;
                    // `install_plugin_from` already matches a catalog `source`
                    // as well as an id, so reading it here is all that was
                    // missing — the request used to 400 as "Missing plugin id".
                    .or_else(|| body.get("source"))
                    .and_then(|v| v.as_str());
                let Some(id) = id else {
                    return HttpResponse::bad_request("Missing plugin id");
                };
                // Install from a catalog id, a catalog `source`, a local root,
                // or a remote archive URL. A remote source is downloaded here,
                // so the recorded install has content behind it; an unknown id
                // is a 404, and the recorded version comes from the catalog or
                // the archive's own manifest, never from a hardcoded literal.
                // Install from a catalog id, a catalog `source`, a local root,
                // or a remote archive URL. A remote source is downloaded here,
                // so the recorded install has content behind it; an unknown id
                // is a 404, and the recorded version comes from the catalog or
                // the archive's own manifest, never from a hardcoded literal.
                match self.plugin_manager.install_plugin_from(id) {
                    Ok(Some((id, info))) => {
                        self.publish_plugin_changed();
                        HttpResponse::ok(&json!({
                            "id": id,
                            "enabled": info.enabled,
                            "version": info.version,
                            "installed": true
                        }))
                    }
                    Ok(None) => HttpResponse::not_found(),
                    Err(e) => HttpResponse::bad_request(e),
                }
            }
            ("GET", "/api/v1/skills") => {
                let config = self.config().await;
                let merge = config.resolve_merge_all_available_skills();
                let extra = config.extra_skill_dirs_paths();
                let plugin_dirs = self.plugin_manager.plugin_skill_dirs();
                let skills = crate::skills::scan_all_skills_with_extra_merge_and_plugins(
                    None,
                    &extra,
                    &plugin_dirs,
                    merge,
                );
                HttpResponse::ok(&json!({ "skills": skills }))
            }
            ("POST", "/api/v1/acp") => {
                let body_str = match std::str::from_utf8(&req.body) {
                    Ok(s) => s,
                    Err(_) => return HttpResponse::bad_request("Invalid UTF-8 payload"),
                };
                // Attach the server engine when one is configured: a bare
                // per-request AcpServer has no model and no auth state, so
                // every session method but `initialize` used to fail with
                // -32000 and every notification was dropped. Uses the shared
                // (factory-preserving) constructor: the host-factory slot is
                // engine-global and must keep pointing at the dedicated stdio
                // ACP server. No notification sink is set on purpose — plain
                // HTTP request/response has no channel for server-push
                // `session/update`s; the final result still travels in the
                // JSON-RPC response.
                let acp_server = match &self.engine {
                    Some(engine) => crate::acp::AcpServer::with_shared_engine(
                        self.store.clone(),
                        engine.clone(),
                    ),
                    None => crate::acp::AcpServer::new(self.store.clone()),
                };
                let response = acp_server.handle_message(body_str).await;
                if let Some(resp) = response {
                    let val = serde_json::to_value(&resp).unwrap_or(Value::Null);
                    HttpResponse::ok(&val)
                } else {
                    HttpResponse::ok(&json!({ "result": "ok" }))
                }
            }
            ("POST", p) if p.starts_with("/api/v1/plugins/") => {
                let tail = p.strip_prefix("/api/v1/plugins/").unwrap_or_default();
                let (id, action) = if let Some(id) = tail.strip_suffix(":enable") {
                    (id, "enable")
                } else if let Some(id) = tail.strip_suffix("/enable") {
                    (id, "enable")
                } else if let Some(id) = tail.strip_suffix(":disable") {
                    (id, "disable")
                } else if let Some(id) = tail.strip_suffix("/disable") {
                    (id, "disable")
                } else if let Some(id) = tail.strip_suffix(":remove") {
                    (id, "remove")
                } else if let Some(id) = tail.strip_suffix("/remove") {
                    (id, "remove")
                } else {
                    return HttpResponse::bad_request("Unsupported plugin action");
                };

                match action {
                    // `Ok(false)` = unknown plugin id: neither installed nor in
                    // the catalog. Answer 404 rather than recording a fake entry.
                    "enable" => match self.plugin_manager.set_plugin_enabled(id, true) {
                        Ok(true) => {
                            self.publish_plugin_changed();
                            // v2 #3963: the toggle reports the resulting
                            // enabled set, so telemetry can attribute later
                            // turns to the plugins that were loaded.
                            self.emit_session_telemetry(
                                "plugin_toggle",
                                json!({
                                    "plugin_id": id,
                                    "enabled": true,
                                    "enabled_plugins": self.plugin_manager.enabled_plugin_ids(),
                                }),
                            );
                            HttpResponse::ok(
                                &json!({ "ok": true, "pluginId": id, "enabled": true }),
                            )
                        }
                        Ok(false) => HttpResponse::not_found(),
                        Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                    },
                    "disable" => match self.plugin_manager.set_plugin_enabled(id, false) {
                        Ok(true) => {
                            self.publish_plugin_changed();
                            self.emit_session_telemetry(
                                "plugin_toggle",
                                json!({
                                    "plugin_id": id,
                                    "enabled": false,
                                    "enabled_plugins": self.plugin_manager.enabled_plugin_ids(),
                                }),
                            );
                            HttpResponse::ok(
                                &json!({ "ok": true, "pluginId": id, "enabled": false }),
                            )
                        }
                        Ok(false) => HttpResponse::not_found(),
                        Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                    },
                    "remove" => match self.plugin_manager.remove_plugin(id) {
                        Ok(true) => {
                            self.publish_plugin_changed();
                            HttpResponse::ok(
                                &json!({ "ok": true, "pluginId": id, "removed": true }),
                            )
                        }
                        Ok(false) => HttpResponse::not_found(),
                        Err(e) => HttpResponse::internal_error(format!("Database error: {e}")),
                    },
                    _ => HttpResponse::bad_request("Unsupported plugin action"),
                }
            }
            ("POST", "/api/v1/oauth/login") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => json!({}),
                };
                let provider = body
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("kimi");
                let region = body.get("region").and_then(|v| v.as_str());
                let flow = self
                    .oauth_manager
                    .clone()
                    .start_login(provider, region)
                    .await;
                HttpResponse::ok(&oauth_flow_wire(Some(&flow), provider))
            }
            ("GET", "/api/v1/oauth/login") => {
                let provider = req.query_param("provider").unwrap_or_else(|| "kimi".into());
                let flow = self.oauth_manager.get_flow(&provider);
                HttpResponse::ok(&oauth_flow_wire(flow.as_ref(), &provider))
            }
            ("DELETE", "/api/v1/oauth/login") => {
                let provider = req.query_param("provider").unwrap_or_else(|| "kimi".into());
                let cancelled = self.oauth_manager.cancel_login(&provider);
                // The bundle's `cancelOAuthLogin` reads `status` alongside
                // `cancelled`; the terminal status of a cancelled flow is
                // `cancelled` when it really was, and the flow's own status
                // otherwise (nothing to cancel).
                let status = if cancelled {
                    "cancelled".to_string()
                } else {
                    self.oauth_manager
                        .get_flow(&provider)
                        .map(|flow| flow.status)
                        .unwrap_or_else(|| "cancelled".to_string())
                };
                HttpResponse::ok(&json!({ "cancelled": cancelled, "status": status }))
            }
            ("POST", "/api/v1/oauth/logout") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => json!({}),
                };
                let provider = body
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("kimi");
                // Wire shape is the protocol's `oauthLogoutResponseSchema`
                // (`{ logged_out: true, provider }`, v2 `authService.ts:258`):
                // always the literal — the nothing-to-drop case is a no-op
                // success upstream. The manager's internal "was anything
                // dropped" bool stays off the wire; the shipped Web bundle
                // reads `logged_out` (snake_case).
                let _ = self.oauth_manager.logout(provider);
                HttpResponse::ok(&json!({ "logged_out": true, "provider": provider }))
            }
            ("GET", "/api/v1/oauth/usage") => {
                let provider = req.query_param("provider").unwrap_or_else(|| "kimi".into());
                HttpResponse::ok(&self.oauth_manager.get_usage(&provider).await)
            }
            // Resolve the client region (mainland-cn/global), mirroring v2
            // `resolveKimiRegion`'s precedence: env host > configured host/key >
            // install-channel marker > default. The standalone server does not
            // persist an oauth ref of its own, so the configured pair is `None`
            // here — env and the marker still decide.
            ("GET", "/api/v1/oauth/region") => {
                let env_host = std::env::var("KIMI_CODE_OAUTH_HOST")
                    .ok()
                    .or_else(|| std::env::var("KIMI_OAUTH_HOST").ok())
                    .filter(|v| !v.trim().is_empty());
                let home = crate::workflow::kimi_home().unwrap_or_else(|| PathBuf::from("."));
                let region = crate::server::oauth::resolve_kimi_region(
                    env_host.as_deref(),
                    None,
                    None,
                    &home,
                );
                HttpResponse::ok(&json!({ "region": region }))
            }
            ("GET", "/api/v1/oauth/user") | ("GET", "/api/v1/oauth/userinfo") => {
                let provider = req.query_param("provider").unwrap_or_else(|| "kimi".into());
                HttpResponse::ok(&self.oauth_manager.get_user_info(&provider).await)
            }
            ("GET", "/api/v1/auth") => {
                let config = self.config().await;
                let has_cached_token = |provider: &str| self.has_cached_token(provider);
                HttpResponse::ok(&crate::server::model_catalog::auth_summary(
                    &config,
                    &has_cached_token,
                ))
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}
