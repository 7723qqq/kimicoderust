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
                    .or_else(|| body.get("source"))
                    .and_then(|v| v.as_str());
                let Some(id) = id else {
                    return HttpResponse::bad_request("Missing plugin id");
                };

                match self.plugin_manager.install_plugin_from(id) {
                    Ok(Some((id, info))) => {
                        self.publish_plugin_changed("install", &id);
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
                    "enable" => match self.plugin_manager.set_plugin_enabled(id, true) {
                        Ok(true) => {
                            self.publish_plugin_changed("enable", id);

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
                            self.publish_plugin_changed("disable", id);
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
                            self.publish_plugin_changed("remove", id);
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

                let _ = self.oauth_manager.logout(provider);
                HttpResponse::ok(&json!({ "logged_out": true, "provider": provider }))
            }
            ("GET", "/api/v1/oauth/usage") => {
                let provider = req.query_param("provider").unwrap_or_else(|| "kimi".into());
                HttpResponse::ok(&self.oauth_manager.get_usage(&provider).await)
            }

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
