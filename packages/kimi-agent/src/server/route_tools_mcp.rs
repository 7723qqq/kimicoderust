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
                        HttpResponse::ok(&json!({
                            "enabled": false,
                            "state": st.state,
                            "url": Value::Null,
                            "device_id": st.device_id,
                            "device_name": st.device_name,
                            "available": false,
                            "reason": REMOTE_CONTROL_UNAVAILABLE,
                            "error": st.error,
                        }))
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
                // Start (or report the already-running) runtime.
                let mut slot = self.remote_control_runtime.lock().await;
                if let Some(handle) = slot.as_ref() {
                    let st = handle.status();
                    self.emit_session_telemetry(
                        "remote_control_toggle",
                        serde_json::json!({ "enabled": true, "outcome": "already_running" }),
                    );
                    return HttpResponse::ok(&json!({
                        "enabled": st.enabled,
                        "state": st.state,
                        "url": st.url,
                        "device_id": st.device_id,
                        "device_name": st.device_name,
                        "available": true,
                        "already_running": true,
                        "error": st.error,
                    }));
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
                let device_id = match self.store.get_state("remote-control", "device_id") {
                    Ok(Some(value)) => value
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(new_device_id),
                    _ => {
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
