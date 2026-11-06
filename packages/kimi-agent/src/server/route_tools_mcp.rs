use super::*;

impl HttpServer {
    pub(super) async fn route_tools_mcp(
        &self,
        method: &str,
        path: &str,
        req: &HttpRequest,
    ) -> HttpResponse {
        let resp = match (method, path) {
            ("GET", "/api/v1/tools") => {
                let session_id = req.query_param("session_id");
                let mut tools: Vec<Value> = crate::tools::core_tool_defs::all_native_tool_defs()
                    .into_iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "description": d.description,
                            "input_schema": d.input_schema,
                            "source": "builtin",
                            "active": true,
                        })
                    })
                    .collect();

                let mcp_tools = self.mcp_manager.list_tool_infos().await;
                for t in mcp_tools {
                    let server_id = if t.name.starts_with("mcp__") {
                        t.name.split("__").nth(1)
                    } else {
                        None
                    };
                    tools.push(json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                        "source": "mcp",
                        "mcp_server_id": server_id,
                        "active": true,
                    }));
                }

                let mut resp = json!({ "tools": tools });
                if let Some(sid) = session_id {
                    resp["session_id"] = json!(sid);
                }
                HttpResponse::ok(&resp)
            }
            // Remote-control runtime (#3594).
            //
            // Remote Control (#3594): the runtime behind this endpoint is now
            // real (`server/remote_control.rs` — device registration, a relay
            // WebSocket channel, reverse HTTP proxy and heartbeats). POST
            // starts it (the caller must supply the Kimi login refresh token —
            // the standalone server holds none of its own), GET reports the
            // live status. The old `501 + available:false` responses existed
            // because there was genuinely nothing listening behind them.
            ("GET", "/api/v1/remote-control") => {
                let runtime = self.remote_control_runtime.lock().await;
                match runtime.as_ref() {
                    Some(handle) => {
                        let st = handle.status();
                        HttpResponse::ok(&json!({
                            "enabled": st.enabled,
                            "state": st.state,
                            "url": st.url,
                            "device_id": st.device_id,
                            "device_name": st.device_name,
                            "available": true,
                            "error": st.error,
                        }))
                    }
                    None => {
                        let st = self.remote_control_state.lock().await.clone();
                        let conflict =
                            self.remote_control_logout_gate
                                .active_since()
                                .map(|started_at| {
                                    super::RemoteControlConflictWire {
                                pid: std::process::id(),
                                local_origin:
                                    crate::server::remote_control::REMOTE_CONTROL_LOGOUT_GATE_ORIGIN
                                        .to_string(),
                                device_id: self
                                    .stored_remote_control_device_id()
                                    .unwrap_or_default(),
                                url: String::new(),
                                started_at,
                            }
                                });
                        let mut payload = json!({
                            "enabled": false,
                            "state": st.state,
                            "url": Value::Null,
                            "device_id": st.device_id,
                            "device_name": st.device_name,
                            "available": false,
                            "reason": REMOTE_CONTROL_UNAVAILABLE,
                            "error": st.error,
                        });
                        if let Some(conflict) = conflict {
                            payload["conflict"] =
                                serde_json::to_value(conflict).unwrap_or(Value::Null);
                        }
                        HttpResponse::ok(&payload)
                    }
                }
            }
            ("POST", "/api/v1/remote-control") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                if body.get("enabled").and_then(|v| v.as_bool()).is_none() {
                    return HttpResponse::bad_request("Missing 'enabled' field");
                }
                let enabled = body["enabled"].as_bool().unwrap_or(false);
                if !enabled {
                    // Stop the runtime if one is live and report the off state.
                    if let Some(handle) = self.remote_control_runtime.lock().await.take() {
                        handle.close().await;
                    }
                    {
                        let mut st = self.remote_control_state.lock().await;
                        *st = RemoteControlStatusWire::off();
                    }
                    // v2 #3897 `remote_control_toggle`
                    // (kap-server/src/routes/remoteControl.ts:84).
                    self.emit_session_telemetry(
                        "remote_control_toggle",
                        serde_json::json!({ "enabled": false, "outcome": "ok" }),
                    );
                    return HttpResponse::ok(&json!({
                        "enabled": false,
                        "state": "off",
                        "url": Value::Null,
                        "available": false,
                    }));
                }
                // Start (or report the already-running) runtime. The in-process
                // slot is the start gate: a logout that holds it or a runtime
                // that already owns it is reported as 40928 with the holder
                // (upstream `REMOTE_CONTROL_ALREADY_RUNNING`).
                if let Some(started_at) = self.remote_control_logout_gate.active_since() {
                    self.emit_session_telemetry(
                        "remote_control_toggle",
                        serde_json::json!({ "enabled": true, "outcome": "already_running" }),
                    );
                    return remote_control_already_running(
                        req,
                        started_at,
                        crate::server::remote_control::REMOTE_CONTROL_LOGOUT_GATE_ORIGIN,
                        self.stored_remote_control_device_id().unwrap_or_default(),
                        String::new(),
                    );
                }
                let mut slot = self.remote_control_runtime.lock().await;
                if let Some(handle) = slot.as_ref() {
                    self.emit_session_telemetry(
                        "remote_control_toggle",
                        serde_json::json!({ "enabled": true, "outcome": "already_running" }),
                    );
                    return remote_control_already_running(
                        req,
                        handle.started_at(),
                        handle.local_origin(),
                        handle.device_id().to_string(),
                        handle.url().to_string(),
                    );
                }
                // The Kimi login refresh token is the WS-upgrade credential at
                // the relay; the standalone server holds none of its own, so
                // the caller (the web UI, which owns the user session) passes
                // it in. Refusing without it is honest: an empty token would
                // fail the upgrade mid-handshake anyway.
                let refresh_token = body
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if refresh_token.trim().is_empty() {
                    return HttpResponse::bad_request(
                        "Field 'refresh_token' is required: remote control authenticates to the relay with the Kimi login credential",
                    );
                }
                // Stable device id, persisted in the session store (TS:
                // `createKimiDeviceId(homeDir)`).
                let device_id = match self.stored_remote_control_device_id() {
                    Some(id) => id,
                    None => {
                        let id = new_device_id();
                        let _ = self
                            .store
                            .put_state("remote-control", "device_id", &json!(id));
                        id
                    }
                };
                // Forward target: this server itself, as addressed by the
                // caller (the Host header carries the loopback host:port).
                let local_base_url =
                    format!("http://{}", req.header("host").unwrap_or("127.0.0.1"));
                let local_server_token = self.auth.token().unwrap_or_default().to_string();
                let options = crate::server::remote_control::RemoteControlOptions {
                    device_id: device_id.clone(),
                    local_base_url,
                    local_server_token,
                    refresh_token,
                    region_relay_origin: remote_control_relay_origin(&self.config().await),
                    ..Default::default()
                };
                let handle = crate::server::remote_control::RemoteControlRuntime::start(options);
                let st = handle.status();
                *slot = Some(handle);
                // v2 reports `error` for a failed start (routes/remoteControl.ts:105);
                // the runtime encodes a failed start in its status.
                let outcome = if st.error.is_some() { "error" } else { "ok" };
                self.emit_session_telemetry(
                    "remote_control_toggle",
                    serde_json::json!({ "enabled": true, "outcome": outcome }),
                );
                HttpResponse::ok(&json!({
                    "enabled": st.enabled,
                    "state": st.state,
                    "url": st.url,
                    "device_id": st.device_id,
                    "device_name": st.device_name,
                    "available": true,
                    "error": st.error,
                }))
            }
            ("GET", "/api/v1/remote-control/devices") => {
                let device_auth = match remote_control_device_auth(&self.config().await) {
                    Ok(auth) => auth,
                    Err(error) => return HttpResponse::internal_error(error),
                };
                match self
                    .remote_control_devices
                    .list_devices(&self.oauth_manager, &device_auth)
                    .await
                {
                    Ok(list) => {
                        HttpResponse::ok(&serde_json::to_value(list).unwrap_or(Value::Null))
                    }
                    Err(error) => {
                        tracing::warn!(%error, "remote-control device request failed");
                        HttpResponse::internal_error(error)
                    }
                }
            }
            ("PATCH", p) if p.starts_with("/api/v1/remote-control/devices/") => {
                let raw_id = p
                    .strip_prefix("/api/v1/remote-control/devices/")
                    .unwrap_or_default();
                if raw_id.is_empty() || raw_id.contains('/') {
                    return provider_validation_error(
                        "Invalid remote-control device id".to_string(),
                    );
                }
                let device_id = crate::server::router::decode_path_segment(raw_id);
                if device_id.is_empty() {
                    return provider_validation_error(
                        "Invalid remote-control device id".to_string(),
                    );
                }
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return provider_validation_error("Invalid JSON payload".to_string()),
                };
                let alias = match crate::server::remote_control::normalize_device_alias(
                    body.get("alias").unwrap_or(&Value::Null),
                ) {
                    Ok(alias) => alias,
                    Err(message) => return provider_validation_error(message),
                };
                let device_auth = match remote_control_device_auth(&self.config().await) {
                    Ok(auth) => auth,
                    Err(error) => return HttpResponse::internal_error(error),
                };
                match self
                    .remote_control_devices
                    .rename_device(&self.oauth_manager, &device_auth, &device_id, &alias)
                    .await
                {
                    Ok(()) => HttpResponse::ok(&json!({"ok": true})),
                    Err(error) => {
                        tracing::warn!(%error, "remote-control device request failed");
                        remote_control_internal_error(req, error)
                    }
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/remote-control/devices/")
                    && p.ends_with("/deactivate") =>
            {
                let raw_id = p
                    .strip_prefix("/api/v1/remote-control/devices/")
                    .and_then(|rest| rest.strip_suffix("/deactivate"))
                    .unwrap_or_default();
                if raw_id.is_empty() || raw_id.contains('/') {
                    return provider_validation_error(
                        "Invalid remote-control device id".to_string(),
                    );
                }
                let device_id = crate::server::router::decode_path_segment(raw_id);
                if device_id.is_empty() {
                    return provider_validation_error(
                        "Invalid remote-control device id".to_string(),
                    );
                }
                let device_auth = match remote_control_device_auth(&self.config().await) {
                    Ok(auth) => auth,
                    Err(error) => return HttpResponse::internal_error(error),
                };

                // Same slot lock as DELETE: a local deactivate cannot race a
                // start, and the cloud mutation happens after the tunnel is
                // stopped (upstream disables before and after the cloud call).
                let mut runtime = self.remote_control_runtime.lock().await;
                let active_id = runtime
                    .as_ref()
                    .map(|handle| handle.device_id().to_string());
                let is_local = active_id.as_deref() == Some(device_id.as_str())
                    || self.stored_remote_control_device_id().as_deref()
                        == Some(device_id.as_str());
                if is_local {
                    if let Some(handle) = runtime.take() {
                        handle.close().await;
                    }
                    *self.remote_control_state.lock().await = RemoteControlStatusWire::off();
                }
                let deactivated = self
                    .remote_control_devices
                    .deactivate_device(&self.oauth_manager, &device_auth, &device_id)
                    .await;
                if is_local {
                    *self.remote_control_state.lock().await = RemoteControlStatusWire::off();
                }
                drop(runtime);
                match deactivated {
                    Ok(()) => HttpResponse::ok(&json!({"ok": true})),
                    Err(error) => {
                        tracing::warn!(%error, "remote-control device request failed");
                        remote_control_internal_error(req, error)
                    }
                }
            }
            ("DELETE", p) if p.starts_with("/api/v1/remote-control/devices/") => {
                let raw_id = p
                    .strip_prefix("/api/v1/remote-control/devices/")
                    .unwrap_or_default();
                if raw_id.is_empty() || raw_id.contains('/') {
                    return HttpResponse::bad_request("Invalid remote-control device id");
                }
                let device_id = crate::server::router::decode_path_segment(raw_id);
                if device_id.is_empty() {
                    return HttpResponse::bad_request("Invalid remote-control device id");
                }
                let device_auth = match remote_control_device_auth(&self.config().await) {
                    Ok(auth) => auth,
                    Err(error) => return HttpResponse::internal_error(error),
                };

                // Keep the runtime slot locked across a current-device delete.
                // POST /remote-control takes the same slot before starting, so a
                // deleted local device cannot be re-registered behind the cloud
                // mutation. This is the fork-runtime equivalent of upstream's
                // start gate; no second filesystem lock is introduced here.
                let mut runtime = self.remote_control_runtime.lock().await;
                let active_id = runtime
                    .as_ref()
                    .map(|handle| handle.device_id().to_string());
                let stored_id = self
                    .store
                    .get_state("remote-control", "device_id")
                    .ok()
                    .flatten()
                    .and_then(|value| value.as_str().map(str::to_string));
                let is_local = active_id.as_deref() == Some(device_id.as_str())
                    || stored_id.as_deref() == Some(device_id.as_str());
                if is_local {
                    if let Some(handle) = runtime.take() {
                        handle.close().await;
                    }
                    *self.remote_control_state.lock().await = RemoteControlStatusWire::off();
                }
                let deleted = self
                    .remote_control_devices
                    .delete_device(&self.oauth_manager, &device_auth, &device_id)
                    .await;
                if is_local {
                    // Upstream disables once more after the cloud mutation. The
                    // slot lock already excludes new starts; repeating the state
                    // reset preserves that externally visible postcondition.
                    *self.remote_control_state.lock().await = RemoteControlStatusWire::off();
                }
                drop(runtime);
                match deleted {
                    Ok(()) => HttpResponse::ok(&json!({"ok": true})),
                    Err(error) => {
                        tracing::warn!(%error, "remote-control device request failed");
                        HttpResponse::internal_error(error)
                    }
                }
            }
            // MCP endpoints
            ("GET", "/api/v1/mcp")
            | ("GET", "/api/v1/mcp/servers")
            | ("GET", "/api/v2/mcp/servers") => {
                let servers = self.mcp_manager.server_entries().await;
                HttpResponse::ok(&json!({ "servers": servers }))
            }
            ("GET", "/api/v1/mcp/tools") => {
                let tools = self.mcp_manager.list_tool_infos().await;
                HttpResponse::ok(&json!({ "tools": tools }))
            }
            // On-demand reconnect for a configured server (v2
            // `reconnectAndJoin` surface): truthful failed/connected status.
            ("POST", p)
                if p.starts_with("/api/v1/mcp/servers/")
                    && (p.ends_with(":reconnect") || p.ends_with(":restart")) =>
            {
                let name = p
                    .strip_prefix("/api/v1/mcp/servers/")
                    .and_then(|rest| {
                        rest.strip_suffix(":reconnect")
                            .or_else(|| rest.strip_suffix(":restart"))
                    })
                    .unwrap_or_default();
                match self.mcp_manager.reconnect(name).await {
                    Ok(()) => {
                        let servers = self.mcp_manager.server_entries().await;
                        let entry = servers.iter().find(|s| s.name == name);
                        HttpResponse::ok(
                            &json!({ "reconnected": true, "restarting": true, "server": entry }),
                        )
                    }
                    Err(e) => HttpResponse::internal_error(e),
                }
            }
            ("POST", "/api/v1/mcp/servers:test") | ("POST", "/api/v2/mcp/servers:test") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let name = body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("test-server");
                // Report the real status of the named configured server rather
                // than a fabricated success.
                let entries = self.mcp_manager.server_entries().await;
                match entries.iter().find(|entry| entry.name == name) {
                    Some(entry) => HttpResponse::ok(&json!({
                        "ok": entry.status == "connected",
                        "name": name,
                        "connected": entry.status == "connected",
                        "transport": entry.transport,
                        "status": entry.status,
                        "tool_count": entry.tool_count,
                        "error": entry.error,
                    })),
                    None => HttpResponse::ok(&json!({
                        "ok": false,
                        "name": name,
                        "connected": false,
                        "status": "not_found",
                    })),
                }
            }
            ("POST", "/api/v1/mcp/servers:inspect") | ("POST", "/api/v2/mcp/servers:inspect") => {
                let body: Value = match serde_json::from_slice(&req.body) {
                    Ok(v) => v,
                    Err(_) => return HttpResponse::bad_request("Invalid JSON payload"),
                };
                let name = body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if let Some(tools) = self.mcp_manager.inspect_server(name).await {
                    HttpResponse::ok(&json!({ "name": name, "tools": tools }))
                } else {
                    HttpResponse::not_found()
                }
            }
            ("DELETE", p)
                if p.starts_with("/api/v1/mcp/servers/")
                    || p.starts_with("/api/v2/mcp/servers/") =>
            {
                let name = p.rsplit('/').next().unwrap_or_default();
                if self.mcp_manager.remove_server(name).await {
                    HttpResponse::ok(&json!({ "removed": true, "name": name }))
                } else {
                    HttpResponse::not_found()
                }
            }
            // The browser lands here after the authorization server's
            // redirect. Unauthenticated by design (a browser carries no bearer
            // token): the single-use `state` minted by `auth:begin-auth-code`
            // is the credential this route checks, and the code is worthless
            // without the PKCE verifier held by the pending flow.
            ("GET", "/api/v1/mcp/auth/callback") | ("GET", "/api/v2/mcp/auth/callback") => {
                let state = req.query_param("state").unwrap_or_default();
                let Some(service) = self.mcp_oauth.lock().await.clone() else {
                    return html_result(
                        503,
                        "Authorization unavailable",
                        "MCP OAuth is not configured on this server.",
                    );
                };
                if let Some(error) = req.query_param("error").filter(|value| !value.is_empty()) {
                    let description = req.query_param("error_description").unwrap_or_default();
                    let message = if description.is_empty() {
                        format!("The provider returned \"{error}\".")
                    } else {
                        format!("The provider returned \"{error}\": {description}")
                    };
                    if let Some(flow) = self.mcp_auth_code.take_by_state(&state).await {
                        self.mcp_auth_code
                            .finish(
                                &flow.flow_id,
                                crate::mcp::oauth::auth_code::AuthCodeStatus {
                                    done: true,
                                    authenticated: false,
                                    key: None,
                                    error: Some(message.clone()),
                                },
                            )
                            .await;
                    }
                    return html_result(400, "Authorization failed", &message);
                }
                let Some(code) = req.query_param("code").filter(|value| !value.is_empty()) else {
                    return html_result(
                        400,
                        "Authorization failed",
                        "The callback carried no authorization code.",
                    );
                };
                let Some(flow) = self.mcp_auth_code.take_by_state(&state).await else {
                    return html_result(
                        400,
                        "Authorization failed",
                        "This authorization request is unknown or has expired. Start the login again.",
                    );
                };
                let outcome = crate::mcp::oauth::auth_code::exchange_code(
                    &reqwest::Client::new(),
                    &flow,
                    &code,
                )
                .await
                .and_then(|tokens| {
                    let key = crate::mcp::oauth::mcp_oauth_store_key(&flow.name, &flow.server_url)?;
                    service.store_tokens(&key, &tokens)?;
                    Ok(key)
                });
                match outcome {
                    Ok(key) => {
                        self.mcp_auth_code
                            .finish(
                                &flow.flow_id,
                                crate::mcp::oauth::auth_code::AuthCodeStatus {
                                    done: true,
                                    authenticated: true,
                                    key: Some(key),
                                    error: None,
                                },
                            )
                            .await;
                        html_result(
                            200,
                            "Authorization complete",
                            "You can close this window and return to Kimi Code.",
                        )
                    }
                    Err(error) => {
                        self.mcp_auth_code
                            .finish(
                                &flow.flow_id,
                                crate::mcp::oauth::auth_code::AuthCodeStatus {
                                    done: true,
                                    authenticated: false,
                                    key: None,
                                    error: Some(error.clone()),
                                },
                            )
                            .await;
                        html_result(400, "Authorization failed", &error)
                    }
                }
            }
            ("GET", "/api/v1/mcp/auth-statuses") | ("GET", "/api/v2/mcp/auth-statuses") => {
                // `authenticated` is a real probe, not key presence: the token
                // is fetched through the OAuth service, which refreshes an
                // expired one and returns `None` when the credential cannot be
                // used. A stored-but-unusable key therefore reports
                // `needs-auth` (v2 `McpServerStatus`) instead of a fabricated
                // `true`. Shape note: v2 returned `data: McpServerAuthStatus[]`;
                // this server keeps its own `{statuses:{<key>:{...}}}` object,
                // which its clients and tests already consume.
                let service = self.mcp_oauth.lock().await.clone();
                let mut statuses = serde_json::Map::new();
                if let Some(service) = service {
                    for key in service.list_keys() {
                        let authenticated = service.access_token(&key).await.is_some();
                        statuses.insert(
                            key,
                            json!({
                                "authenticated": authenticated,
                                "status": if authenticated { "authenticated" } else { "needs-auth" },
                            }),
                        );
                    }
                }
                HttpResponse::ok(&json!({ "statuses": statuses }))
            }
            ("POST", p)
                if p.starts_with("/api/v1/mcp/auth:") || p.starts_with("/api/v2/mcp/auth:") =>
            {
                let action = p.rsplit(':').next().unwrap_or_default();
                let body: Value = if req.body.is_empty() {
                    json!({})
                } else {
                    serde_json::from_slice(&req.body).unwrap_or(json!({}))
                };
                let name = body
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let url = body.get("url").and_then(|v| v.as_str()).unwrap_or_default();
                let Some(service) = self.mcp_oauth.lock().await.clone() else {
                    return HttpResponse::json(
                        503,
                        &json!({ "error": "MCP OAuth is not configured" }),
                    );
                };
                match action {
                    // Browser authorization-code + PKCE: the server discovers
                    // the authorization server, registers a public PKCE client
                    // and mints one flow. The browser opens the returned URL;
                    // the callback route below exchanges the code.
                    "begin-auth-code" => {
                        if name.is_empty() || url.is_empty() {
                            return HttpResponse::bad_request("name and url are required");
                        }
                        // The redirect must land on this server. A caller
                        // behind an HTTPS proxy passes its own spelling;
                        // otherwise the request's Host header is the address
                        // the browser used to reach us.
                        let redirect_uri = body
                            .get("redirect_uri")
                            .and_then(|v| v.as_str())
                            .filter(|value| !value.is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                format!(
                                    "http://{}{}",
                                    req.header("host").unwrap_or("127.0.0.1"),
                                    MCP_AUTH_CALLBACK_PATH
                                )
                            });
                        let scope = body.get("scope").and_then(|v| v.as_str());
                        match self
                            .mcp_auth_code
                            .begin(
                                &reqwest::Client::new(),
                                name,
                                url,
                                &redirect_uri,
                                scope,
                                "Kimi Code",
                            )
                            .await
                        {
                            Ok(start) => HttpResponse::ok(
                                &serde_json::to_value(start).unwrap_or(Value::Null),
                            ),
                            Err(e) => HttpResponse::bad_request(e),
                        }
                    }
                    // Poll the flow the browser is completing: 202 while it is
                    // still pending, 200 on success, 400 on failure.
                    "complete-auth-code" => {
                        let flow_id = body
                            .get("flow_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if flow_id.is_empty() {
                            return HttpResponse::bad_request("flow_id is required");
                        }
                        let status = self.mcp_auth_code.status(flow_id).await;
                        let payload = serde_json::to_value(&status).unwrap_or(Value::Null);
                        if !status.done {
                            HttpResponse::json(202, &payload)
                        } else if status.authenticated {
                            HttpResponse::ok(&payload)
                        } else {
                            HttpResponse::json(400, &payload)
                        }
                    }
                    "cancel-auth-code" => {
                        let flow_id = body
                            .get("flow_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if flow_id.is_empty() {
                            return HttpResponse::bad_request("flow_id is required");
                        }
                        let cancelled = self.mcp_auth_code.cancel(flow_id).await;
                        HttpResponse::ok(&json!({ "cancelled": cancelled }))
                    }
                    // RFC 8628 §3.1: start a device authorization.
                    "begin" => {
                        let endpoint = body
                            .get("device_authorization_endpoint")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        let client_id = body
                            .get("client_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if name.is_empty() || endpoint.is_empty() || client_id.is_empty() {
                            return HttpResponse::bad_request(
                                "name, device_authorization_endpoint and client_id are required",
                            );
                        }
                        match crate::mcp::oauth::begin_device_login(
                            &reqwest::Client::new(),
                            endpoint,
                            client_id,
                        )
                        .await
                        {
                            Ok(start) => HttpResponse::ok(&json!(start)),
                            Err(e) => HttpResponse::bad_request(e),
                        }
                    }
                    // RFC 8628 §3.4: poll until approved, then persist.
                    "complete" => {
                        let device_code = body
                            .get("device_code")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        let token_endpoint = body
                            .get("token_endpoint")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        let client_id = body
                            .get("client_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        if name.is_empty()
                            || url.is_empty()
                            || device_code.is_empty()
                            || token_endpoint.is_empty()
                            || client_id.is_empty()
                        {
                            return HttpResponse::bad_request(
                                "name, url, device_code, token_endpoint and client_id are required",
                            );
                        }
                        let start = crate::mcp::oauth::DeviceCodeStart {
                            device_code: device_code.to_string(),
                            user_code: body
                                .get("user_code")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            verification_uri: body
                                .get("verification_uri")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            verification_uri_complete: None,
                            expires_in: body
                                .get("expires_in")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(300),
                            interval: body.get("interval").and_then(|v| v.as_u64()).unwrap_or(5),
                        };
                        match crate::mcp::oauth::poll_device_login(
                            &reqwest::Client::new(),
                            token_endpoint,
                            client_id,
                            &start,
                        )
                        .await
                        {
                            Ok(tokens) => {
                                let key = match crate::mcp::oauth::mcp_oauth_store_key(name, url) {
                                    Ok(key) => key,
                                    Err(e) => return HttpResponse::bad_request(e),
                                };
                                match service.store_tokens(&key, &tokens) {
                                    Ok(()) => HttpResponse::ok(&json!({
                                        "authenticated": true,
                                        "key": key,
                                    })),
                                    Err(e) => HttpResponse::internal_error(e),
                                }
                            }
                            Err(e) => HttpResponse::bad_request(e),
                        }
                    }
                    // Drop the stored credentials.
                    "cancel" | "reset" => {
                        let key = match crate::mcp::oauth::mcp_oauth_store_key(name, url) {
                            Ok(key) => key,
                            Err(e) => return HttpResponse::bad_request(e),
                        };
                        match service.remove(&key) {
                            Ok(removed) => HttpResponse::ok(&json!({
                                "authenticated": false,
                                "removed": removed,
                            })),
                            Err(e) => HttpResponse::internal_error(e),
                        }
                    }
                    other => HttpResponse::bad_request(format!("Unknown MCP auth action: {other}")),
                }
            }
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/mcp") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 6 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                // The roster is engine-global: there is no per-workspace MCP
                // config, so every session sees the same servers. The session
                // is validated so a client cannot probe arbitrary ids, and the
                // payload names the scope rather than implying it is per-session.
                let servers = self.mcp_manager.server_entries().await;
                HttpResponse::ok(&json!({
                    "servers": servers,
                    "sessionId": session_id,
                    "scope": "engine-global",
                }))
            }
            // The session-scoped MCP views the web UI calls. Same engine-global
            // roster as above; the paths differ because the client asks per
            // session, and the session is validated the same way.
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.ends_with("/mcp/servers") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 7 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let servers = self.mcp_manager.server_entries().await;
                HttpResponse::ok(&json!({ "servers": servers, "sessionId": session_id }))
            }
            ("GET", p) if p.starts_with("/api/v1/sessions/") && p.contains("/mcp/servers/") => {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 8 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let name = segments[7];
                let servers = self.mcp_manager.server_entries().await;
                match servers.into_iter().find(|entry| entry.name == name) {
                    Some(entry) => {
                        HttpResponse::ok(&serde_json::to_value(&entry).unwrap_or(Value::Null))
                    }
                    None => HttpResponse::not_found(),
                }
            }
            ("POST", p)
                if p.starts_with("/api/v1/sessions/")
                    && p.contains("/mcp/servers/")
                    && (p.ends_with(":reconnect") || p.ends_with(":restart")) =>
            {
                let segments: Vec<&str> = p.split('/').collect();
                if segments.len() != 8 {
                    return HttpResponse::not_found();
                }
                let session_id = segments[4];
                if self.store.get_session(session_id).ok().flatten().is_none() {
                    return HttpResponse::not_found();
                }
                let name = segments[7]
                    .strip_suffix(":reconnect")
                    .or_else(|| segments[7].strip_suffix(":restart"))
                    .unwrap_or_default();
                match self.mcp_manager.reconnect(name).await {
                    Ok(()) => HttpResponse::ok(&json!({ "reconnected": true })),
                    Err(e) => HttpResponse::internal_error(e),
                }
            }
            _ => return HttpResponse::route_miss(),
        };
        finalize_response(req, resp)
    }
}

fn remote_control_device_auth(
    config: &crate::config::KimiConfig,
) -> Result<crate::server::remote_control::RemoteControlDeviceAuth, String> {
    let configured = crate::region::ConfiguredOAuthRef::from_config(config);
    let has_explicit_slot = configured.key.is_some();
    let oauth_key = match configured.key.as_ref() {
        Some(key) => key.clone(),
        None => {
            let region = crate::region::resolve_kimi_region(
                &configured,
                crate::workflow::kimi_home().as_deref(),
            );
            let profile = crate::region::kimi_region_profile(region);
            crate::region::resolve_kimi_code_oauth_key(
                Some(profile.oauth_host),
                Some(profile.base_url),
            )
        }
    };
    let storage_name = remote_control_token_storage_name(&oauth_key)?;
    Ok(crate::server::remote_control::RemoteControlDeviceAuth {
        relay_origin: remote_control_relay_origin(config),
        storage_name,
        allow_managed_aliases: !has_explicit_slot,
    })
}

/// `resolveKimiTokenStorageName`: strip the `oauth/` namespace, then reject a
/// path-like key instead of letting the credential store sanitize it into a
/// different account's file.
fn remote_control_token_storage_name(oauth_key: &str) -> Result<String, String> {
    let name = if oauth_key == "kimi-code" || oauth_key == crate::region::KIMI_CODE_OAUTH_KEY {
        "kimi-code"
    } else {
        oauth_key.strip_prefix("oauth/").unwrap_or(oauth_key)
    };
    if name.is_empty() || name.starts_with('.') || name.chars().any(|ch| matches!(ch, '/' | '\\')) {
        return Err(format!("Invalid Kimi OAuth token key: \"{oauth_key}\"."));
    }
    Ok(name.to_string())
}

/// The callback path the begin route advertises unless the caller supplies its
/// own redirect URI.
const MCP_AUTH_CALLBACK_PATH: &str = "/api/v1/mcp/auth/callback";

/// A small HTML page for the browser the authorization server redirected: the
/// flow's result has to be readable without any client.
fn html_result(status: u16, title: &str, message: &str) -> HttpResponse {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem\">\
         <h1>{title}</h1><p>{message}</p></body></html>",
        title = escape_html(title),
        message = escape_html(message),
    );
    HttpResponse::bytes(status, "text/html; charset=utf-8", body.into_bytes())
}

/// The provider's error text is third-party content rendered into the result
/// page, so it must not be able to inject markup.
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

impl HttpServer {
    /// The persisted stable device id (ts `createKimiDeviceId(homeDir)`).
    fn stored_remote_control_device_id(&self) -> Option<String> {
        self.store
            .get_state("remote-control", "device_id")
            .ok()
            .flatten()
            .and_then(|value| value.as_str().map(str::to_string))
    }
}

/// 50001 for a failed cloud device mutation, matching the upstream
/// `handleDeviceRoute` catch-all code.
fn remote_control_internal_error(req: &HttpRequest, message: String) -> HttpResponse {
    HttpResponse::json(
        500,
        &json!({
            "code": envelope::error_codes::INTERNAL_ERROR,
            "msg": message,
            "data": Value::Null,
            "request_id": req.request_id(),
        }),
    )
}

/// 40928 conflict: another holder owns the remote-control start slot
/// (upstream `REMOTE_CONTROL_ALREADY_RUNNING` with `details`).
fn remote_control_already_running(
    req: &HttpRequest,
    started_at: u64,
    local_origin: &str,
    device_id: String,
    url: String,
) -> HttpResponse {
    HttpResponse::json(
        409,
        &json!({
            "code": envelope::error_codes::REMOTE_CONTROL_ALREADY_RUNNING,
            "msg": "Remote Control is already running; stop the current session before starting another.",
            "data": Value::Null,
            "request_id": req.request_id(),
            "details": {
                "pid": std::process::id(),
                "local_origin": local_origin,
                "device_id": device_id,
                "url": url,
                "started_at": started_at,
            },
        }),
    )
}
