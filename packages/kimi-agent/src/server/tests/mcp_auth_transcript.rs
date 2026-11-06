use super::super::*;
use std::collections::HashMap;

/// `mcp/auth:begin` + `:complete` drive the RFC 8628 flow and persist the
/// credentials; `auth-statuses` reports them and `:reset` clears them.
#[tokio::test]
async fn test_http_mcp_auth_device_flow() {
    let (url, _hits, _shutdown) =
            crate::mcp::oauth::device::test_helpers::spawn_mock_oauth_server(vec![
                (
                    200,
                    r#"{"device_code":"dev-1","user_code":"ABCD","verification_uri":"https://example.test/device","expires_in":60,"interval":0}"#,
                ),
                (400, r#"{"error":"authorization_pending"}"#),
                (
                    200,
                    r#"{"access_token":"mcp-token","refresh_token":"r1","expires_in":3600}"#,
                ),
            ])
            .await;

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let service = Arc::new(crate::mcp::oauth::McpOAuthService::new(Arc::new(
        crate::mcp::oauth::McpOAuthFileStore::new(dir.path()),
    )));
    let server = HttpServer::new(store).with_mcp_oauth_service(service).await;

    let begin = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/mcp/auth:begin".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "name": "srv",
                "device_authorization_endpoint": url,
                "client_id": "client-1"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(begin.status, 200);
    let begin_body: Value = serde_json::from_slice(&begin.body).unwrap();
    assert_eq!(begin_body["user_code"], "ABCD");
    assert_eq!(
        begin_body["verification_uri"],
        "https://example.test/device"
    );

    let complete = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/mcp/auth:complete".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "name": "srv",
                "url": "https://example.test/mcp",
                "device_code": "dev-1",
                "token_endpoint": url,
                "client_id": "client-1",
                "interval": 0,
                "expires_in": 60
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(complete.status, 200);
    let complete_body: Value = serde_json::from_slice(&complete.body).unwrap();
    assert_eq!(complete_body["authenticated"], true);

    let statuses = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp/auth-statuses".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let statuses_body: Value = serde_json::from_slice(&statuses.body).unwrap();
    let key = crate::mcp::oauth::mcp_oauth_store_key("srv", "https://example.test/mcp").unwrap();
    assert_eq!(statuses_body["statuses"][&key]["authenticated"], true);
    assert_eq!(statuses_body["statuses"][&key]["status"], "authenticated");
    // A usable token is required: an empty credentials dir reports no key
    // at all, and a stored-but-unusable key must say `needs-auth` rather
    // than a fabricated `true` (covered by the OAuth service's own tests).

    let reset = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/mcp/auth:reset".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "name": "srv",
                "url": "https://example.test/mcp"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(reset.status, 200);
    let reset_body: Value = serde_json::from_slice(&reset.body).unwrap();
    assert_eq!(reset_body["removed"], true);
}

/// `mcp/auth:begin-auth-code` discovers the authorization server, registers a
/// PKCE client and returns the browser URL; the unauthenticated callback route
/// exchanges the code and stores the grant; polling reports the outcome. The
/// callback is the only `/api` GET that bypasses bearer auth — the single-use
/// state is its credential.
#[tokio::test]
async fn test_http_mcp_auth_authorization_code_flow() {
    let (as_base, as_requests) = crate::mcp::oauth::auth_code::test_helpers::spawn_mock_as().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let dir = tempfile::tempdir().unwrap();
    let service = Arc::new(crate::mcp::oauth::McpOAuthService::new(Arc::new(
        crate::mcp::oauth::McpOAuthFileStore::new(dir.path()),
    )));
    let server = HttpServer::new(store)
        .with_auth(crate::server::auth::ServerAuth::Token(Arc::from("tok3n")))
        .with_mcp_oauth_service(service)
        .await;

    let post = |path: &str, body: Value| HttpRequest {
        method: "POST".into(),
        path: path.into(),
        query: None,
        headers: std::collections::HashMap::new(),
        body: serde_json::to_vec(&body).unwrap(),
    };

    // The begin route stays behind the bearer gate.
    let unauthorized = server
        .handle_request(&post(
            "/api/v1/mcp/auth:begin-auth-code",
            json!({ "name": "srv", "url": format!("{as_base}/mcp") }),
        ))
        .await;
    assert_eq!(unauthorized.status, 401);

    let begin = server
        .handle_request(&HttpRequest {
            headers: std::collections::HashMap::from([(
                "authorization".to_string(),
                "Bearer tok3n".to_string(),
            )]),
            ..post(
                "/api/v1/mcp/auth:begin-auth-code",
                json!({
                    "name": "srv",
                    "url": format!("{as_base}/mcp"),
                    "redirect_uri": "http://127.0.0.1:9/cb"
                }),
            )
        })
        .await;
    assert_eq!(
        begin.status,
        200,
        "{}",
        String::from_utf8_lossy(&begin.body)
    );
    let begin_body: Value = serde_json::from_slice(&begin.body).unwrap();
    let state = begin_body["state"].as_str().unwrap().to_string();
    let flow_id = begin_body["flow_id"].as_str().unwrap().to_string();
    let authorization_url = begin_body["authorization_url"].as_str().unwrap();
    assert!(authorization_url.contains("code_challenge_method=S256"));
    assert!(authorization_url.contains("resource="));

    // Polling before the browser returns: still pending.
    let pending = server
        .handle_request(&HttpRequest {
            headers: std::collections::HashMap::from([(
                "authorization".to_string(),
                "Bearer tok3n".to_string(),
            )]),
            ..post(
                "/api/v1/mcp/auth:complete-auth-code",
                json!({ "flow_id": flow_id }),
            )
        })
        .await;
    assert_eq!(pending.status, 202);

    // The callback carries no bearer token; it must still be served.
    let callback = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp/auth/callback".into(),
            query: Some(format!("code=the-code&state={state}")),
            headers: std::collections::HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(callback.status, 200);
    assert_eq!(
        callback.header("Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert!(
        as_requests
            .lock()
            .await
            .iter()
            .any(|request| request.starts_with("POST /token "))
    );

    let complete = server
        .handle_request(&HttpRequest {
            headers: std::collections::HashMap::from([(
                "authorization".to_string(),
                "Bearer tok3n".to_string(),
            )]),
            ..post(
                "/api/v1/mcp/auth:complete-auth-code",
                json!({ "flow_id": flow_id }),
            )
        })
        .await;
    assert_eq!(complete.status, 200);
    let complete_body: Value = serde_json::from_slice(&complete.body).unwrap();
    assert_eq!(complete_body["authenticated"], true);
    let key = complete_body["key"].as_str().unwrap().to_string();
    assert_eq!(
        key,
        crate::mcp::oauth::mcp_oauth_store_key("srv", &format!("{as_base}/mcp")).unwrap()
    );
    let service = server.mcp_oauth.lock().await.clone().unwrap();
    assert!(
        service.access_token(&key).await.is_some(),
        "the grant the engine would use is stored and usable"
    );

    // A replayed callback state is refused (single use).
    let replay = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp/auth/callback".into(),
            query: Some(format!("code=the-code&state={state}")),
            headers: std::collections::HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(replay.status, 400);

    // Unknown flows are finished-with-error, never "still pending".
    let unknown = server
        .handle_request(&HttpRequest {
            headers: std::collections::HashMap::from([(
                "authorization".to_string(),
                "Bearer tok3n".to_string(),
            )]),
            ..post(
                "/api/v1/mcp/auth:complete-auth-code",
                json!({ "flow_id": "no-such-flow" }),
            )
        })
        .await;
    assert_eq!(unknown.status, 400);
}

#[tokio::test]
async fn test_http_session_events_and_projection() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let server = HttpServer::with_hub(store.clone(), hub.clone());

    let sid = "sess-event-http";
    store
        .create_session(sid, Some("Event Stream Test"))
        .unwrap();

    // 1. Publish events onto hub lane -> automatically captured by persister into wire_events
    let bus = hub.bus_for(sid);
    bus.publish(&crate::events::EngineEvent::Custom(json!({
        "type": "message.user",
        "content": "What is the capital of France?"
    })));
    bus.publish(&crate::events::EngineEvent::Custom(json!({
        "type": "message.assistant",
        "content": "The capital of France is Paris."
    })));

    // 2. GET /api/v1/sessions/:id/events (both /events and :events)
    let res_events_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/events"),
            query: Some("since=0&limit=10".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_events_slash.status, 200);
    let val_slash: Value = serde_json::from_slice(&res_events_slash.body).unwrap();
    assert_eq!(val_slash["sessionId"], sid);
    assert_eq!(val_slash["events"].as_array().unwrap().len(), 2);
    assert_eq!(val_slash["total"], 2);

    let res_events_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}:events"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_events_colon.status, 200);

    // 3. GET /api/v1/sessions/:id/projection
    let res_proj = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/projection"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_proj.status, 200);
    let val_proj: Value = serde_json::from_slice(&res_proj.body).unwrap();
    let msgs = val_proj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "What is the capital of France?");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "The capital of France is Paris.");

    // 4. POST /api/v1/sessions/:id/events:compact
    let res_compact = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:events:compact"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "summary": "Paris geography summary" })).unwrap(),
        })
        .await;
    assert_eq!(res_compact.status, 200);

    // Projection after compaction must reflect summary
    let res_proj_after = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}:projection"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_proj_after.status, 200);
    let val_proj_after: Value = serde_json::from_slice(&res_proj_after.body).unwrap();
    let msgs_after = val_proj_after["messages"].as_array().unwrap();
    assert_eq!(msgs_after.len(), 1);
    assert_eq!(msgs_after[0]["role"], "system");
    assert_eq!(msgs_after[0]["content"], "Paris geography summary");
}

#[tokio::test]
async fn test_http_transcript_routes() {
    let server = HttpServer::in_memory().unwrap();
    let sid = "sess-transcript";
    server
        .store
        .create_session(sid, Some("Transcript"))
        .unwrap();

    let mut assistant = crate::turn_loop::types::LLMMessage::new("assistant", "hello");
    assistant.tool_calls = vec![crate::turn_loop::types::ToolCall {
        id: "c1".into(),
        name: "ExitPlanMode".into(),
        arguments: json!({}),
        extras: None,
    }];
    let mut tool = crate::turn_loop::types::LLMMessage::new(
        "tool",
        "Plan saved to: /tmp/plan.md\n## Approved Plan:\ndo the thing",
    );
    tool.tool_call_id = Some("c1".into());
    let messages = vec![
        crate::turn_loop::types::LLMMessage::new("user", "start the work"),
        assistant,
        tool,
    ];
    server
        .store
        .save_turn(sid, "turn-1", 1, &messages, None, None)
        .unwrap();

    let transcript = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/transcript"),
            query: Some("agent_id=main".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(transcript.status, 200);
    let val: Value = serde_json::from_slice(&transcript.body).unwrap();
    assert_eq!(val["agent_id"], "main");
    assert_eq!(val["items"].as_array().unwrap().len(), 1);
    assert_eq!(val["items"][0]["prompt"], "start the work");
    assert_eq!(val["items"][0]["steps"][0]["frames"][1]["state"], "done");

    let user_messages = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/transcript/user-messages"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(user_messages.status, 200);
    let um: Value = serde_json::from_slice(&user_messages.body).unwrap();
    assert_eq!(um["agents"][0]["messages"][0]["prompt"], "start the work");

    let plan = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/transcript/plan"),
            query: Some("agent_id=main&tool_call_id=c1".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(plan.status, 200);
    let plan_val: Value = serde_json::from_slice(&plan.body).unwrap();
    assert_eq!(plan_val["plans"].as_array().unwrap().len(), 1);
    assert_eq!(plan_val["plans"][0]["path"], "/tmp/plan.md");

    let ops = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/transcript/ops"),
            query: Some("agent_id=main&since_seq=0".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(ops.status, 200);
    let ops_val: Value = serde_json::from_slice(&ops.body).unwrap();
    assert_eq!(ops_val["complete"], false);

    // With a persisted journal the catch-up serves an idempotent reset
    // batch instead of falling back to a full refresh.
    server
        .hub()
        .bus_for(sid)
        .publish(&crate::events::EngineEvent::LlmStepBegin {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "turn-1".into(),
            step: 1,
        });
    let ops_after = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/transcript/ops"),
            query: Some("agent_id=main&since_seq=0".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let ops_after_val: Value = serde_json::from_slice(&ops_after.body).unwrap();
    assert_eq!(ops_after_val["complete"], true);
    assert_eq!(ops_after_val["batches"][0]["ops"][0]["op"], "reset");
    assert!(
        ops_after_val["batches"][0]["seq"].as_u64().unwrap() > 0,
        "batch carries the journal seq"
    );
}
