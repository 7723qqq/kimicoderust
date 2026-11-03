use super::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_health_endpoint() {
    let server = HttpServer::in_memory().unwrap();
    let req = HttpRequest {
        method: "GET".into(),
        path: "/api/v1/health".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res = server.handle_request(&req).await;
    assert_eq!(res.status, 200);
    let val: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(val["status"], "ok");
    assert_eq!(val["engine"], "kimi-agent-rust");
}

/// v2 #3832: a metadata object that names its own variant is a prompt
/// origin (the transcript contract's shape), not opaque client metadata —
/// the v3 projection reads the `skill_activation` variant off the turn
/// origin for the opening user message's `skill_activations`.
#[test]
fn prompt_origin_from_metadata_distinguishes_origin_variants() {
    let opaque = prompt_origin_from_metadata(Some(&json!([{ "surface": "web" }]))).unwrap();
    assert_eq!(opaque["kind"], "user");
    assert_eq!(opaque["clientMetadata"][0]["surface"], "web");

    let activation = prompt_origin_from_metadata(Some(&json!([{
        "origin": {
            "kind": "skill_activation",
            "trigger": "user-slash",
            "skillName": "review",
            "skillArgs": "src/main.rs",
        },
    }])))
    .unwrap();
    assert_eq!(activation["kind"], "skill_activation");
    assert_eq!(activation["skillName"], "review");
    assert_eq!(activation["skillArgs"], "src/main.rs");
    assert!(
        activation.get("clientMetadata").is_none(),
        "an origin variant is not re-wrapped as client metadata"
    );

    assert!(prompt_origin_from_metadata(None).is_none());
}

#[tokio::test]
async fn test_turn_origin_persists_and_projects() {
    // #3764: the origin the route builds from the request's `metadata`
    // lands in the turns table and comes back through list_turns, so the
    // v3 projection can carry it instead of assuming `user`.
    let store = SqliteSessionStore::in_memory().unwrap();
    store.create_session("sess-origin", Some("origin")).unwrap();
    let origin = json!({
        "kind": "user",
        "clientMetadata": [{ "surface": "web", "threadId": "abc" }],
    });
    store
        .save_turn(
            "sess-origin",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user("hello")],
            None,
            Some(&origin),
        )
        .unwrap();
    let turns = store.list_turns("sess-origin").unwrap();
    assert_eq!(turns.len(), 1);
    let saved = turns[0].origin.as_ref().unwrap();
    assert_eq!(saved["kind"], "user");
    assert_eq!(saved["clientMetadata"][0]["threadId"], "abc");

    // A turn saved without an origin reads back as None (pre-#3764 rows).
    store
        .save_turn(
            "sess-origin",
            "turn-2",
            2,
            &[crate::turn_loop::types::LLMMessage::user("second")],
            None,
            None,
        )
        .unwrap();
    let turns = store.list_turns("sess-origin").unwrap();
    assert_eq!(turns.len(), 2);
    assert!(turns[1].origin.is_none());
}

#[tokio::test]
async fn test_http_sessions_crud_and_prompt() {
    let server = HttpServer::in_memory().unwrap();

    // 1. Create session
    let req_create = HttpRequest {
        method: "POST".into(),
        path: "/api/v1/sessions".into(),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({ "title": "Web REST Test" })).unwrap(),
    };
    let res_create = server.handle_request(&req_create).await;
    // v2 answers the bare `sessionSchema` (no `statusCode` override).
    assert_eq!(res_create.status, 200);
    let val_create: Value = serde_json::from_slice(&res_create.body).unwrap();
    let sid = val_create["id"].as_str().unwrap();

    // 2. List sessions
    let req_list = HttpRequest {
        method: "GET".into(),
        path: "/api/v1/sessions".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_list = server.handle_request(&req_list).await;
    assert_eq!(res_list.status, 200);
    let val_list: Value = serde_json::from_slice(&res_list.body).unwrap();
    assert_eq!(val_list["sessions"].as_array().unwrap().len(), 1);

    // 3. Prompt session
    let req_prompt = HttpRequest {
        method: "POST".into(),
        path: format!("/api/v1/sessions/{sid}/prompt"),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({ "prompt": "Hello REST" })).unwrap(),
    };
    // No engine attached: refuse. This route used to answer
    // `Processed: {prompt}` with status 200, which a client could not tell
    // apart from a real turn.
    let res_prompt = server.handle_request(&req_prompt).await;
    assert_eq!(res_prompt.status, 503);
    let val_prompt: Value = serde_json::from_slice(&res_prompt.body).unwrap();
    assert!(
        val_prompt["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no engine"),
        "{val_prompt}"
    );
    assert!(
        !String::from_utf8_lossy(&res_prompt.body).contains("Processed:"),
        "the canned reply is still being served"
    );

    // 4. Get specific session
    let req_get = HttpRequest {
        method: "GET".into(),
        path: format!("/api/v1/sessions/{sid}"),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_get = server.handle_request(&req_get).await;
    assert_eq!(res_get.status, 200);
    let val_get: Value = serde_json::from_slice(&res_get.body).unwrap();
    // v2 answers the bare `sessionSchema` (`toWireSession`), not a wrapper.
    assert_eq!(val_get["session_id"], sid);
    assert_eq!(val_get["title"], "Web REST Test");

    // 4b. Create child session
    let req_child = HttpRequest {
        method: "POST".into(),
        path: format!("/api/v1/sessions/{sid}/children"),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({ "title": "Child Session" })).unwrap(),
    };
    let res_child = server.handle_request(&req_child).await;
    assert_eq!(res_child.status, 200);
    let val_child: Value = serde_json::from_slice(&res_child.body).unwrap();
    assert_eq!(val_child["parent_session_id"], sid);
    assert_eq!(val_child["title"], "Child Session");

    // Verify child listed
    let req_children = HttpRequest {
        method: "GET".into(),
        path: format!("/api/v1/sessions/{sid}/children"),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_children = server.handle_request(&req_children).await;
    assert_eq!(res_children.status, 200);
    let val_children: Value = serde_json::from_slice(&res_children.body).unwrap();
    // v2 answers the paged `{items, has_more}` contract; the web client
    // reads `.items`.
    assert_eq!(val_children["items"].as_array().unwrap().len(), 1);
    assert_eq!(val_children["has_more"], false);

    // 5. Delete session
    let req_del = HttpRequest {
        method: "DELETE".into(),
        path: format!("/api/v1/sessions/{sid}"),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_del = server.handle_request(&req_del).await;
    assert_eq!(res_del.status, 200);
    let val_del: Value = serde_json::from_slice(&res_del.body).unwrap();
    assert_eq!(val_del["deleted"], true);

    // 6. Verify deleted
    let res_get_after = server.handle_request(&req_get).await;
    assert_eq!(res_get_after.status, 404);
}

/// The REST surface for remote control (#3594) is backed by a real runtime
/// (`server/remote_control.rs`): enabling without the Kimi login credential
/// is refused honestly (the relay WS upgrade would fail anyway), and the
/// off state is reported as such — with a reason that says the runtime is
/// not started, not that it does not exist. The `501` era is over — a start
/// request with a credential would actually arm the runtime.
#[tokio::test]
async fn test_remote_control_reports_the_not_started_runtime() {
    let server = HttpServer::in_memory().unwrap();

    let req_get = HttpRequest {
        method: "GET".into(),
        path: "/api/v1/remote-control".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_get = server.handle_request(&req_get).await;
    assert_eq!(res_get.status, 200);
    let val_get: Value = serde_json::from_slice(&res_get.body).unwrap();
    assert_eq!(val_get["enabled"], false);
    assert_eq!(val_get["state"], "off");
    assert_eq!(val_get["available"], false);
    assert!(val_get["url"].is_null(), "no device URL may be advertised");
    assert!(
        val_get["reason"]
            .as_str()
            .unwrap()
            .contains("remote-control is not running"),
        "the response must explain that the runtime simply is not started: {val_get}"
    );

    // Enabling requires the Kimi login credential: without it the relay
    // WS upgrade would fail mid-handshake, so the honest answer is a 400
    // naming the missing field — not a fabricated `state: "on"`.
    let req_enable = HttpRequest {
        method: "POST".into(),
        path: "/api/v1/remote-control".into(),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({ "enabled": true })).unwrap(),
    };
    let res_enable = server.handle_request(&req_enable).await;
    assert_eq!(res_enable.status, 400);
    let val_enable: Value = serde_json::from_slice(&res_enable.body).unwrap();
    assert!(
        val_enable["error"]
            .as_str()
            .unwrap_or_default()
            .contains("refresh_token"),
        "{val_enable}"
    );

    // A missing `enabled` field is still a client error.
    let req_missing = HttpRequest {
        method: "POST".into(),
        path: "/api/v1/remote-control".into(),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({})).unwrap(),
    };
    assert_eq!(server.handle_request(&req_missing).await.status, 400);

    // GET is unchanged by the refused enable.
    let res_get2 = server.handle_request(&req_get).await;
    let val_get2: Value = serde_json::from_slice(&res_get2.body).unwrap();
    assert_eq!(val_get2["state"], "off");
    assert_eq!(val_get2["enabled"], false);

    // Disabling when nothing runs is an honest idempotent off.
    let req_disable = HttpRequest {
        method: "POST".into(),
        path: "/api/v1/remote-control".into(),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({ "enabled": false })).unwrap(),
    };
    let res_disable = server.handle_request(&req_disable).await;
    assert_eq!(res_disable.status, 200);
    let val_disable: Value = serde_json::from_slice(&res_disable.body).unwrap();
    assert_eq!(val_disable["enabled"], false);
    assert_eq!(val_disable["state"], "off");
}

/// v2 #3969 on the server side: the relay the toggle arms follows the
/// login region, resolved from the persisted OAuth ref — with no new wire
/// field. An operator's relay env override still wins over both.
#[test]
fn remote_control_relay_follows_the_persisted_login_region() {
    let override_relay = || {
        std::env::var("KIMI_CODE_REMOTE_CONTROL_RELAY_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string())
    };
    let expected_for =
        |region_relay: &str| override_relay().unwrap_or_else(|| region_relay.to_string());

    let global: crate::config::KimiConfig = concat!(
        "[providers.\"managed:kimi-code\"]\n",
        "type = \"kimi\"\n\n",
        "[providers.\"managed:kimi-code\".oauth]\n",
        "key = \"oauth/kimi-code-env-0e4f99c69cc27850\"\n",
        "oauthHost = \"https://auth.kimi.ai\"\n",
    )
    .parse()
    .unwrap();
    assert_eq!(
        remote_control_relay_origin(&global),
        expected_for("https://code-rc.kimi.ai"),
        "a global login must reach the .ai relay"
    );

    let mainland: crate::config::KimiConfig =
        "[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\n\n\
             [providers.\"managed:kimi-code\".oauth]\nkey = \"oauth/kimi-code\"\n"
            .parse()
            .unwrap();
    assert_eq!(
        remote_control_relay_origin(&mainland),
        expected_for(crate::server::remote_control::REMOTE_CONTROL_RELAY_ORIGIN),
        "a mainland login keeps the .com relay"
    );

    // A provider without an oauth ref (never logged in) is mainland, too.
    let no_login: crate::config::KimiConfig =
        "[providers.openai]\ntype = \"openai\"\n".parse().unwrap();
    assert_eq!(
        remote_control_relay_origin(&no_login),
        expected_for(crate::server::remote_control::REMOTE_CONTROL_RELAY_ORIGIN)
    );
}

#[tokio::test]
async fn test_session_delete_action_endpoint() {
    let server = HttpServer::in_memory().unwrap();
    let sid = "sess_del_action_test";
    server
        .store
        .create_session(sid, Some("Delete Action Test"))
        .unwrap();

    let req_del = HttpRequest {
        method: "POST".into(),
        path: format!("/api/v1/sessions/{sid}:delete"),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_del = server.handle_request(&req_del).await;
    assert_eq!(res_del.status, 200);
    let val_del: Value = serde_json::from_slice(&res_del.body).unwrap();
    assert_eq!(val_del["deleted"], true);

    let req_get = HttpRequest {
        method: "GET".into(),
        path: format!("/api/v1/sessions/{sid}"),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res_get = server.handle_request(&req_get).await;
    assert_eq!(res_get.status, 404);
}

fn engine_without_a_model(store: Arc<SqliteSessionStore>, hub: Arc<EventHub>) -> ServerEngine {
    ServerEngine::new(
        crate::pipeline::PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "test-model".into(),
            providers: Vec::new(),
            native_llm: None,
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: false,
            shell_path: None,
            policy_snapshot: None,
            github_token: None,
            github_base_url: None,
            subagent_timeout_ms: None,
            agent_tool_veto: None,
            tools_veto: None,
            todo_tool_veto: None,
            tower_worktree_root: None,
            tower_enabled: false,
            tool_select: false,
            sandbox_mode: None,
            sandbox_policy: None,
            caller_agent_id: None,
            session_id: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            background: crate::storage::BackgroundLimits::default(),
        },
        hub,
        store,
    )
}

/// The REST goal read is served from the engine's goal store — the store the
/// turn settle writes — so it reports the budget and counters the engine
/// folded, not a stale sqlite row or a host-side copy.
#[tokio::test]
async fn session_goal_route_serves_the_engine_store_snapshot() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let dir = tempfile::tempdir().unwrap();
    let workspace = store
        .create_workspace(dir.path().to_str().unwrap(), None)
        .unwrap();
    store
        .create_session_with_workspace("sess-engine-goal", None, Some(&workspace.id))
        .unwrap();
    let server = HttpServer::with_hub(store.clone(), hub.clone())
        .with_engine(engine_without_a_model(store.clone(), hub));

    let session_store =
        crate::storage::StateStore::for_session(dir.path(), "sess-engine-goal").unwrap();
    for write in [
        json!({ "action": "create", "objective": "engine goal" }),
        json!({ "action": "set_budget", "value": 4, "unit": "turns" }),
        json!({ "action": "record_usage", "turns": 1, "output_tokens": 7 }),
    ] {
        let outcome = session_store.apply_write("goal", &write).unwrap();
        session_store.write_domain("goal", &outcome.stored).unwrap();
    }
    // A stale sqlite row must not shadow the engine store.
    store
        .put_state("goal", "sess-engine-goal", &json!({ "objective": "stale" }))
        .unwrap();

    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-engine-goal/goal".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["objective"], "engine goal");
    assert_eq!(body["status"], "active");
    assert_eq!(body["turnsUsed"], 1);
    assert_eq!(body["tokensUsed"], 7);
    assert_eq!(body["budget"]["turnBudget"], 4);
    assert_eq!(body["budget"]["remainingTurns"], 3);
}

async fn prompt(server: &HttpServer, session_id: &str) -> HttpResponse {
    server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{session_id}/prompt"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "prompt": "Hello REST" })).unwrap(),
        })
        .await
}

#[tokio::test]
async fn the_prompt_route_reaches_the_engine_and_reports_its_failure() {
    let server = HttpServer::in_memory().unwrap();
    let sid = {
        let created = server
            .handle_request(&HttpRequest {
                method: "POST".into(),
                path: "/api/v1/sessions".into(),
                query: None,
                headers: HashMap::new(),
                body: serde_json::to_vec(&json!({ "title": "wired" })).unwrap(),
            })
            .await;
        let body: Value = serde_json::from_slice(&created.body).unwrap();
        body["id"].as_str().unwrap().to_string()
    };

    let hub = server.hub();
    let store = server.store_arc();
    let server = server.with_engine(engine_without_a_model(store, hub));

    // No providers and no native_llm: the pipeline refuses to build, which
    // must surface as a server error naming the cause —not a fake 200.
    let response = prompt(&server, &sid).await;
    let body = String::from_utf8_lossy(&response.body).into_owned();
    assert_eq!(response.status, 500, "{body}");
    assert!(body.contains("rustSelfContained"), "{body}");
}

/// A session whose stored history cannot be read is the failure a user
/// experiences as "my conversation is gone". It used to leave no event
/// behind — only a 500 in the access log — and renaming the telemetry event
/// left every test in this file green, so nothing observed that it fires.
#[tokio::test]
async fn an_unreadable_history_reports_session_load_failed() {
    let dir = std::env::temp_dir().join(format!("kimi-sload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sessions.db");

    // File-backed, not in-memory: the failure has to be injected from a
    // second connection, which an in-memory database cannot have.
    let store = Arc::new(SqliteSessionStore::open(&db).unwrap());
    let hub = Arc::new(EventHub::new());

    let captured: Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);
    let server = HttpServer::with_hub(store.clone(), hub.clone()).with_telemetry_sink(Arc::new(
        move |event: &str, payload: Value| {
            sink.lock().unwrap().push((event.to_string(), payload));
        },
    ));

    let created = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "unreadable" })).unwrap(),
        })
        .await;
    let body: Value = serde_json::from_slice(&created.body).unwrap();
    let sid = body["id"].as_str().unwrap().to_string();

    let server = server.with_engine(engine_without_a_model(store, hub));

    // Break the read the route depends on, leaving everything else the route
    // checks first — the session row, the workspace — intact.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("DROP TABLE messages", []).unwrap();
    }

    let response = prompt(&server, &sid).await;
    assert_eq!(response.status, 500, "a broken read is a server error");

    let events = captured.lock().unwrap().clone();
    let failed = events
        .iter()
        .find(|(event, _)| event == "session_load_failed");
    assert!(
        failed.is_some(),
        "the failure must be reported, not only logged: {events:?}"
    );
    let payload = &failed.unwrap().1;
    // v2's `SessionLoadFailedEvent` declares exactly one property.
    // `session_id` and `stage` were this engine's invention.
    assert!(payload.get("session_id").is_none());
    assert!(payload.get("stage").is_none());
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("messages"),
        "the reason must name the failure: {payload}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unknown_session_gets_404_rather_than_a_turn() {
    let server = HttpServer::in_memory().unwrap();
    let hub = server.hub();
    let store = server.store_arc();
    let server = server.with_engine(engine_without_a_model(store, hub));

    let response = prompt(&server, "sess-does-not-exist").await;
    assert_eq!(response.status, 404);
}

#[test]
fn server_with_engine_auto_attaches_mcp_manager() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let engine = engine_without_a_model(store.clone(), hub.clone());
    assert!(engine.mcp_manager().is_none());

    let server = HttpServer::with_hub(store, hub).with_engine(engine);
    let engine = server.engine().unwrap();
    assert!(engine.mcp_manager().is_some());
}

#[tokio::test]
async fn turn_numbers_advance_from_the_stored_turns() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    store.create_session("s1", None).unwrap();
    assert_eq!(store.next_turn_number("s1").unwrap(), 1);
    store
        .save_turn(
            "s1",
            "t1",
            1,
            &[crate::turn_loop::types::LLMMessage::user("hi")],
            None,
            None,
        )
        .unwrap();
    assert_eq!(store.next_turn_number("s1").unwrap(), 2);
    assert_eq!(store.next_turn_number("missing").unwrap(), 1);
}

#[tokio::test]
async fn test_http_cron_endpoints() {
    let server = HttpServer::in_memory().unwrap();
    let created = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "cron-test" })).unwrap(),
        })
        .await;
    let sid = serde_json::from_slice::<Value>(&created.body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 1. Initial list is empty
    let res_list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/cron"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list.status, 200);
    let val_list: Value = serde_json::from_slice(&res_list.body).unwrap();
    assert_eq!(val_list["entries"].as_array().unwrap().len(), 0);

    // 2. Add invalid cron expression -> 400 Bad Request
    let res_bad = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/cron"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "cron": "invalid cron",
                "prompt": "do something"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_bad.status, 400);

    // 3. Add valid cron entry
    let res_add = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/cron"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "id": "c1",
                "cron": "0 9 * * *",
                "prompt": "daily report",
                "recurring": true
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_add.status, 201);
    let val_add: Value = serde_json::from_slice(&res_add.body).unwrap();
    assert_eq!(val_add["id"], "c1");

    // 4. List again contains entry (test global route)
    let res_list2 = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/cron".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list2.status, 200);
    let val_list2: Value = serde_json::from_slice(&res_list2.body).unwrap();
    assert_eq!(val_list2["entries"].as_array().unwrap().len(), 1);

    // 5. Delete cron entry
    let res_del = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/sessions/{sid}/cron/c1"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del.status, 200);

    // 6. Delete non-existent cron entry -> 404
    let res_del_missing = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/sessions/{sid}/cron/c1"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del_missing.status, 404);
}

#[tokio::test]
async fn test_http_tasks_endpoints() {
    let server = HttpServer::in_memory().unwrap();
    let runner = server.task_runner();

    // Spawn a background task
    runner
        .spawn_task("task-1".into(), "test task".into(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            "task output".into()
        })
        .unwrap();

    // 1. List tasks
    let res_list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/tasks".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list.status, 200);
    let val_list: Value = serde_json::from_slice(&res_list.body).unwrap();
    // v2 `pageResponseSchema(taskSchema)`: `{items}` with protocol-named
    // elements (`id`), not the legacy camelCase `taskId`.
    let tasks = val_list["items"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-1");
    assert_eq!(tasks[0]["taskId"], "task-1", "the legacy key survives too");

    // 2. Get single task
    let res_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/tasks/task-1".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_get.status, 200);
    let val_get: Value = serde_json::from_slice(&res_get.body).unwrap();
    assert_eq!(val_get["id"], "task-1");

    // 3. Stop task
    let res_stop = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/tasks/task-1/stop".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_stop.status, 200);
    let val_stop: Value = serde_json::from_slice(&res_stop.body).unwrap();
    assert_eq!(val_stop["stopped"], true);

    // 4. Missing task -> 404
    let res_missing = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/tasks/non-existent".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_missing.status, 404);

    // 5. Create a session for session-scoped task routes
    server.store.create_session("sess-task", None).unwrap();
    runner
        .spawn_task("task-cancel".into(), "cancel task".into(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            "ok".into()
        })
        .unwrap();

    // 6. Cancel task: POST /api/v1/sessions/sess-task/tasks/task-cancel:cancel
    let res_cancel = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-task/tasks/task-cancel:cancel".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cancel.status, 200);
    let val_cancel: Value = serde_json::from_slice(&res_cancel.body).unwrap();
    assert_eq!(val_cancel["cancelled"], true);

    // 7. Detach task: POST /api/v1/sessions/sess-task/tasks/task-cancel:detach
    let res_detach = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-task/tasks/task-cancel:detach".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_detach.status, 200);
    let val_detach: Value = serde_json::from_slice(&res_detach.body).unwrap();
    assert_eq!(val_detach["detached"], false);
}

/// #3717 late-settle silence, server wiring: the session row is the
/// liveness source, so a task attributed to a deleted session settles
/// without firing either terminal vocabulary, while a live session's
/// task still announces on its lane and queues its notification.
#[tokio::test]
async fn late_settle_for_a_deleted_session_stays_silent() {
    let server = HttpServer::in_memory().unwrap();
    server.store.create_session("sess-live", None).unwrap();
    let runner = server.task_runner();

    let (created_live, terminated_live) = {
        let events: Arc<std::sync::Mutex<Vec<Value>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = events.clone();
        runner.set_event_sink(Arc::new(move |_session, event| {
            sink.lock().unwrap().push(event);
        }));
        runner
            .spawn_task_with_meta(
                crate::storage::TaskSpawnMeta {
                    session_id: Some("sess-live"),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                },
                "task-live".into(),
                "job".into(),
                async { crate::storage::TaskOutcome::Completed("done".to_string()) },
            )
            .unwrap();
        assert!(matches!(
            runner.wait("task-live", 2000).await,
            crate::storage::task_runner::TaskWaitResult::Completed(_)
        ));

        // The live session's completion went out and its notification
        // queued for the drain.
        let fired = |ty: &str| events.lock().unwrap().iter().any(|e| e["type"] == ty);
        (fired("event.task.completed"), {
            assert_eq!(runner.pending_notification_count(Some("sess-live")), 1);
            fired("background.task.terminated")
        })
    };
    assert!(created_live && terminated_live);

    // A session that no longer exists (row gone): settle stays silent.
    runner
        .spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: Some("sess-gone"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
            },
            "task-gone".into(),
            "job".into(),
            async { crate::storage::TaskOutcome::Completed("done".to_string()) },
        )
        .unwrap();
    assert!(matches!(
        runner.wait("task-gone", 2000).await,
        crate::storage::task_runner::TaskWaitResult::Completed(_)
    ));
    // The entry still settled and stays queryable on the tasks surface.
    assert_eq!(runner.entry("task-gone").unwrap()["status"], "completed");
    assert_eq!(runner.pending_notification_count(Some("sess-gone")), 0);
    assert!(
        runner
            .take_pending_notifications(Some("sess-gone"))
            .is_empty()
    );
}

#[tokio::test]
async fn shutdown_route_cancels_the_serve_token() {
    let server = HttpServer::in_memory().unwrap();
    let token = server.shutdown_token();
    assert!(!token.is_cancelled());

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/shutdown".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    assert!(
        token.is_cancelled(),
        "POST /shutdown must stop the accept loop"
    );
}

#[test]
fn loopback_hosts_are_recognized_and_wildcards_are_not() {
    for host in ["127.0.0.1", "127.0.0.53", "localhost", "::1", "[::1]"] {
        assert!(is_loopback_host(host), "{host} is loopback");
    }
    for host in ["0.0.0.0", "::", "192.168.1.5", "kimi.internal", ""] {
        assert!(!is_loopback_host(host), "{host} is not loopback");
    }
}

#[tokio::test]
async fn debug_routes_need_the_flag_and_a_loopback_bind() {
    let request = || HttpRequest {
        method: "GET".into(),
        path: "/api/v1/debug/channels".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };

    // Off by default: the reflection surface is not a product route.
    let off = HttpServer::in_memory().unwrap();
    assert_eq!(off.handle_request(&request()).await.status, 404);

    // Asked for, on loopback: mounted.
    let on = HttpServer::in_memory().unwrap().with_debug_endpoints(true);
    assert_eq!(on.handle_request(&request()).await.status, 200);

    // Asked for, but reachable from the network: still not mounted.
    let remote = HttpServer::in_memory()
        .unwrap()
        .with_debug_endpoints(true)
        .with_bind_host("0.0.0.0");
    assert_eq!(remote.handle_request(&request()).await.status, 404);
}

#[tokio::test]
async fn shutdown_is_unregistered_on_a_non_loopback_bind() {
    let request = || HttpRequest {
        method: "POST".into(),
        path: "/api/v1/shutdown".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };

    let remote = HttpServer::in_memory().unwrap().with_bind_host("0.0.0.0");
    let token = remote.shutdown_token();
    assert_eq!(remote.handle_request(&request()).await.status, 404);
    assert!(!token.is_cancelled(), "a 404 must not stop the server");

    let opted_in = HttpServer::in_memory()
        .unwrap()
        .with_bind_host("0.0.0.0")
        .with_allow_remote_shutdown(true);
    let token = opted_in.shutdown_token();
    assert_eq!(opted_in.handle_request(&request()).await.status, 200);
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn a_host_outside_the_allowlist_is_refused_before_the_credential() {
    let server = HttpServer::in_memory()
        .unwrap()
        .with_host_guard(HostGuard::new(
            "127.0.0.1",
            vec!["kimi.example".to_string()],
        ));

    let with_host = |host: Option<&str>| {
        let mut headers = HashMap::new();
        if let Some(host) = host {
            headers.insert("host".to_string(), host.to_string());
        }
        HttpRequest {
            method: "GET".into(),
            path: "/api/v1/health".into(),
            query: None,
            headers,
            body: Vec::new(),
        }
    };

    // `/health` answers unauthenticated, so a 403 here can only come from
    // the host guard.
    assert_eq!(
        server
            .handle_request(&with_host(Some("127.0.0.1:58627")))
            .await
            .status,
        200
    );
    assert_eq!(
        server
            .handle_request(&with_host(Some("kimi.example")))
            .await
            .status,
        200
    );

    let refused = server
        .handle_request(&with_host(Some("evil.example")))
        .await;
    assert_eq!(refused.status, 403);
    let body = String::from_utf8_lossy(&refused.body);
    assert!(body.contains("--allowed-host"), "{body}");
    assert!(body.contains("KIMI_CODE_ALLOWED_HOSTS"), "{body}");

    // A missing `Host` is refused too: HTTP/1.1 requires it.
    assert_eq!(server.handle_request(&with_host(None)).await.status, 403);
}

#[tokio::test]
async fn an_in_process_server_without_a_guard_ignores_the_host_header() {
    let server = HttpServer::in_memory().unwrap();
    let mut headers = HashMap::new();
    headers.insert("host".to_string(), "evil.example".to_string());
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/health".into(),
            query: None,
            headers,
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
}

#[tokio::test]
async fn test_http_meta_and_config_endpoints() {
    let server = HttpServer::in_memory().unwrap();

    // 1. Meta endpoint
    let res_meta = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/meta".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_meta.status, 200);
    let val_meta: Value = serde_json::from_slice(&res_meta.body).unwrap();
    assert_eq!(val_meta["backend"], "rust");
    assert_eq!(val_meta["capabilities"]["websocket"], true);
    assert_eq!(val_meta["capabilities"]["tasks"], true);
    assert!(val_meta["server_id"].as_str().unwrap().starts_with("srv-"));
    assert!(val_meta["started_at"].as_str().is_some());

    // 2. Config endpoint
    let res_cfg = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/config".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cfg.status, 200);
    let val_cfg: Value = serde_json::from_slice(&res_cfg.body).unwrap();
    assert!(val_cfg["default_model"].is_string());
    assert!(val_cfg["providers"].is_object());
    assert!(val_cfg["models"].is_object());
    assert!(val_cfg["yolo"].is_boolean());
    assert!(val_cfg["plan_mode"].is_boolean());
    assert!(val_cfg["default_plan_mode"].is_boolean());
    assert!(val_cfg["telemetry"].is_boolean());
    assert!(val_cfg["model_catalog"].is_object());

    // 3. Config POST update
    let res_post_cfg = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/config".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "default_model": "test-custom-model",
                "yolo": true,
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_post_cfg.status, 200);
    let val_post_cfg: Value = serde_json::from_slice(&res_post_cfg.body).unwrap();
    assert_eq!(val_post_cfg["default_model"], "test-custom-model");
    assert_eq!(val_post_cfg["yolo"], true);

    // 4. Config Reload endpoint
    let res_reload = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/config:reload".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_reload.status, 200);
    let val_reload: Value = serde_json::from_slice(&res_reload.body).unwrap();
    assert_eq!(val_reload["status"], "reloaded");

    // 5. Runtime and info endpoints
    let res_runtime = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/runtime".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_runtime.status, 200);
    let val_runtime: Value = serde_json::from_slice(&res_runtime.body).unwrap();
    assert_eq!(val_runtime["backend"], "rust");
    assert!(val_runtime["version"].is_string());

    // 6. Capabilities endpoint
    let res_cap = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/capabilities".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cap.status, 200);
    let val_cap: Value = serde_json::from_slice(&res_cap.body).unwrap();
    let caps = val_cap["capabilities"].as_array().unwrap();
    // Objects, not bare ids: the Web client filters on `supported`, so the
    // list has to carry the same element shape as the detail route.
    assert!(caps.iter().any(|c| c["id"] == "task_runner"));
    assert!(caps.iter().any(|c| c["id"] == "cron_scheduler"));
    assert!(caps.iter().any(|c| c["id"] == "gui_store"));
    assert!(
        caps.iter().all(|c| c["supported"] == true),
        "every built-in capability is compiled in"
    );

    // 6b. Capability detail route: a known id is `ready`, an unknown one is
    // a 404, and the install action honestly refuses (nothing to install).
    let res_cap_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/capabilities/bash".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cap_get.status, 200);
    let val_cap_get: Value = serde_json::from_slice(&res_cap_get.body).unwrap();
    assert_eq!(val_cap_get["id"], "bash");
    assert_eq!(val_cap_get["state"], "ready");
    assert_eq!(val_cap_get["supported"], true);

    let res_cap_missing = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/capabilities/no-such-capability".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cap_missing.status, 404);

    let res_cap_install = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/capabilities/bash:install".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cap_install.status, 400);

    // 6c. Region endpoint: resolves from env / marker / default.
    let res_region = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/oauth/region".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_region.status, 200);
    let val_region: Value = serde_json::from_slice(&res_region.body).unwrap();
    assert!(
        val_region["region"] == "mainland-cn" || val_region["region"] == "global",
        "{}",
        val_region["region"]
    );

    // 7. Shutdown endpoint
    let res_sd = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/shutdown".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sd.status, 200);
    let val_sd: Value = serde_json::from_slice(&res_sd.body).unwrap();
    assert_eq!(val_sd["status"], "shutting_down");

    // 8. GUI Store endpoints
    // setItem
    let res_set = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/gui/store/setItem".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "key": "app_theme",
                "value": "theme_cyberpunk",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_set.status, 200);

    // getItem
    let res_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/gui/store/getItem".into(),
            query: Some("key=app_theme".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_get.status, 200);
    let val_get: Value = serde_json::from_slice(&res_get.body).unwrap();
    assert_eq!(val_get["value"], "theme_cyberpunk");

    // length
    let res_len = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/gui/store/length".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_len.status, 200);
    let val_len: Value = serde_json::from_slice(&res_len.body).unwrap();
    assert_eq!(val_len["length"], 1);

    // removeItem
    let res_rm = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/gui/store/removeItem".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "key": "app_theme",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_rm.status, 200);

    // clear
    let res_clear = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/gui/store/clear".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_clear.status, 200);
}

/// The REST fork surface is where `turnIndex` (v2 `opts.turnIndex`) actually
/// reaches the store. It pins three things the store tests cannot: the body
/// field is parsed at all, a bad value is a 400 rather than a silent
/// whole-session copy, and an out-of-range index does not create a session.
#[tokio::test]
async fn the_fork_route_reads_turn_index_and_rejects_a_bad_one() {
    let server = HttpServer::in_memory().unwrap();
    server.store_arc().create_session("sess-cut", None).unwrap();
    for (n, id) in ["turn-a", "turn-b", "turn-c"].iter().enumerate() {
        server
            .store_arc()
            .save_turn(
                "sess-cut",
                id,
                n as u32 + 1,
                &[
                    crate::turn_loop::types::LLMMessage::user(*id),
                    crate::turn_loop::types::LLMMessage::assistant("reply"),
                ],
                None,
                None,
            )
            .unwrap();
    }

    let fork_with = |body: Value| {
        let server = &server;
        async move {
            server
                .handle_request(&HttpRequest {
                    method: "POST".into(),
                    path: "/api/v1/sessions/sess-cut/fork".into(),
                    query: None,
                    headers: HashMap::new(),
                    body: serde_json::to_vec(&body).unwrap(),
                })
                .await
        }
    };

    // Cutting at turn 1 keeps two of the three turns.
    let res = fork_with(json!({ "turnIndex": 1 })).await;
    assert_eq!(res.status, 200);
    let val: Value = serde_json::from_slice(&res.body).unwrap();
    let sid = val["id"].as_str().unwrap().to_string();
    let forked = server.store_arc().load_session_history(&sid).unwrap();
    assert_eq!(forked.len(), 4, "two turns of two messages each");
    assert!(
        forked.iter().all(|m| m.content != "turn-c"),
        "the third turn is dropped"
    );

    // A non-integer index is a client error, not a whole-session copy.
    let res = fork_with(json!({ "turnIndex": "one" })).await;
    assert_eq!(res.status, 400, "a string turnIndex must be refused");

    // So is a negative one — `as_u64` cannot represent it.
    let res = fork_with(json!({ "turnIndex": -1 })).await;
    assert_eq!(res.status, 400);

    // An index naming a turn the session does not have is a 400 too, and
    // it must not leave a stray child session behind.
    let res = fork_with(json!({ "turnIndex": 99 })).await;
    assert_eq!(res.status, 400);
    let children = server.store_arc().list_children("sess-cut").unwrap();
    assert_eq!(
        children.len(),
        1,
        "only the successful fork created a child; the rejected ones did not"
    );
}

#[tokio::test]
async fn test_http_session_status_abort_and_fork() {
    let server = HttpServer::in_memory().unwrap();
    server
        .store_arc()
        .create_session("sess-test", Some("Original Session"))
        .unwrap();

    // 1. Session status
    let res_status = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-test/status".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_status.status, 200);
    let val_status: Value = serde_json::from_slice(&res_status.body).unwrap();
    assert_eq!(val_status["busy"], false);
    assert_eq!(val_status["permission"], "auto");

    // 2. Abort when no active turn -> false
    let res_abort = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-test/abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_abort.status, 200);
    let val_abort: Value = serde_json::from_slice(&res_abort.body).unwrap();
    assert_eq!(val_abort["aborted"], false);

    // 3. Fork session
    let res_fork = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-test/fork".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "Forked Branch" })).unwrap(),
        })
        .await;
    // v2 `forkSessionResponseSchema = sessionSchema`, answered 200 with the
    // new session's own document.
    assert_eq!(res_fork.status, 200);
    let val_fork: Value = serde_json::from_slice(&res_fork.body).unwrap();
    let new_sid = val_fork["id"].as_str().unwrap();
    assert_eq!(val_fork["title"], "Forked Branch");

    // Forked session can be queried
    let res_forked_status = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{new_sid}/status"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_forked_status.status, 200);

    // 4. BTW side-channel start (both slash and colon syntaxes)
    let res_btw = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-test/btw".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_btw.status, 200);
    let val_btw: Value = serde_json::from_slice(&res_btw.body).unwrap();
    let btw_agent_id = val_btw["agent_id"].as_str().unwrap();
    assert!(btw_agent_id.starts_with("agent-btw-"));

    let res_btw_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-test:btw".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_btw_colon.status, 200);
    let val_btw_colon: Value = serde_json::from_slice(&res_btw_colon.body).unwrap();
    assert!(
        val_btw_colon["agent_id"]
            .as_str()
            .unwrap()
            .starts_with("agent-btw-")
    );

    // 5. Missing session on status/abort/fork/btw -> 404
    let res_missing_status = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/non-existent/status".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_missing_status.status, 404);

    let res_missing_abort = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/non-existent/abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_missing_abort.status, 404);

    let res_missing_fork = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/non-existent/fork".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_missing_fork.status, 404);
}

#[tokio::test]
async fn test_http_static_assets_and_spa_routing() {
    use std::fs::{self, File};
    use std::io::Write;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let index_path = dir.path().join("index.html");
    let mut f1 = File::create(&index_path).unwrap();
    f1.write_all(b"<!DOCTYPE html><html><body>Kimi Web UI</body></html>")
        .unwrap();

    let assets_dir = dir.path().join("assets");
    fs::create_dir(&assets_dir).unwrap();
    let js_path = assets_dir.join("index-123.js");
    let mut f2 = File::create(&js_path).unwrap();
    f2.write_all(b"console.log('web ui loaded');").unwrap();

    let server = HttpServer::in_memory().unwrap().with_web_assets(dir.path());

    // 1. Root / serves index.html
    let res_root = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_root.status, 200);
    assert_eq!(
        res_root.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        res_root.body,
        b"<!DOCTYPE html><html><body>Kimi Web UI</body></html>"
    );

    // 2. Static asset request
    let res_asset = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/assets/index-123.js".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_asset.status, 200);
    assert_eq!(
        res_asset.header("content-type"),
        Some("application/javascript; charset=utf-8")
    );
    assert_eq!(res_asset.body, b"console.log('web ui loaded');");

    // 3. SPA deep route fallback
    let res_spa = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/session/sess-abc".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_spa.status, 200);
    assert_eq!(
        res_spa.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        res_spa.body,
        b"<!DOCTYPE html><html><body>Kimi Web UI</body></html>"
    );

    // 4. API routes do not fallback to index.html
    let res_api = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/missing-route".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_api.status, 404);
}

#[tokio::test]
async fn test_http_workspaces_crud_and_trust() {
    let server = HttpServer::in_memory().unwrap();

    // 1. Initial list is empty
    let res_list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list.status, 200);
    let val_list: Value = serde_json::from_slice(&res_list.body).unwrap();
    assert_eq!(val_list["items"].as_array().unwrap().len(), 0);

    // 2. Create workspace - missing root -> 400
    let res_bad = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(res_bad.status, 400);

    // 3. Create valid workspace
    let res_create = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "root": "/workspace/my-app",
                "name": "My Application"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_create.status, 201);
    let ws: Value = serde_json::from_slice(&res_create.body).unwrap();
    let ws_id = ws["id"].as_str().unwrap().to_string();
    assert!(ws_id.starts_with("wd_my-app_"));
    assert_eq!(ws["name"], "My Application");
    assert_eq!(ws["session_count"], 0);

    // 4. Get workspace by id
    let res_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_get.status, 200);
    let val_get: Value = serde_json::from_slice(&res_get.body).unwrap();
    assert_eq!(val_get["id"], ws_id);

    // 5. Query and toggle trust
    let res_trust = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}/trust"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_trust.status, 200);
    let val_trust: Value = serde_json::from_slice(&res_trust.body).unwrap();
    assert_eq!(val_trust["trusted"], true);

    let res_set_trust = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/workspaces/{ws_id}/trust"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "trusted": false })).unwrap(),
        })
        .await;
    assert_eq!(res_set_trust.status, 200);
    let val_set_trust: Value = serde_json::from_slice(&res_set_trust.body).unwrap();
    assert_eq!(val_set_trust["trusted"], false);

    // 6. Create session under workspace -> session_count updates
    server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "title": "Task 1",
                "workspace_id": ws_id
            }))
            .unwrap(),
        })
        .await;

    let res_get_updated = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let val_updated: Value = serde_json::from_slice(&res_get_updated.body).unwrap();
    assert_eq!(val_updated["session_count"], 1);

    // 6b. Rename workspace via PATCH /api/v1/workspaces/{ws_id}
    let res_patch = server
        .handle_request(&HttpRequest {
            method: "PATCH".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "name": "Renamed Application"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_patch.status, 200);
    let val_patched: Value = serde_json::from_slice(&res_patch.body).unwrap();
    assert_eq!(val_patched["name"], "Renamed Application");

    // 7. Delete workspace
    let res_del = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del.status, 200);

    // 8. Delete non-existent -> 404
    let res_del_missing = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del_missing.status, 404);
}

#[tokio::test]
async fn test_http_models_catalog_and_session_export() {
    let server = HttpServer::in_memory().unwrap();
    // The catalog routes project `config.toml`, so inject a deterministic
    // one instead of discovering the developer's config.
    let config: crate::config::KimiConfig = r#"
default_model = "kimi-latest"

[providers.kimi]
type = "openai"
api_key = "sk-test"
base_url = "https://example.test/v1"

[models.kimi-latest]
provider = "kimi"
model = "kimi-latest"
max_context_size = 262144

[models.fast]
provider = "kimi"
model = "k3-fast"
max_context_size = 128000
"#
    .parse()
    .unwrap();
    *server.config_override.lock().await = Some(config);

    // 1. Models catalog (config-driven, v2 `IModelCatalog.listModels`)
    let res_models = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/models".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_models.status, 200);
    let val_models: Value = serde_json::from_slice(&res_models.body).unwrap();
    let items = val_models["items"].as_array().unwrap();
    let model = items
        .iter()
        .find(|item| item["model"] == "kimi-latest")
        .unwrap();
    assert_eq!(model["provider"], "kimi");
    assert_eq!(model["display_name"], "kimi-latest");
    assert_eq!(model["max_context_size"], 262144);

    // Alternate /model-catalog endpoint also returns 200
    let res_catalog = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/model-catalog".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_catalog.status, 200);

    // 2. Session export
    server
        .store_arc()
        .create_session("sess-to-export", Some("To Export"))
        .unwrap();
    server
        .store_arc()
        .save_turn(
            "sess-to-export",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user(
                "Please export me",
            )],
            None,
            None,
        )
        .unwrap();

    let res_exp_get = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-to-export/export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_exp_get.status, 200);
    let val_exp: Value = serde_json::from_slice(&res_exp_get.body).unwrap();
    assert_eq!(val_exp["session"]["session_id"], "sess-to-export");
    assert_eq!(val_exp["turns_count"], 1);
    assert_eq!(val_exp["messages"].as_array().unwrap().len(), 1);
    assert!(val_exp["exported_at"].is_string());

    // POST export works symmetrically
    let res_exp_post = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-to-export/export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_exp_post.status, 200);

    // Export non-existent returns 404
    let res_exp_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-non-existent/export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_exp_none.status, 404);

    // 3. Providers listing (config-driven, credential state included)
    let res_providers = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/providers".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_providers.status, 200);
    let val_prov: Value = serde_json::from_slice(&res_providers.body).unwrap();
    let providers = val_prov["items"].as_array().unwrap();
    let kimi = providers.iter().find(|item| item["id"] == "kimi").unwrap();
    assert_eq!(kimi["type"], "openai");
    assert_eq!(kimi["has_api_key"], true);
    assert_eq!(kimi["status"], "connected");
    assert_eq!(kimi["default_model"], "kimi-latest");
    let kimi_models = kimi["models"].as_array().unwrap();
    assert_eq!(kimi_models.len(), 2);
    assert!(kimi_models.contains(&json!("kimi-latest")));
    assert!(kimi_models.contains(&json!("fast")));

    // 3b. Auth readiness reflects the injected config.
    let res_auth = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/auth".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_auth.status, 200);
    let val_auth: Value = serde_json::from_slice(&res_auth.body).unwrap();
    assert_eq!(val_auth["models_ready"], true);
    assert_eq!(val_auth["providers_count"], 1);
    assert_eq!(val_auth["managed_provider"], Value::Null);

    // 4. Catalog providers listing
    let res_catalog_prov = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/catalog/providers".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_catalog_prov.status, 200);
    let val_cat_prov: Value = serde_json::from_slice(&res_catalog_prov.body).unwrap();
    assert!(val_cat_prov["items"].as_array().unwrap().len() >= 4);

    // 5. Set default model via POST /api/v1/models/{tail}
    let res_set_default = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/models/fast:set_default".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_set_default.status, 200);
    let val_set_def: Value = serde_json::from_slice(&res_set_default.body).unwrap();
    assert_eq!(val_set_def["model"], "fast");

    // 6. Prompts list
    let res_prompts = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_prompts.status, 200);

    // 7. API v2 sessions
    let res_v2_sess = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_v2_sess.status, 200);
    let val_v2: Value = serde_json::from_slice(&res_v2_sess.body).unwrap();
    assert!(val_v2["items"].as_array().is_some());

    // 8. Session init: POST /api/v1/sessions/:id:init
    let res_init = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-to-export:init".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_init.status, 200);
    let val_init: Value = serde_json::from_slice(&res_init.body).unwrap();
    assert_eq!(val_init["status"], "initiated");
    assert!(val_init["prompt"].as_str().unwrap().contains("AGENTS.md"));
}

/// The full server path for an unconsumed steer (v2's semantics; the
/// session path's session-scoped queue behaves the same): a prompt runs
/// against a model endpoint that hangs mid-stream, a queued prompt is
/// steered into the running turn, the turn is aborted before the steer is
/// drained — the cancel check runs at the step top, ahead of
/// `drain_steers` — and the next prompt's turn must carry the steered text
/// exactly once. Before `release_turn_steer_state`, the steer died with the
/// aborted turn and the text appeared nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_survives_a_cancel_and_joins_the_next_turn_over_rest() {
    // A local OpenAI-compatible endpoint: the first request streams one
    // delta and then holds the connection (only a cancellation ends that
    // turn); every later request answers a complete turn.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let first_request = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mock = {
        let first_request = first_request.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let first_request = first_request.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let hanging = first_request.swap(false, std::sync::atomic::Ordering::SeqCst);
                    let body = if hanging {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n".to_string()
                    } else {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                             data: [DONE]\n\n"
                            .to_string()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.flush().await;
                    if hanging {
                        // Hold the connection open: the turn only ends through cancellation.
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                });
            }
        })
    };

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let engine = ServerEngine::new(
        crate::pipeline::PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "mock-model".into(),
            providers: Vec::new(),
            native_llm: Some(crate::rpc::types::NativeLlmConfig {
                protocol: "openai".into(),
                base_url: format!("http://{addr}/v1"),
                api_key: "test-key".into(),
                model: "mock-model".into(),
                ..Default::default()
            }),
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: true,
            shell_path: None,
            policy_snapshot: None,
            github_token: None,
            github_base_url: None,
            subagent_timeout_ms: None,
            agent_tool_veto: None,
            tools_veto: None,
            todo_tool_veto: None,
            tower_worktree_root: None,
            tower_enabled: false,
            tool_select: false,
            sandbox_mode: None,
            sandbox_policy: None,
            caller_agent_id: None,
            session_id: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            background: crate::storage::BackgroundLimits::default(),
        },
        hub.clone(),
        store.clone(),
    );
    let server = HttpServer::with_hub(store.clone(), hub).with_engine(engine);
    let engine = server.engine().expect("the server holds its engine");
    store.create_session("sess-steer-cancel", None).unwrap();

    let post = |path: &str, body: Value| HttpRequest {
        method: "POST".into(),
        path: path.into(),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&body).unwrap(),
    };
    async fn wait_until(deadline_secs: u64, mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "condition not met within {deadline_secs}s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // 1. The first prompt runs against the hanging endpoint.
    let first = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts",
            json!({ "prompt": "first" }),
        ))
        .await;
    assert_eq!(first.status, 200);
    let first_item: Value = serde_json::from_slice(&first.body).unwrap();
    let first_id = first_item["prompt_id"].as_str().unwrap().to_string();
    assert_eq!(first_item["status"], "running");
    wait_until(10, || engine.is_turn_active("sess-steer-cancel")).await;

    // 2. A second prompt queues behind it and is steered into the running turn.
    let second = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts",
            json!({ "prompt": "steered-text" }),
        ))
        .await;
    let second_item: Value = serde_json::from_slice(&second.body).unwrap();
    let second_id = second_item["prompt_id"].as_str().unwrap().to_string();
    assert_eq!(second_item["status"], "queued");

    let steer = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts:steer",
            json!({ "prompt_ids": [second_id] }),
        ))
        .await;
    assert_eq!(steer.status, 200);

    // 3. Abort the running turn before its next step head drains the steer.
    let abort = server
        .handle_request(&post(
            &format!("/api/v1/sessions/sess-steer-cancel/prompts/{first_id}:abort"),
            json!({}),
        ))
        .await;
    assert_eq!(abort.status, 200);
    wait_until(10, || !engine.is_turn_active("sess-steer-cancel")).await;
    assert_eq!(
        engine.queued_steer_count("sess-steer-cancel"),
        1,
        "the undrained steer survives the cancelled turn"
    );

    // 4. The next prompt's turn drains the survivor at its first step head.
    let third = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts",
            json!({ "prompt": "third" }),
        ))
        .await;
    assert_eq!(third.status, 200);

    // 5. The steered text is persisted exactly once, inside the new turn.
    let steered_turn = std::sync::Mutex::new(None);
    wait_until(10, || {
        let messages = store.load_session_messages("sess-steer-cancel").unwrap();
        let hits: Vec<&crate::session::sqlite_store::StoredMessage> = messages
            .iter()
            .filter(|m| m.message.content.contains("steered-text"))
            .collect();
        if hits.len() > 1 {
            panic!(
                "the steered text must appear exactly once, saw {}",
                hits.len()
            );
        }
        if let Some(hit) = hits.first() {
            *steered_turn.lock().unwrap() = Some(hit.turn_id.clone());
        }
        !hits.is_empty()
    })
    .await;
    let steered_turn = steered_turn.into_inner().unwrap();
    let messages = store.load_session_messages("sess-steer-cancel").unwrap();
    let third_turn = messages
        .iter()
        .find(|m| m.message.content == "third")
        .map(|m| m.turn_id.clone())
        .expect("the third prompt is persisted");
    assert_eq!(
        steered_turn.as_deref(),
        Some(third_turn.as_str()),
        "the rescued steer belongs to the turn that ran after the cancel"
    );
    mock.abort();
}

#[tokio::test]
async fn prompt_queue_routes_reflect_state() {
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-pq", None).unwrap();

    // Idle session: no active prompt and an empty queue.
    let list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-pq/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(list.status, 200);
    let list_body: Value = serde_json::from_slice(&list.body).unwrap();
    assert!(list_body["active"].is_null());
    assert_eq!(list_body["queued"], json!([]));

    // Steering an unknown prompt id is PROMPT_NOT_FOUND, not a fake success.
    let steer = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-pq/prompts:steer".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "prompt_ids": ["nope"] })).unwrap(),
        })
        .await;
    assert_eq!(steer.status, 404);

    // Aborting an unknown prompt is PROMPT_NOT_FOUND, not a fake success.
    let abort = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-pq/prompts/nope:abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(abort.status, 404);
    let abort_body: Value = serde_json::from_slice(&abort.body).unwrap();
    assert_eq!(abort_body["code"], 40402);
}

/// ROADMAP §7.6: the web UI's abort button sends the live
/// `event.message.created` id (`msg-u{turn}`). Before the message-id
/// resolution landed, the route answered 404 for exactly the id the
/// button holds.
#[tokio::test]
async fn abort_resolves_the_live_user_message_id_at_the_route() {
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-abort", None).unwrap();

    let (item, run) = server.prompt_queue.admit(
        "sess-abort",
        json!({
            "prompt_id": "prompt-1",
            "user_message_id": "msg-prompt-1",
            "content": [{ "type": "text", "text": "hi" }],
            "created_at": "2026-01-01T00:00:00Z",
        }),
        "hi".into(),
        Vec::new(),
        None,
    );
    assert!(run.is_some(), "the first prompt is active");
    assert_eq!(item["status"], "running");
    // The driver stamps the turn the live event names.
    server.prompt_queue.stamp_active_turn("sess-abort", 2);

    let abort = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-abort/prompts/msg-u2:abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(abort.status, 200);
    let abort_body: Value = serde_json::from_slice(&abort.body).unwrap();
    assert_eq!(abort_body["aborted"], true);
    assert!(server.prompt_queue.active_is_cancelled("sess-abort"));
}

#[tokio::test]
async fn prompt_submission_options_persist_to_session_profile() {
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-opt", None).unwrap();
    let body = json!({
        "content": [{ "type": "text", "text": "hi" }],
        "model": "alias-2",
        "thinking": "high",
        "permission_mode": "yolo",
        "plan_mode": true,
        "disabled_tools": ["Bash"],
        "profile": "coder",
        "metadata": { "title": "T" },
    });
    apply_prompt_submission_options(&server, "sess-opt", &body).unwrap();

    let config = server
        .store()
        .get_state("agent_config", "sess-opt")
        .unwrap()
        .unwrap();
    assert_eq!(config["model"], "alias-2");
    assert_eq!(config["thinking"], "high");
    assert_eq!(config["plan_mode"], true);
    assert_eq!(config["disabled_tools"], json!(["Bash"]));
    assert_eq!(config["profile"], "coder");

    let metadata = server
        .store()
        .get_state("metadata", "sess-opt")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["permission_mode"], "yolo");
    assert_eq!(metadata["title"], "T");

    // An unknown permission mode is rejected before any turn runs.
    let bad = json!({
        "content": [{ "type": "text", "text": "x" }],
        "permission_mode": "nope",
    });
    assert!(apply_prompt_submission_options(&server, "sess-opt", &bad).is_err());
}

#[tokio::test]
async fn prompt_plan_mode_activates_workspace_plan_state() {
    let dir = std::env::temp_dir().join(format!("kimi-plan-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-plan", None).unwrap();
    let body = json!({
        "content": [{ "type": "text", "text": "plan it" }],
        "plan_mode": true,
        "metadata": { "cwd": dir.to_string_lossy() },
    });
    apply_prompt_submission_options(&server, "sess-plan", &body).unwrap();

    let state = crate::storage::StateStore::for_workspace(&dir).unwrap();
    assert_eq!(state.read_domain("plan").unwrap()["active"], true);
}

#[tokio::test]
async fn test_http_provider_crud_writes_config() {
    let dir = std::env::temp_dir().join(format!("kimi-provider-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        "default_model = \"\"\n\n[providers.openai]\ntype = \"openai\"\napi_key = \"sk-old\"\n",
    )
    .unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());

    fn request(method: &str, path: &str, body: Option<&Value>) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            path: path.into(),
            query: None,
            headers: HashMap::new(),
            body: body
                .map(|value| serde_json::to_vec(value).unwrap())
                .unwrap_or_default(),
        }
    }

    // 1. Create writes the provider + aliases and seeds the unset default.
    let create_body = json!({
        "id": "kimi-code",
        "type": "openai",
        "api_key": "sk-test",
        "base_url": "https://example.test/v1",
        "default_model": "k3",
        "models": [
            { "model": "k3", "max_context_size": 200000, "display_name": "K3" },
            { "model": "fast", "max_context_size": 128000 }
        ]
    });
    let res = server
        .handle_request(&request("POST", "/api/v1/providers", Some(&create_body)))
        .await;
    assert_eq!(res.status, 201);
    let created: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(created["id"], "kimi-code");
    assert_eq!(created["has_api_key"], true);
    assert_eq!(created["status"], "connected");
    assert_eq!(created["default_model"], "kimi-code/k3");

    // 2. A duplicate id is a PROVIDER_ALREADY_EXISTS conflict.
    let res = server
        .handle_request(&request("POST", "/api/v1/providers", Some(&create_body)))
        .await;
    assert_eq!(res.status, 409);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40921);

    // 3. The single-provider read reveals the stored key.
    let res = server
        .handle_request(&request("GET", "/api/v1/providers/kimi-code", None))
        .await;
    assert_eq!(res.status, 200);
    let fetched: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(fetched["api_key"], "sk-test");

    // 4. The write is format-preserving and the read side sees it.
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.kimi-code]"), "{text}");
    assert!(text.contains("[models.\"kimi-code/k3\"]"), "{text}");
    assert!(text.contains("default_model = \"kimi-code/k3\""), "{text}");
    assert!(text.contains("[providers.openai]"), "{text}");
    let res = server
        .handle_request(&request("GET", "/api/v1/models", None))
        .await;
    let models: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(
        models["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["model"] == "kimi-code/k3")
    );

    // 5. Replace with a rename migrates default_model, drops stale aliases.
    let res = server
        .handle_request(&request(
            "PUT",
            "/api/v1/providers/kimi-code",
            Some(&json!({
                "new_id": "kimi-code-2",
                "type": "anthropic",
                "api_key": "sk-new",
                "base_url": "https://example.test/v2",
                "default_model": "k3",
                "models": [{ "model": "k3", "max_context_size": 200000 }]
            })),
        ))
        .await;
    assert_eq!(res.status, 200);
    let replaced: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(replaced["provider"]["id"], "kimi-code-2");
    assert_eq!(replaced["provider"]["type"], "anthropic");
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.kimi-code-2]"), "{text}");
    assert!(!text.contains("[providers.kimi-code]"), "{text}");
    assert!(
        text.contains("default_model = \"kimi-code-2/k3\""),
        "{text}"
    );
    assert!(!text.contains("kimi-code/fast"), "{text}");

    // 6. OAuth-managed providers refuse writes; delete cleans aliases.
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str(
            "\n[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\noauth = { provider = \"managed:kimi-code\" }\n",
        );
    std::fs::write(&config_path, text).unwrap();
    *server.config_override.lock().await = None;

    let res = server
        .handle_request(&request(
            "DELETE",
            "/api/v1/providers/managed%3Akimi-code",
            None,
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40003);

    let res = server
        .handle_request(&request("DELETE", "/api/v1/providers/kimi-code-2", None))
        .await;
    assert_eq!(res.status, 200);
    let deleted: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(deleted["deleted"], true);
    let text = std::fs::read_to_string(&config_path).unwrap();
    // The provider and its aliases are gone; the (now dangling) default
    // pointer is the user's setting and stays, matching kap-server.
    assert!(!text.contains("[providers.kimi-code-2]"), "{text}");
    assert!(!text.contains("[models.\"kimi-code-2/k3\"]"), "{text}");
    assert!(
        text.contains("default_model = \"kimi-code-2/k3\""),
        "{text}"
    );
    assert!(text.contains("[providers.openai]"), "{text}");

    // 7. Validation failures carry the kap-server validation code.
    let res = server
        .handle_request(&request(
            "POST",
            "/api/v1/providers",
            Some(&json!({ "id": "bad id!", "type": "openai", "models": [] })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40001);

    std::fs::remove_dir_all(&dir).ok();
}

fn catalog_request(method: &str, path: &str, body: Option<&Value>) -> HttpRequest {
    HttpRequest {
        method: method.into(),
        path: path.into(),
        query: None,
        headers: HashMap::new(),
        body: body
            .map(|value| serde_json::to_vec(value).unwrap())
            .unwrap_or_default(),
    }
}

/// A one-connection-at-a-time registry server: the response (status line
/// and JSON body) can be swapped between requests, and every request head
/// is captured so the auth header can be asserted.
struct RegistryServer {
    url: String,
    response: Arc<std::sync::Mutex<(String, String)>>,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
}

impl RegistryServer {
    async fn spawn(status: &str, body: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = Arc::new(std::sync::Mutex::new((
            status.to_string(),
            body.to_string(),
        )));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let shared_response = response.clone();
        let shared_requests = requests.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    continue;
                }
                shared_requests
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let (status, body) = shared_response.lock().unwrap().clone();
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        Self {
            url: format!("http://{addr}/api.json"),
            response,
            requests,
        }
    }

    fn set_response(&self, status: &str, body: &str) {
        *self.response.lock().unwrap() = (status.to_string(), body.to_string());
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn catalog_routes_fall_back_to_the_builtin_when_the_fetch_fails() {
    let mut server = HttpServer::in_memory().unwrap();
    server.models_dev_cache =
        Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(|| {
            Box::pin(async { Err("upstream down".to_string()) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        })));

    let res = server
        .handle_request(&catalog_request("GET", "/api/v1/catalog/providers", None))
        .await;
    assert_eq!(res.status, 200);
    let list: Value = serde_json::from_slice(&res.body).unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "the built-in snapshot ends the chain");
    assert!(items.iter().any(|item| item["id"] == "moonshot"));

    // The per-id route serves the same entry the list advertises.
    let res = server
        .handle_request(&catalog_request(
            "GET",
            "/api/v1/catalog/providers/anthropic",
            None,
        ))
        .await;
    assert_eq!(res.status, 200);
    let item: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(item["id"], "anthropic");
    assert_eq!(item["wire_type"], "anthropic");
    assert_eq!(item["base_url"], "https://api.anthropic.com");

    // An unknown id is a 404, not a fabricated entry.
    let res = server
        .handle_request(&catalog_request(
            "GET",
            "/api/v1/catalog/providers/nope",
            None,
        ))
        .await;
    assert_eq!(res.status, 404);
}

#[tokio::test]
async fn import_registry_writes_providers_with_their_source() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let registry = json!({
        "acme": {
            "id": "acme",
            "name": "Acme",
            "api": "https://registry.example.test/v1",
            "type": "openai",
            "env": ["ACME_API_KEY"],
            "models": { "big": { "id": "big-1", "limit": { "context": 128000 } } },
        },
        "beta": {
            "id": "beta",
            "name": "Beta",
            "api": "https://beta.example.test/v1",
            "type": "anthropic",
            "models": {
                "sonnet": {
                    "id": "sonnet-1",
                    "limit": { "context": 200000 },
                    "tool_call": true,
                    "reasoning": true,
                },
            },
        },
        "broken": { "id": "broken", "name": "Broken" },
    });
    let upstream = RegistryServer::spawn("200 OK", &registry.to_string()).await;

    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url, "api_key": "sk-reg" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let imported: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(imported["models_imported"], 2);
    let providers = imported["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2, "the invalid entry is skipped");
    assert_eq!(imported["credential_env"]["acme"], "ACME_API_KEY");
    // The Bearer key reached the registry.
    let requests = upstream.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("authorization: Bearer sk-reg")),
        "{requests:?}"
    );

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.acme]"), "{text}");
    assert!(text.contains("[providers.acme.source]"), "{text}");
    assert!(text.contains("kind = \"apiJson\""), "{text}");
    assert!(text.contains("apiKey = \"sk-reg\""), "{text}");
    assert!(text.contains("[models.\"acme/big\"]"), "{text}");
    assert!(text.contains("[models.\"beta/sonnet\"]"), "{text}");
    // The unset default is seeded from the first entry's first model.
    assert!(text.contains("default_model = \"acme/big\""), "{text}");

    // A re-import of the same URL with one provider gone removes the
    // vanished provider and its aliases; the URL is the identity, so the
    // stored key is reused when the request omits it.
    upstream.set_response(
        "200 OK",
        &json!({
            "acme": {
                "id": "acme",
                "name": "Acme",
                "api": "https://registry.example.test/v1",
                "type": "openai",
                "models": { "big": { "id": "big-1", "limit": { "context": 128000 } } },
            },
        })
        .to_string(),
    );
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(!text.contains("[providers.beta]"), "{text}");
    assert!(!text.contains("beta/sonnet"), "{text}");
    assert!(text.contains("[providers.acme]"), "{text}");
    // v2's remove-then-apply clears a default that pointed into the
    // re-imported provider, and its `hadDefault` reads the pre-apply
    // value, so it is not reseeded — the port mirrors that.
    assert!(!text.contains("default_model"), "{text}");
}

#[tokio::test]
async fn import_registry_rejects_the_unimportable() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"acme/big\"\n").unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let upstream = RegistryServer::spawn("200 OK", "{}").await;

    // A missing url is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({})),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40001);

    // An empty registry has no importable providers.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40005);
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("no importable providers")
    );

    // An upstream failure carries the status in the reason.
    upstream.set_response("401 Unauthorized", r#"{"message":"bad key"}"#);
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url, "api_key": "sk-bad" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40005);
    let msg = body["msg"].as_str().unwrap();
    assert!(msg.contains("cannot be imported"), "{msg}");
    assert!(msg.contains("401"), "{msg}");
    assert!(msg.contains("bad key"), "{msg}");

    // An OAuth-managed provider refuses the import.
    upstream.set_response(
        "200 OK",
        &json!({
            "acme": {
                "id": "acme",
                "name": "Acme",
                "api": "https://registry.example.test/v1",
                "type": "openai",
                "models": { "big": { "id": "big-1", "limit": { "context": 128000 } } },
            },
        })
        .to_string(),
    );
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[providers.acme]\ntype = \"openai\"\noauth = { provider = \"acme\" }\n");
    std::fs::write(&config_path, text).unwrap();
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40003);
}

#[tokio::test]
async fn import_catalog_falls_back_to_the_builtin_when_the_fetch_fails() {
    // The offline path a blocked-network user takes: the upstream fetch
    // fails, so the import resolves against the built-in snapshot.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let mut server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    server.models_dev_cache =
        Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(|| {
            Box::pin(async { Err("upstream down".to_string()) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        })));

    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "anthropic", "api_key": "sk-offline" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let imported: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(imported["models_imported"], 1);
    assert_eq!(imported["provider"]["id"], "anthropic");
    assert_eq!(imported["provider"]["type"], "anthropic");
    assert_eq!(imported["provider"]["status"], "connected");

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.anthropic]"), "{text}");
    assert!(
        text.contains("base_url = \"https://api.anthropic.com\""),
        "{text}"
    );
    assert!(
        text.contains("[models.\"anthropic/claude-3-7-sonnet-20250219\"]"),
        "{text}"
    );
    assert!(
        text.contains("default_model = \"anthropic/claude-3-7-sonnet-20250219\""),
        "{text}"
    );
}

#[tokio::test]
async fn import_catalog_writes_the_provider_and_its_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let mut server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let catalog = json!({
        "acme": {
            "id": "acme",
            "name": "Acme",
            "type": "openai",
            "api": "https://api.example.test/v1",
            "env": ["ACME_API_KEY"],
            "models": {
                "big": {
                    "id": "big",
                    "name": "Big",
                    "limit": { "context": 128000, "input": 64000 },
                    "reasoning_options": [{ "type": "effort", "values": ["low", "high"] }],
                },
                "small": { "id": "small", "limit": { "context": 8192 } },
            },
        },
    });
    server.models_dev_cache = Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(
        move || {
            let catalog = catalog.clone();
            Box::pin(async move { Ok(catalog) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        },
    )));

    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme", "api_key": "sk-acme" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let imported: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(imported["models_imported"], 2);
    assert_eq!(imported["provider"]["id"], "acme");
    assert_eq!(imported["provider"]["type"], "openai");
    assert_eq!(imported["provider"]["status"], "connected");

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.acme]"), "{text}");
    assert!(text.contains("type = \"openai\""), "{text}");
    assert!(text.contains("api_key = \"sk-acme\""), "{text}");
    assert!(
        text.contains("base_url = \"https://api.example.test/v1\""),
        "{text}"
    );
    assert!(text.contains("[models.\"acme/big\"]"), "{text}");
    assert!(text.contains("[models.\"acme/small\"]"), "{text}");
    // The unset default is seeded from the first imported model.
    assert!(text.contains("default_model = \"acme/big\""), "{text}");
    // The always-thinking rename and the capped input size ride along.
    assert!(text.contains("always_thinking"), "{text}");
    assert!(text.contains("max_input_size = 64000"), "{text}");

    // A re-import under a new id rewrites that entry; the old provider's
    // aliases survive (v2 filters by the target id) and the credential
    // carries over when the request supplies none.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme", "id": "acme-2" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.acme-2]"), "{text}");
    assert!(text.contains("[models.\"acme-2/big\"]"), "{text}");
    assert!(text.contains("[models.\"acme/big\"]"), "{text}");
    assert!(text.contains("api_key = \"sk-acme\""), "{text}");
    // The seeded default is not rewritten by the re-import.
    assert!(text.contains("default_model = \"acme/big\""), "{text}");
}

#[tokio::test]
async fn import_catalog_rejects_the_unimportable() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"acme/big\"\n").unwrap();
    let mut server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let catalog = json!({
        "acme": {
            "id": "acme",
            "name": "Acme",
            "api": "https://api.example.test/v1",
            "models": { "big": { "id": "big", "limit": { "context": 128000 } } },
        },
        "gateway": { "id": "gateway", "name": "Gateway", "npm": "@acme/gateway" },
        "bedrock": { "id": "bedrock", "name": "Bedrock", "type": "bedrock" },
        "empty": { "id": "empty", "name": "Empty", "api": "https://api.example.test" },
    });
    server.models_dev_cache = Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(
        move || {
            let catalog = catalog.clone();
            Box::pin(async move { Ok(catalog) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        },
    )));

    // An unknown catalog entry is CATALOG_ENTRY_NOT_FOUND.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "nope" })),
        ))
        .await;
    assert_eq!(res.status, 404);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40417);

    // A missing catalog_id is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({})),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40001);

    // Every unimportable-entry branch carries CATALOG_IMPORT_INVALID.
    for catalog_id in ["gateway", "bedrock", "empty"] {
        let res = server
            .handle_request(&catalog_request(
                "POST",
                "/api/v1/providers:import_catalog",
                Some(&json!({ "catalog_id": catalog_id })),
            ))
            .await;
        assert_eq!(res.status, 400, "{catalog_id}");
        let body: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(body["code"], 40004, "{catalog_id}");
    }

    // An entry that needs an endpoint is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "gateway" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("requires a base_url")
    );

    // A proprietary-SDK entry is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "bedrock" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);

    // An entry with no importable models is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "empty" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("no importable models")
    );

    // An unusable target id is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme", "id": "bad id!" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);

    // An OAuth-managed provider refuses the import.
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[providers.acme]\ntype = \"openai\"\noauth = { provider = \"acme\" }\n");
    std::fs::write(&config_path, text).unwrap();
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40003);
}

#[tokio::test]
async fn provider_create_publishes_config_changed_on_the_global_lane() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let mut sub = server.hub().attach();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/providers".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "id": "kimi-code",
                "type": "openai",
                "api_key": "sk-test",
                "base_url": "https://example.test/v1",
                "default_model": "k3",
                "models": [{ "model": "k3", "max_context_size": 200000 }]
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res.status, 201);

    let ev = sub.recv().await.unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.config.changed");
    let crate::events::EngineEvent::ConfigChanged {
        changed_fields,
        config,
    } = &ev.event
    else {
        panic!("expected ConfigChanged, got {:?}", ev.event);
    };
    assert_eq!(changed_fields, &vec!["providers".to_string()]);
    // The payload carries the post-write config, so the client can fold
    // it instead of re-fetching.
    assert_eq!(config["providers"]["kimi-code"]["type"], "openai");
    assert_eq!(config["default_model"], "kimi-code/k3");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn model_catalog_changed_publishes_on_the_global_lane() {
    let server = HttpServer::in_memory().unwrap();
    let mut sub = server.hub().attach();
    server.publish_model_catalog_changed(&json!({
            "changed": [{ "provider_id": "kimi-code", "provider_name": "Kimi Code", "added": 2, "removed": 1 }],
            "unchanged": ["other"],
            "failed": [],
        }));

    let ev = sub.recv().await.unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.model_catalog.changed");
    let crate::events::EngineEvent::Custom(value) = &ev.event else {
        panic!("expected Custom event");
    };
    assert_eq!(value["changed"][0]["added"], 2);
    assert_eq!(value["unchanged"][0], "other");
    assert!(value["failed"].as_array().unwrap().is_empty());
}

/// The v3 `config.warning` entity's producer: a `config.toml` with a
/// malformed `[models]` entry reaches the global lane as
/// `event.config.warning`, the event the global translator turns into the
/// entity. The warning is captured at config load, which happens before
/// the server exists, so this exercises the staging seam too.
#[tokio::test]
async fn a_malformed_model_entry_reaches_the_global_lane_as_a_config_warning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
default_model = "typo"

[models.typo]
provider = "kimi"
max_context_size = 1000
"#,
    )
    .unwrap();
    let config = crate::config::KimiConfig::from_file(&path).unwrap();
    assert_eq!(config.config_warnings.len(), 1);

    let server = HttpServer::in_memory().unwrap();
    let mut sub = server.hub().attach();
    let server = server.with_config(config);
    // The seeded config is what `config()` reads back, so the warning list
    // and the config view come from one load rather than two that could
    // differ.
    assert_eq!(server.config().await.config_warnings.len(), 1);

    let ev = sub.recv().await.unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.config.warning");
    let crate::events::EngineEvent::Custom(value) = &ev.event else {
        panic!("expected Custom event, got {:?}", ev.event);
    };
    // The shape the global translator reads: `warnings[].message`.
    let warnings = value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{value}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap()
            .contains("[models] entry 'typo' is missing"),
        "{value}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A config that loaded cleanly must not publish an empty warning entity
/// *at startup*: a fresh daemon has no stale advisory to clear, and the
/// entity would sit in the global lane's replay ring saying nothing.
/// (The reload path is the opposite — see the test below.)
#[tokio::test]
async fn a_clean_config_publishes_no_config_warning_at_startup() {
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config(Default::default());

    // Nothing is on the global lane, so the subscription's replay is empty
    // and `recv` only returns on a publish. Any event would prove the
    // publish happened; a timeout proves none did.
    let mut sub = server.hub().attach();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv())
            .await
            .is_err(),
        "a clean config must not broadcast a config.warning"
    );
}

/// The clear-the-stale-advisory path the reload route exists for: a server
/// started on a broken `config.toml` warns, and reloading a repaired file
/// publishes `warnings: []` so the client drops the advisory it is holding.
/// Upstream has the same asymmetry — `publishConfigWarnings`
/// (`kap-server/src/start.ts`) publishes on every
/// diagnostics change with no empty check, and only its startup call
/// filters.
#[tokio::test]
async fn reloading_a_repaired_config_publishes_an_empty_warning_list() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let broken = r#"
default_model = "typo"

[models.typo]
provider = "kimi"
max_context_size = 1000
"#;
    std::fs::write(&path, broken).unwrap();

    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(path.clone());
    let mut sub = server.hub().attach();
    let server = server.with_config(crate::config::KimiConfig::from_file(&path).unwrap());

    // Startup: the broken entry is announced.
    let ev = sub.recv().await.unwrap();
    assert_eq!(ev.event.event_type(), "event.config.warning");
    let crate::events::EngineEvent::Custom(value) = &ev.event else {
        panic!("expected Custom event, got {:?}", ev.event);
    };
    assert_eq!(value["warnings"].as_array().unwrap().len(), 1, "{value}");

    // The user fixes the file and reloads.
    std::fs::write(
        &path,
        "[models.good]\nprovider = \"kimi\"\nmodel = \"kimi-k2\"\n",
    )
    .unwrap();
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/config:reload".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);

    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub.recv())
        .await
        .expect("a reload of a repaired config must publish, not go silent")
        .unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.config.warning");
    let crate::events::EngineEvent::Custom(value) = &ev.event else {
        panic!("expected Custom event, got {:?}", ev.event);
    };
    assert!(
        value["warnings"].as_array().unwrap().is_empty(),
        "a repaired config must clear the client's advisory: {value}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_http_provider_refresh_discovers_managed_models() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A mock `GET /models` whose payload the test can swap between calls.
    let payload = Arc::new(std::sync::Mutex::new(json!({
        "data": [
            { "id": "k3", "limit": { "context": 200000 }, "display_name": "K3", "reasoning": { "type": "both" } },
            { "id": "k3-fast", "limit": { "context": 128000 } }
        ]
    })));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let payload_for_server = payload.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let payload = payload_for_server
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let _ = sock.read(&mut buffer).await;
                let body = payload.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });

    // Cached OAuth credential the refresh authenticates with.
    let credentials =
        std::env::temp_dir().join(format!("kimi-refresh-creds-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&credentials).unwrap();
    std::fs::write(
        credentials.join("kimi.json"),
        json!({
            "access_token": "token-1",
            "refresh_token": "refresh-1",
            "expires_at": chrono::Utc::now().timestamp() + 3600,
            "scope": "kimi",
            "token_type": "Bearer",
            "expires_in": 3600,
        })
        .to_string(),
    )
    .unwrap();

    let dir = std::env::temp_dir().join(format!("kimi-refresh-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
            &config_path,
            format!(
                "default_model = \"kimi-code/k3\"\n\n[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\nbase_url = \"http://{addr}/v1\"\noauth = {{ provider = \"managed:kimi-code\" }}\n"
            ),
        )
        .unwrap();

    let oauth = crate::server::oauth::OAuthManager::with_hosts(
        format!("http://{addr}"),
        format!("http://{addr}/v1"),
        Some(credentials.clone()),
    );
    let server = HttpServer::in_memory()
        .unwrap()
        .with_oauth_manager(Arc::new(oauth))
        .with_config_write_path(config_path.clone());

    fn request(path: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: path.into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    // 1. A targeted refresh discovers both models and writes the aliases.
    let res = server
        .handle_request(&request("/api/v1/providers/managed%3Akimi-code:refresh"))
        .await;
    assert_eq!(res.status, 200);
    let result: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(result["changed"][0]["provider_id"], "managed:kimi-code");
    assert_eq!(result["changed"][0]["provider_name"], "Kimi Code");
    assert_eq!(result["changed"][0]["added"], 2);
    assert_eq!(result["changed"][0]["removed"], 0);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[models.\"kimi-code/k3\"]"), "{text}");
    assert!(text.contains("[models.\"kimi-code/k3-fast\"]"), "{text}");
    assert!(text.contains("provider = \"managed:kimi-code\""), "{text}");

    // 2. The same payload again is a no-op.
    let res = server
        .handle_request(&request("/api/v1/providers/managed%3Akimi-code:refresh"))
        .await;
    let result: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(result["unchanged"], json!(["managed:kimi-code"]));
    assert!(result["changed"].as_array().unwrap().is_empty());

    // 3. A model the endpoint stops listing is reported and removed.
    *payload
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = json!({
        "data": [{ "id": "k3", "limit": { "context": 200000 } }]
    });
    let res = server
        .handle_request(&request("/api/v1/providers:refresh"))
        .await;
    let result: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(result["changed"][0]["removed"], 1);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(!text.contains("kimi-code/k3-fast"), "{text}");
    assert!(text.contains("default_model = \"kimi-code/k3\""), "{text}");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&credentials).ok();
}

#[tokio::test]
async fn test_http_files_upload_download_delete() {
    let dir = std::env::temp_dir().join(format!("kimi-files-route-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_file_store(crate::server::files::FileStore::with_root(dir.clone()));

    let boundary = "route-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"note.txt\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: text/plain\r\n\r\n");
    body.extend_from_slice(b"hello files");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/files".into(),
            query: None,
            headers: HashMap::from([(
                "content-type".to_string(),
                format!("multipart/form-data; boundary={boundary}"),
            )]),
            body,
        })
        .await;
    assert_eq!(res.status, 200);
    let meta: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(meta["name"], "note.txt");
    assert_eq!(meta["media_type"], "text/plain");
    assert_eq!(meta["size"], 11);
    let file_id = meta["id"].as_str().unwrap().to_string();

    // Download the whole file.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/files/{file_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, b"hello files");
    assert_eq!(res.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        res.header("content-disposition"),
        Some("attachment; filename=\"note.txt\"")
    );

    // A range answers 206 with the slice.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/files/{file_id}"),
            query: None,
            headers: HashMap::from([("range".to_string(), "bytes=0-4".to_string())]),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 206);
    assert_eq!(res.body, b"hello");
    assert_eq!(res.header("content-range"), Some("bytes 0-4/11"));

    // Unknown ids answer FILE_NOT_FOUND.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/files/f_missing".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 404);
    let missing: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(missing["code"], 40407);

    // Delete drops the blob; the listing is empty afterwards.
    let res = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/files/{file_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let deleted: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(deleted["deleted"], true);

    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/files".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let list: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(list["files"].as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn test_http_mcp_endpoints() {
    let server = HttpServer::in_memory().unwrap();

    // 1. Initial MCP servers list is empty
    let res_mcp = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_mcp.status, 200);
    let val_mcp: Value = serde_json::from_slice(&res_mcp.body).unwrap();
    assert_eq!(val_mcp["servers"].as_array().unwrap().len(), 0);

    let res_mcp_servers = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp/servers".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_mcp_servers.status, 200);
    let val_mcp_servers: Value = serde_json::from_slice(&res_mcp_servers.body).unwrap();
    assert_eq!(val_mcp_servers["servers"].as_array().unwrap().len(), 0);

    // 2. Add an MCP client to server's manager
    let client = crate::mcp::client::McpClient::mock("github-mcp");
    server.mcp_manager().add_client(client).await;

    let res_mcp_updated = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_mcp_updated.status, 200);
    let val_mcp_updated: Value = serde_json::from_slice(&res_mcp_updated.body).unwrap();
    let servers = val_mcp_updated["servers"].as_array().unwrap();
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0]["name"], "github-mcp");
    assert_eq!(servers[0]["status"], "connected");
    assert_eq!(servers[0]["tool_count"], 1);

    // 3. Query tools
    let res_tools = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp/tools".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_tools.status, 200);
    let val_tools: Value = serde_json::from_slice(&res_tools.body).unwrap();
    assert_eq!(val_tools["tools"].as_array().unwrap().len(), 1);

    // 4. Session MCP endpoint
    server
        .store_arc()
        .create_session("sess-mcp", Some("MCP Session"))
        .unwrap();
    let res_sess_mcp = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-mcp/mcp".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_mcp.status, 200);
    let val_sess_mcp: Value = serde_json::from_slice(&res_sess_mcp.body).unwrap();
    assert_eq!(val_sess_mcp["sessionId"], "sess-mcp");
    assert_eq!(val_sess_mcp["servers"].as_array().unwrap().len(), 1);
    // The roster is engine-global and the payload says so, rather than
    // leaving a client to assume the servers are session-scoped.
    assert_eq!(val_sess_mcp["scope"], "engine-global");

    // 4b. The session-scoped MCP views the web UI calls. Same roster, the
    // paths the client actually requests.
    let res_sess_servers = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-mcp/mcp/servers".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_servers.status, 200);
    let val_sess_servers: Value = serde_json::from_slice(&res_sess_servers.body).unwrap();
    assert_eq!(val_sess_servers["servers"].as_array().unwrap().len(), 1);

    let res_sess_detail = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-mcp/mcp/servers/github-mcp".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_detail.status, 200);
    let val_sess_detail: Value = serde_json::from_slice(&res_sess_detail.body).unwrap();
    assert_eq!(val_sess_detail["name"], "github-mcp");
    assert_eq!(val_sess_detail["tools"].as_array().unwrap().len(), 1);

    let res_sess_unknown = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-mcp/mcp/servers/nope".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_unknown.status, 404);

    let res_sess_reconnect = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-mcp/mcp/servers/github-mcp:reconnect".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    // The route reaches the manager, which refuses: the fixture added the
    // client directly, so there is no spawn recipe to reconnect from. A
    // 404 here would mean the path never matched.
    assert_eq!(res_sess_reconnect.status, 500);
    let val_sess_reconnect: Value = serde_json::from_slice(&res_sess_reconnect.body).unwrap();
    assert!(
        val_sess_reconnect["error"]
            .as_str()
            .unwrap_or_default()
            .contains("is not configured"),
        "{val_sess_reconnect}"
    );

    // 5. Session not found -> 404
    let res_sess_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing/mcp".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_none.status, 404);

    // The session-scoped views validate the session the same way.
    for path in [
        "/api/v1/sessions/sess-missing/mcp/servers",
        "/api/v1/sessions/sess-missing/mcp/servers/github-mcp",
    ] {
        let res = server
            .handle_request(&HttpRequest {
                method: "GET".into(),
                path: path.into(),
                query: None,
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await;
        assert_eq!(res.status, 404, "{path}");
    }

    // 6. Test server probe: POST /api/v1/mcp/servers:test
    let res_test = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/mcp/servers:test".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "name": "github-mcp" })).unwrap(),
        })
        .await;
    assert_eq!(res_test.status, 200);

    // 7. Inspect server: POST /api/v1/mcp/servers:inspect
    let res_inspect = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/mcp/servers:inspect".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "name": "github-mcp" })).unwrap(),
        })
        .await;
    assert_eq!(res_inspect.status, 200);
    let val_inspect: Value = serde_json::from_slice(&res_inspect.body).unwrap();
    assert!(val_inspect["tools"].as_array().is_some());

    // 8. Auth statuses: GET /api/v1/mcp/auth-statuses
    let res_auth = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/mcp/auth-statuses".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_auth.status, 200);

    // 9. Delete server: DELETE /api/v1/mcp/servers/github-mcp
    let res_del = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: "/api/v1/mcp/servers/github-mcp".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_del.status, 200);
}

#[tokio::test]
async fn test_http_fs_endpoints() {
    let server = HttpServer::in_memory().unwrap();
    let temp_dir = tempfile::tempdir().unwrap();
    let temp_path = temp_dir.path().to_string_lossy().to_string();

    // Create subdirectories and files
    let sub_dir = temp_dir.path().join("child_dir");
    std::fs::create_dir_all(&sub_dir).unwrap();
    let file_a = temp_dir.path().join("file_a.txt");
    std::fs::write(&file_a, "content a").unwrap();
    let file_b = sub_dir.join("file_b.rs");
    std::fs::write(&file_b, "content b").unwrap();

    // 1. GET /api/v1/fs:home
    let res_home = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/fs:home".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_home.status, 200);
    let val_home: Value = serde_json::from_slice(&res_home.body).unwrap();
    assert!(val_home.get("home").is_some());
    assert!(val_home["recent_roots"].is_array());

    // 2. GET /api/v1/fs:browse
    let res_browse = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/fs:browse".into(),
            query: Some(format!(
                "path={}",
                url::form_urlencoded::byte_serialize(temp_path.as_bytes()).collect::<String>()
            )),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_browse.status, 200);
    let val_browse: Value = serde_json::from_slice(&res_browse.body).unwrap();
    let entries = val_browse["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "child_dir");
    assert_eq!(entries[0]["is_dir"], true);

    // 3. GET /api/v1/fs:browse with non-existent path -> 404
    let res_browse_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/fs:browse".into(),
            query: Some("path=/path/that/definitely/does/not/exist/xyz".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_browse_none.status, 404);

    // 4. POST /api/v1/workspace/fs:search
    let res_search = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspace/fs:search".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "workspace": temp_path,
                "query": "file_b",
                "limit": 10
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_search.status, 200);
    let val_search: Value = serde_json::from_slice(&res_search.body).unwrap();
    let items = val_search["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "file_b.rs");
    assert_eq!(items[0]["kind"], "file");
    assert_eq!(val_search["truncated"], false);
}

#[tokio::test]
async fn test_http_session_action_dual_syntax_and_controls() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    // Create base session
    store
        .create_session("sess-dual", Some("Dual Syntax Test"))
        .unwrap();

    let msgs = [
        crate::turn_loop::types::LLMMessage::user("first question"),
        crate::turn_loop::types::LLMMessage::assistant("first reply"),
        crate::turn_loop::types::LLMMessage::user("second question"),
        crate::turn_loop::types::LLMMessage::assistant("second reply"),
    ];
    store
        .save_turn("sess-dual", "turn-1", 1, &msgs[..2], None, None)
        .unwrap();
    store
        .save_turn("sess-dual", "turn-2", 2, &msgs[2..], None, None)
        .unwrap();

    store
        .put_state(
            "goal",
            "sess-dual",
            &json!({ "objective": "verify dual syntax" }),
        )
        .unwrap();

    // 1. :status and /status
    let res_status_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:status".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_status_colon.status, 200);
    let status_val: Value = serde_json::from_slice(&res_status_colon.body).unwrap();
    // v2 `sessionStatusResponseSchema`: a bare status object keyed by field
    // name, with no `sessionId`.
    assert!(status_val["busy"].is_boolean());
    assert!(status_val["permission"].is_string());

    let res_status_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual/status".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_status_slash.status, 200);

    // 2. :messages and /messages
    let res_msgs_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:messages".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_msgs_colon.status, 200);
    let msgs_val: Value = serde_json::from_slice(&res_msgs_colon.body).unwrap();
    assert_eq!(msgs_val["messages"].as_array().unwrap().len(), 4);

    let res_msgs_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual/messages".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_msgs_slash.status, 200);

    // 3. :goal and /goal
    let res_goal_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:goal".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_goal_colon.status, 200);
    let goal_val: Value = serde_json::from_slice(&res_goal_colon.body).unwrap();
    // v2 answers the bare `goalSnapshotSchema`.
    assert_eq!(goal_val["objective"], "verify dual syntax");

    let res_goal_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual/goal".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_goal_slash.status, 200);

    // 4. :abort and /abort
    let res_abort_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_abort_colon.status, 200);

    // 5. :export and /export
    let res_export_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-dual:export".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_export_colon.status, 200);

    // 6. :fork
    let res_fork_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:fork".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "Colon Forked" })).unwrap(),
        })
        .await;
    assert_eq!(res_fork_colon.status, 200);
    let fork_val: Value = serde_json::from_slice(&res_fork_colon.body).unwrap();
    let new_sid = fork_val["id"].as_str().unwrap();
    let forked_history = store.load_session_history(new_sid).unwrap();
    assert_eq!(forked_history.len(), 4);

    // 7. :compact
    let res_compact_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:compact".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    // 7. :compact — a summary is mandatory now, so a server with no
    // engine refuses before touching history. Both syntaxes must carry
    // the same refusal (the dedicated 503 contract is covered by
    // test_http_compact_without_summarizer_preserves_history).
    assert_eq!(res_compact_colon.status, 503);
    let compact_val: Value = serde_json::from_slice(&res_compact_colon.body).unwrap();
    assert_eq!(compact_val["error"], "no engine configured for this server");

    // 8. :undo
    let res_undo_colon = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-dual:undo".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1 })).unwrap(),
        })
        .await;
    assert_eq!(res_undo_colon.status, 200);
    let undo_val: Value = serde_json::from_slice(&res_undo_colon.body).unwrap();
    assert_eq!(undo_val["sessionId"], "sess-dual");
}

#[tokio::test]
async fn test_http_skills_endpoints() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    let temp_dir = tempfile::tempdir().unwrap();
    let ws_root = temp_dir.path().to_string_lossy().to_string();

    let skill_dir = temp_dir
        .path()
        .join(".agents")
        .join("skills")
        .join("test-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: test-skill\ndescription: Test workspace skill\n---\n",
    )
    .unwrap();

    let ws = store.create_workspace(&ws_root, Some("Skill WS")).unwrap();
    let ws_id = ws.id;

    // 1. GET /api/v1/workspaces/:id/skills
    let res_ws = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}/skills"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_ws.status, 200);
    let val_ws: Value = serde_json::from_slice(&res_ws.body).unwrap();
    let skills_ws = val_ws["skills"].as_array().unwrap();
    assert!(skills_ws.iter().any(|s| s["name"] == "test-skill"));
    assert!(
        skills_ws
            .iter()
            .any(|s| s["name"] == "check-kimi-code-docs")
    );

    // 2. Non-existent workspace -> 404
    let res_ws_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/workspaces/ws-missing/skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_ws_none.status, 404);

    // 3. GET /api/v1/sessions/:id/skills (both :skills and /skills)
    store
        .create_session_with_workspace("sess-skill", Some("Skill Session"), Some(&ws_id))
        .unwrap();

    let res_sess_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-skill:skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_colon.status, 200);
    let val_sess: Value = serde_json::from_slice(&res_sess_colon.body).unwrap();
    assert_eq!(val_sess["sessionId"], "sess-skill");
    let skills_sess = val_sess["skills"].as_array().unwrap();
    assert!(skills_sess.iter().any(|s| s["name"] == "test-skill"));

    let res_sess_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-skill/skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_slash.status, 200);

    // 4. Non-existent session -> 404
    let res_sess_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing:skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_sess_none.status, 404);

    // 5. POST activate skill -> 200
    let res_act = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-skill/skills/test-skill:activate".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "args": "foo" })).unwrap(),
        })
        .await;
    assert_eq!(res_act.status, 200);
    let val_act: Value = serde_json::from_slice(&res_act.body).unwrap();
    assert_eq!(val_act["activated"], true);
    assert_eq!(val_act["skill_name"], "test-skill");
}

#[tokio::test]
async fn test_http_interaction_questions_and_approvals() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    store
        .create_session("sess-inter", Some("Interaction Test"))
        .unwrap();

    let inter_mgr = server.interaction_manager();

    // 1. Questions: Register and resolve
    let q_req = crate::rpc::types::AskQuestionRequest {
        question_id: "q_test_http".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_q".into(),
        background: false,
        timeout_ms: None,
        questions: vec![crate::rpc::types::AskQuestionItem {
            question: "Select mode?".into(),
            header: Some("Mode".into()),
            options: vec![
                crate::rpc::types::AskQuestionOption {
                    label: "Fast".into(),
                    description: None,
                },
                crate::rpc::types::AskQuestionOption {
                    label: "Safe".into(),
                    description: None,
                },
            ],
            multi_select: false,
        }],
    };

    let rx_q = inter_mgr.register_question("sess-inter", q_req);

    // GET questions (both /questions and :questions)
    let res_q_list_slash = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-inter/questions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_q_list_slash.status, 200);
    let val_q_list: Value = serde_json::from_slice(&res_q_list_slash.body).unwrap();
    assert_eq!(val_q_list["items"].as_array().unwrap().len(), 1);

    let res_q_list_colon = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-inter:questions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_q_list_colon.status, 200);

    // POST resolve question
    let res_q_resolve = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-inter/questions/q_test_http:resolve".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "answers": {
                    "Select mode?": "Safe"
                },
                "method": "click"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_q_resolve.status, 200);
    let resp_q = rx_q.await.unwrap();
    assert_eq!(resp_q.answers.get("Select mode?").unwrap(), "Safe");

    // 2. Questions: Dismiss
    let q_dismiss_req = crate::rpc::types::AskQuestionRequest {
        question_id: "q_dismiss_http".into(),
        turn_id: "turn-2".into(),
        tool_call_id: "call_d".into(),
        background: false,
        timeout_ms: None,
        questions: vec![],
    };
    let rx_d = inter_mgr.register_question("sess-inter", q_dismiss_req);

    let res_q_dismiss = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-inter/questions/q_dismiss_http:dismiss".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_q_dismiss.status, 200);
    let resp_d = rx_d.await.unwrap();
    assert!(resp_d.note.is_some());

    // 3. Approvals: Register and allow
    let appr_req = crate::rpc::types::PermissionCheckRequest {
        tool_name: "Bash".into(),
        session_approval_rule: None,
        tool_call_id: "call_bash_1".into(),
        turn_id: "turn-inter".into(),
        arguments: json!({ "command": "cargo build" }),
        reason: None,
    };
    let (aid, rx_a) = inter_mgr.register_approval("sess-inter", appr_req, "build project binary");

    let res_a_list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-inter/approvals".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_a_list.status, 200);
    let val_a_list: Value = serde_json::from_slice(&res_a_list.body).unwrap();
    assert_eq!(val_a_list["items"].as_array().unwrap().len(), 1);

    let res_a_resolve = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-inter/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "decision": "approved"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_a_resolve.status, 200);
    let decision_a = rx_a.await.unwrap();
    assert!(decision_a.is_allow());

    // 4. Approvals: Reject / Deny
    let appr_req_deny = crate::rpc::types::PermissionCheckRequest {
        tool_name: "Write".into(),
        session_approval_rule: None,
        tool_call_id: "call_write_1".into(),
        turn_id: "turn-inter".into(),
        arguments: json!({ "path": "/root/important.conf" }),
        reason: None,
    };
    let (aid_deny, rx_deny) =
        inter_mgr.register_approval("sess-inter", appr_req_deny, "overwrite system file");

    let res_a_deny = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-inter/approvals/{aid_deny}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "decision": "rejected",
                "feedback": "Denied by user choice"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_a_deny.status, 200);
    let decision_deny = rx_deny.await.unwrap();
    assert!(!decision_deny.is_allow());
    assert_eq!(
        decision_deny.reason.as_deref(),
        Some("Denied by user choice")
    );
}

/// An approval resolution must be explicit. A missing or unparsable body
/// must never resolve to "approved" — that would let any client able to
/// reach the route grant a dangerous tool call with an empty POST.
#[tokio::test]
async fn test_approval_resolve_is_fail_closed() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    store
        .create_session("sess-fc", Some("Fail Closed"))
        .unwrap();

    let inter_mgr = server.interaction_manager();
    let appr_req = crate::rpc::types::PermissionCheckRequest {
        tool_name: "Bash".into(),
        session_approval_rule: None,
        tool_call_id: "call_fc".into(),
        turn_id: "turn-fc".into(),
        arguments: json!({ "command": "rm -rf /" }),
        reason: None,
    };

    // Empty body must not approve.
    let (aid, mut rx) = inter_mgr.register_approval("sess-fc", appr_req.clone(), "dangerous");
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 400, "empty body must be rejected");
    assert!(
        rx.try_recv().is_err(),
        "no decision must reach the waiter on a rejected body"
    );

    // Malformed JSON must not approve.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: b"{not json".to_vec(),
        })
        .await;
    assert_eq!(res.status, 400, "malformed body must be rejected");

    // A well-formed body without `decision` must not approve either.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "feedback": "looks fine" })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 400, "missing decision must be rejected");
    assert!(
        rx.try_recv().is_err(),
        "missing decision must not resolve the approval"
    );

    // An explicit decision still works.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/sess-fc/approvals/{aid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "decision": "approved" })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    assert!(rx.try_recv().unwrap().is_allow());

    // Unparsable question resolution is rejected the same way.
    let q_req = crate::rpc::types::AskQuestionRequest {
        question_id: "q_fc".into(),
        turn_id: "turn-1".into(),
        tool_call_id: "call_q_fc".into(),
        background: false,
        timeout_ms: None,
        questions: vec![crate::rpc::types::AskQuestionItem {
            question: "Proceed?".into(),
            header: None,
            options: vec![crate::rpc::types::AskQuestionOption {
                label: "Yes".into(),
                description: None,
            }],
            multi_select: false,
        }],
    };
    let _rx_q = inter_mgr.register_question("sess-fc", q_req);
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-fc/questions/q_fc".into(),
            query: None,
            headers: HashMap::new(),
            body: b"{not json".to_vec(),
        })
        .await;
    assert_eq!(res.status, 400, "malformed question body must be rejected");
}

#[tokio::test]
async fn test_http_session_profile_and_file_history() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    store
        .create_session("sess-prof", Some("Original Title"))
        .unwrap();

    // 1. GET profile
    let res_get_prof = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-prof/profile".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_get_prof.status, 200);
    let val_get_prof: Value = serde_json::from_slice(&res_get_prof.body).unwrap();
    // v2 answers the bare session document; `agent_config` rides inside it.
    assert_eq!(val_get_prof["title"], "Original Title");
    assert!(val_get_prof["agent_config"]["model"].is_string());

    // 2. POST update profile
    let res_post_prof = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-prof:profile".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "title": "Renamed Title",
                "agent_config": {
                    "model": "kimi-v2",
                    "permission_mode": "manual"
                }
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_post_prof.status, 200);
    let val_post_prof: Value = serde_json::from_slice(&res_post_prof.body).unwrap();
    assert_eq!(val_post_prof["title"], "Renamed Title");
    assert_eq!(val_post_prof["agent_config"]["model"], "kimi-v2");
    assert_eq!(val_post_prof["agent_config"]["permission_mode"], "manual");

    // 3. Confirm persistence
    let stored = store.get_session("sess-prof").unwrap().unwrap();
    assert_eq!(stored.title.as_deref(), Some("Renamed Title"));

    // 4. File history changes (record a real change first)
    store
        .record_file_change(
            "sess-prof",
            1,
            "src/main.rs",
            None,
            Some("fn main() {\n    println!(\"hello\");\n}\n"),
        )
        .unwrap();

    let res_changes = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-prof/file-history/changes".into(),
            query: Some("turn_id=1".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_changes.status, 200);
    let val_changes: Value = serde_json::from_slice(&res_changes.body).unwrap();
    assert!(val_changes["changes"].is_array());
    assert_eq!(val_changes["enabled"], true);
    assert_eq!(val_changes["recorded"], true);
    assert_eq!(val_changes["changes"][0]["path"], "src/main.rs");
    assert_eq!(val_changes["changes"][0]["status"], "added");

    // 5. File history content
    let res_content = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-prof/file-history/content".into(),
            query: Some("turn_id=1&path=src/main.rs".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_content.status, 200);
    let val_content: Value = serde_json::from_slice(&res_content.body).unwrap();
    assert!(
        val_content["content"]
            .as_str()
            .unwrap()
            .contains("println!(\"hello\")")
    );

    // 6. Non-existent session
    let res_missing_prof = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing/profile".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_missing_prof.status, 404);

    let res_missing_changes = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing/file-history/changes".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_missing_changes.status, 404);
}

#[tokio::test]
async fn test_http_tools_endpoint() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store);

    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/tools".into(),
            query: Some("session_id=sess-test".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);

    let val: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(val["session_id"], "sess-test");
    let tools = val["tools"].as_array().expect("tools array");
    assert!(tools.len() >= 8);

    // Check for presence of core native tools
    let tool_names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default())
        .collect();
    assert!(tool_names.contains(&"Read"));
    assert!(tool_names.contains(&"Write"));
    assert!(tool_names.contains(&"Edit"));
    assert!(tool_names.contains(&"Bash"));
    assert!(tool_names.contains(&"Grep"));
    assert!(tool_names.contains(&"Glob"));

    // Validate structure of a tool descriptor
    let read_tool = tools.iter().find(|t| t["name"] == "Read").unwrap();
    assert_eq!(read_tool["source"], "builtin");
    assert_eq!(read_tool["active"], true);
    assert!(!read_tool["description"].as_str().unwrap().is_empty());
    assert!(read_tool["input_schema"].is_object());
}

#[tokio::test]
/// `fs::content` serves raw bytes by absolute path, honours a single
/// `Range` slice, and revalidates with `If-None-Match` → 304. This route
/// used to be missing entirely, so the web UI had no way to read a file
/// it had browsed to.
async fn test_http_fs_content_raw_range_and_revalidate() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("sample.txt");
    std::fs::write(&file, "0123456789").unwrap();
    let path = file.to_string_lossy().to_string();

    let get = |query: Option<String>, extra: Vec<(&str, &str)>| {
        let mut headers = HashMap::new();
        for (k, v) in extra {
            headers.insert(k.to_string(), v.to_string());
        }
        HttpRequest {
            method: "GET".into(),
            path: "/api/v1/fs::content".into(),
            query,
            headers,
            body: Vec::new(),
        }
    };

    // Full content + ETag.
    let full = server
        .handle_request(&get(Some(format!("path={path}")), vec![]))
        .await;
    assert_eq!(full.status, 200);
    assert_eq!(full.body, b"0123456789".to_vec());
    let etag = full
        .headers
        .get("ETag")
        .cloned()
        .expect("response carries an ETag");

    // A single byte range is sliced with Content-Range.
    let ranged = server
        .handle_request(&get(
            Some(format!("path={path}")),
            vec![("Range", "bytes=2-4")],
        ))
        .await;
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.body, b"234".to_vec());
    assert_eq!(
        ranged.headers.get("Content-Range").map(String::as_str),
        Some("bytes 2-4/10")
    );

    // Unchanged content revalidates to 304.
    let revalidated = server
        .handle_request(&get(
            Some(format!("path={path}")),
            vec![("If-None-Match", etag.as_str())],
        ))
        .await;
    assert_eq!(revalidated.status, 304);

    // Relative paths are rejected, directories are not served.
    let relative = server
        .handle_request(&get(Some("path=relative/file.txt".into()), vec![]))
        .await;
    assert_eq!(relative.status, 400);
    let is_dir = server
        .handle_request(&get(
            Some(format!("path={}", dir.path().to_string_lossy())),
            vec![],
        ))
        .await;
    assert_eq!(is_dir.status, 400);
}

/// Cron entries created through the REST surface survive a restart: they
/// are persisted under `("cron", "entries")` and reloaded by `run_serve`.
/// Before this, the scheduler was built empty and never ticked, so
/// schedules were lost on restart and never fired.
#[tokio::test]
async fn test_cron_entries_persist_across_restart() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    let created = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/cron".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "cron": "*/5 * * * *",
                "prompt": "standup notes",
                "recurring": true
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(created.status, 201);
    let val: Value = serde_json::from_slice(&created.body).unwrap();
    let id = val["id"].as_str().unwrap().to_string();

    // Simulate a restart: a fresh server over the same store reloads the
    // persisted entry into its scheduler.
    let restarted = HttpServer::new(store.clone());
    let entries = restarted.load_cron_entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].id, id);
    assert_eq!(entries[0].prompt, "standup notes");
    assert_eq!(entries[0].cron, "*/5 * * * *");
    assert!(entries[0].recurring);

    // Deleting through the REST surface also drops the persisted entry.
    let deleted = restarted
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/cron/{id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(deleted.status, 200);
    assert!(restarted.load_cron_entries().is_empty());
}

/// A schedule created through a session-scoped route records the session
/// it belongs to, so the daemon's tick loop has somewhere to run the fired
/// prompt. A global `/api/v1/cron` entry has none and only publishes
/// `cron.fired`.
#[tokio::test]
async fn test_session_scoped_cron_records_its_session() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    store
        .create_session("sess-cron", Some("Cron Session"))
        .unwrap();

    let scoped = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-cron/cron".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "cron": "*/5 * * * *",
                "prompt": "check the deploy"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(scoped.status, 201);

    let global = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/cron".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "cron": "0 9 * * *",
                "prompt": "morning"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(global.status, 201);

    let entries = server.load_cron_entries();
    let scoped_entry = entries
        .iter()
        .find(|e| e.prompt == "check the deploy")
        .expect("the session-scoped entry");
    assert_eq!(scoped_entry.session_id.as_deref(), Some("sess-cron"));
    let global_entry = entries
        .iter()
        .find(|e| e.prompt == "morning")
        .expect("the global entry");
    assert_eq!(global_entry.session_id, None);

    // An unknown session is still refused before anything is created.
    let missing = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-missing/cron".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "cron": "*/5 * * * *",
                "prompt": "nope"
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(missing.status, 404);
}

/// The session warnings route reports the engine's own degradations — an
/// MCP server it could not connect — instead of the unconditional `[]` it
/// used to answer, which left a user with missing MCP tools no reason.
#[tokio::test]
async fn test_session_warnings_route_reports_mcp_failures() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    store
        .create_session("sess-warn", Some("Warn Session"))
        .unwrap();

    // A healthy (empty) roster produces nothing.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-warn/warnings".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let val: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(val["warnings"].as_array().unwrap().len(), 0);

    server
        .mcp_manager()
        .configure(
            "broken",
            crate::mcp::manager::McpServerRecipe::Stdio {
                command: "definitely-not-a-real-binary-xyz".into(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
            },
            crate::mcp::manager::McpServerOptions::default(),
        )
        .await
        .expect_err("the spawn must fail");

    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-warn/warnings".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let val: Value = serde_json::from_slice(&res.body).unwrap();
    let list = val["warnings"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{val}");
    assert_eq!(list[0]["code"], "mcp.server_failed");
    assert_eq!(list[0]["severity"], "warning");
    assert!(
        list[0]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("broken"),
        "{val}"
    );

    // An unknown session is still refused.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing/warnings".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 404);
}

#[tokio::test]
async fn test_http_plugins_endpoints() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    // 1. GET /api/v1/plugins/marketplace
    let res_market = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/plugins/marketplace".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_market.status, 200);
    let val_market: Value = serde_json::from_slice(&res_market.body).unwrap();
    let entries = val_market["entries"].as_array().unwrap();
    assert!(entries.iter().any(|p| p["id"] == "kimi-webbridge"));

    // 2. GET /api/v1/plugins (initial empty)
    let res_list_empty = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list_empty.status, 200);
    let val_list_empty: Value = serde_json::from_slice(&res_list_empty.body).unwrap();
    assert_eq!(val_list_empty["plugins"].as_array().unwrap().len(), 0);

    // 3. POST /api/v1/plugins/kimi-webbridge:enable
    let res_enable = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins/kimi-webbridge:enable".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_enable.status, 200);
    let val_enable: Value = serde_json::from_slice(&res_enable.body).unwrap();
    assert_eq!(val_enable["ok"], true);
    assert_eq!(val_enable["enabled"], true);

    // 4. GET /api/v1/plugins (now 1)
    let res_list_one = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_list_one.status, 200);
    let val_list_one: Value = serde_json::from_slice(&res_list_one.body).unwrap();
    let plugins_one = val_list_one["plugins"].as_array().unwrap();
    assert_eq!(plugins_one.len(), 1);
    assert_eq!(plugins_one[0]["id"], "kimi-webbridge");
    assert_eq!(plugins_one[0]["enabled"], true);

    // 5. POST /api/v1/plugins/kimi-webbridge:disable
    let res_disable = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins/kimi-webbridge:disable".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_disable.status, 200);
    let val_disable: Value = serde_json::from_slice(&res_disable.body).unwrap();
    assert_eq!(val_disable["enabled"], false);

    // 6. GET /api/v1/workspaces/:id/plugins
    let temp_dir = tempfile::tempdir().unwrap();
    let ws = store
        .create_workspace(&temp_dir.path().to_string_lossy(), Some("Plugin WS"))
        .unwrap();
    let ws_id = ws.id;

    let res_ws_plugins = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/workspaces/{ws_id}/plugins"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_ws_plugins.status, 200);
    let val_ws_plugins: Value = serde_json::from_slice(&res_ws_plugins.body).unwrap();
    assert_eq!(val_ws_plugins["plugins"].as_array().unwrap().len(), 1);

    // 7. POST /api/v1/plugins/kimi-webbridge:remove
    let res_remove = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins/kimi-webbridge:remove".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_remove.status, 200);

    // 8. Remove non-existent -> 404
    let res_remove_again = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins/kimi-webbridge:remove".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_remove_again.status, 404);

    // 9. Install plugin via POST /api/v1/plugins
    //
    // The id must resolve against the catalog (or already be installed):
    // an unknown id is a 404, not a fabricated `installed: true` with a
    // hardcoded version.
    let res_install_unknown = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "id": "test-new-plugin" })).unwrap(),
        })
        .await;
    assert_eq!(res_install_unknown.status, 404);

    let res_install = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "id": "kimi-webbridge" })).unwrap(),
        })
        .await;
    assert_eq!(res_install.status, 200);
    let val_inst: Value = serde_json::from_slice(&res_install.body).unwrap();
    assert_eq!(val_inst["id"], "kimi-webbridge");
    assert_eq!(val_inst["enabled"], true);
    // The version comes from the catalog entry, never from a literal.
    assert_eq!(val_inst["version"], "1.11.3");

    // 10. Global skills endpoint GET /api/v1/skills
    let res_skills = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/skills".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_skills.status, 200);
    let val_skills: Value = serde_json::from_slice(&res_skills.body).unwrap();
    assert!(val_skills["skills"].is_array());

    // 11. ACP JSON-RPC endpoint POST /api/v1/acp
    let rpc_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });
    let res_acp = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/acp".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&rpc_req).unwrap(),
        })
        .await;
    assert_eq!(res_acp.status, 200);
    let val_acp: Value = serde_json::from_slice(&res_acp.body).unwrap();
    assert_eq!(val_acp["jsonrpc"], "2.0");
    // ACP handshake uses the spec shape: numeric protocolVersion plus
    // camelCase agentCapabilities (see src/acp/types.rs).
    assert_eq!(val_acp["result"]["protocolVersion"], 1);
    assert!(val_acp["result"]["agentCapabilities"].is_object());

    // 12. The bridge attaches the server engine when one is configured:
    // `session/new` must create a session, not fail with -32000.
    let hub = Arc::new(EventHub::new());
    let engine = engine_without_a_model(store.clone(), hub.clone());
    let eng_server = HttpServer::with_hub(store.clone(), hub).with_engine(engine);
    let res_new = eng_server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/acp".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session/new",
                "params": {}
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res_new.status, 200);
    let val_new: Value = serde_json::from_slice(&res_new.body).unwrap();
    assert!(
        val_new.get("error").is_none(),
        "engine-attached bridge must not auth-fail: {val_new}"
    );
    assert!(
        val_new["result"]["sessionId"].is_string(),
        "session/new must create a session: {val_new}"
    );
}

/// v2 #3963: a plugin toggle reports `plugin_toggle` with the resulting
/// enabled set, so telemetry can attribute later turns to the plugins
/// that were loaded.
#[tokio::test]
async fn test_http_plugin_toggle_emits_telemetry() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let seen: Arc<std::sync::Mutex<Vec<(String, Value)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_seen = seen.clone();
    let server = HttpServer::new(store.clone()).with_telemetry_sink(Arc::new(
        move |event: &str, payload: Value| {
            sink_seen.lock().unwrap().push((event.to_string(), payload));
        },
    ));

    let post = |path: &str| HttpRequest {
        method: "POST".into(),
        path: path.into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };

    assert_eq!(
        server
            .handle_request(&post("/api/v1/plugins/kimi-webbridge:enable"))
            .await
            .status,
        200
    );
    assert_eq!(
        server
            .handle_request(&post("/api/v1/plugins/kimi-webbridge:disable"))
            .await
            .status,
        200
    );

    let events = seen.lock().unwrap();
    let toggles: Vec<&Value> = events
        .iter()
        .filter(|(event, _)| event == "plugin_toggle")
        .map(|(_, payload)| payload)
        .collect();
    assert_eq!(toggles.len(), 2, "one event per committed toggle");
    assert_eq!(toggles[0]["plugin_id"], "kimi-webbridge");
    assert_eq!(toggles[0]["enabled"], true);
    assert_eq!(toggles[0]["enabled_plugins"], "kimi-webbridge");
    assert_eq!(toggles[1]["plugin_id"], "kimi-webbridge");
    assert_eq!(toggles[1]["enabled"], false);
    assert_eq!(
        toggles[1]["enabled_plugins"], "",
        "a known empty set serializes as the empty string"
    );
}

/// `POST /api/v1/plugins` must reach the download path for a remote source,
/// not merely record a catalog row: a URL install that cannot be fetched has
/// to fail loudly instead of reporting `installed: true` with nothing behind
/// it. The loopback URL is refused by the SSRF guard before any connection.
#[tokio::test]
async fn test_http_plugin_install_from_a_url_downloads() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let home = tempfile::tempdir().unwrap();
    let server = HttpServer::new(store.clone()).with_plugin_home(home.path().to_path_buf());

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "id": "http://127.0.0.1:9/plugin.zip" })).unwrap(),
        })
        .await;
    assert_eq!(
        res.status, 400,
        "a refused download is not a successful install"
    );
    let val: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(
        val["error"]
            .as_str()
            .unwrap_or_default()
            .contains("private"),
        "the SSRF guard must be the refusal: {val}"
    );
    assert!(
        !home.path().join("plugins").exists(),
        "a refused download must leave no managed copy"
    );
}

#[tokio::test]
async fn test_http_search_pagination_is_real() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    store.create_session("sess-p", Some("Paging")).unwrap();
    for i in 0..3 {
        let msgs = vec![crate::turn_loop::types::LLMMessage::user(format!(
            "pagination probe message {i}"
        ))];
        store
            .save_turn("sess-p", &format!("t{i}"), i + 1, &msgs, None, None)
            .unwrap();
    }
    let server = HttpServer::new(store.clone());
    let post_search = |body: Vec<u8>| HttpRequest {
        method: "POST".into(),
        path: "/api/v1/search".into(),
        query: None,
        headers: HashMap::new(),
        body,
    };

    // Page 1 of 3 with page_size 2: two items, more available, cursor set.
    let res1 = server
        .handle_request(&post_search(
            serde_json::to_vec(&json!({ "query": "pagination probe", "page_size": 2 })).unwrap(),
        ))
        .await;
    assert_eq!(res1.status, 200);
    let page1: Value = serde_json::from_slice(&res1.body).unwrap();
    assert_eq!(page1["items"].as_array().unwrap().len(), 2);
    assert_eq!(page1["has_more"], true);
    let cursor = page1["page_token"].as_str().expect("cursor must be set");

    // Page 2 follows the cursor: the remaining item, no more pages.
    let res2 = server
        .handle_request(&post_search(
            serde_json::to_vec(
                &json!({ "query": "pagination probe", "page_size": 2, "page_token": cursor }),
            )
            .unwrap(),
        ))
        .await;
    assert_eq!(res2.status, 200);
    let page2: Value = serde_json::from_slice(&res2.body).unwrap();
    assert_eq!(page2["items"].as_array().unwrap().len(), 1);
    assert_eq!(page2["has_more"], false);
    assert!(page2["page_token"].is_null());

    // A garbage cursor is a client error, not silently page one.
    let res_bad = server
        .handle_request(&post_search(
            serde_json::to_vec(&json!({ "query": "pagination probe", "page_token": "nope" }))
                .unwrap(),
        ))
        .await;
    assert_eq!(res_bad.status, 400);
}

#[tokio::test]
async fn test_http_oauth_and_media_endpoints() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let mut server = HttpServer::new(store.clone());
    // Point OAuth at a closed local port: the flow must surface an
    // honest "error" snapshot instead of a fabricated pending login.
    server.oauth_manager = Arc::new(crate::server::oauth::OAuthManager::with_hosts(
        "http://127.0.0.1:1".into(),
        "http://127.0.0.1:1".into(),
        Some(std::env::temp_dir().join(format!("kimi-oauth-srv-test-{}", fastrand::u32(..)))),
    ));

    // 1. POST /api/v1/oauth/login
    let res_login = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/oauth/login".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "provider": "kimi" })).unwrap(),
        })
        .await;
    assert_eq!(res_login.status, 200);
    let val_login: Value = serde_json::from_slice(&res_login.body).unwrap();
    // v2 has no `error` status: a failed flow is `denied`/`expired`, and the
    // reason travels in `error_message`.
    assert!(
        matches!(
            val_login["status"].as_str(),
            Some("denied" | "expired" | "cancelled")
        ),
        "a failed start reports a terminal v2 status, got {}",
        val_login["status"]
    );
    assert!(
        val_login["error_message"].is_string(),
        "the failure reason must travel"
    );

    // 2. GET /api/v1/oauth/login
    let res_poll = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/oauth/login".into(),
            query: Some("provider=kimi".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_poll.status, 200);
    let val_poll: Value = serde_json::from_slice(&res_poll.body).unwrap();
    // Same vocabulary as the start route: a failed flow is a terminal v2
    // status carrying `error_message`, never `"error"`.
    assert!(
        matches!(
            val_poll["status"].as_str(),
            Some("denied" | "expired" | "cancelled")
        ),
        "got {}",
        val_poll["status"]
    );

    // 3. GET /api/v1/oauth/usage & /api/v1/oauth/user
    let res_usage = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/oauth/usage".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_usage.status, 200);
    let val_usage: Value = serde_json::from_slice(&res_usage.body).unwrap();
    assert_eq!(val_usage["authenticated"], false, "no credentials stored");

    let res_user = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/oauth/user".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_user.status, 200);
    let val_user: Value = serde_json::from_slice(&res_user.body).unwrap();
    assert_eq!(val_user["authenticated"], false, "no credentials stored");

    // 4. DELETE /api/v1/oauth/login (cancel)
    let res_cancel = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: "/api/v1/oauth/login".into(),
            query: Some("provider=kimi".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_cancel.status, 200);
    let val_cancel: Value = serde_json::from_slice(&res_cancel.body).unwrap();
    assert_eq!(val_cancel["cancelled"], true);

    // 5. POST /api/v1/oauth/logout
    let res_logout = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/oauth/logout".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "provider": "kimi" })).unwrap(),
        })
        .await;
    assert_eq!(res_logout.status, 200);
    let val_logout: Value = serde_json::from_slice(&res_logout.body).unwrap();
    assert_eq!(val_logout["loggedOut"], true);

    // 6. Session Media: existing file
    store
        .create_session("sess-media", Some("Media Test"))
        .unwrap();

    let temp_dir = tempfile::tempdir().unwrap();
    let sample_img = temp_dir.path().join("sample.png");
    std::fs::write(&sample_img, b"\x89PNG\r\n\x1a\nfakeimage").unwrap();
    let sample_path_str = sample_img.to_string_lossy().to_string();

    let res_media = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/sess-media/media/{sample_path_str}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_media.status, 200);
    assert_eq!(res_media.header("content-type"), Some("image/png"));
    assert_eq!(res_media.body, b"\x89PNG\r\n\x1a\nfakeimage");

    // 7. Session Media: missing file / missing session -> 404
    let res_media_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-media/media/non-existent.png".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_media_none.status, 404);

    let res_media_sess_none = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-missing/media/any.png".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_media_sess_none.status, 404);
}

#[tokio::test]
async fn test_http_envelope_negotiation() {
    let server = HttpServer::in_memory().unwrap();

    // 1. Regular request without envelope -> bare JSON
    let res_bare = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/health".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_bare.status, 200);
    let val_bare: Value = serde_json::from_slice(&res_bare.body).unwrap();
    assert_eq!(val_bare["status"], "ok");
    assert!(val_bare.get("code").is_none());

    // 2. Request with X-Envelope header -> Envelope JSON
    let mut headers = HashMap::new();
    headers.insert("X-Envelope".into(), "true".into());
    headers.insert("X-Request-ID".into(), "req_client_789".into());

    let res_env = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/health".into(),
            query: None,
            headers,
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_env.status, 200);
    let val_env: Value = serde_json::from_slice(&res_env.body).unwrap();
    assert_eq!(val_env["code"], 0);
    assert_eq!(val_env["msg"], "success");
    assert_eq!(val_env["request_id"], "req_client_789");
    assert_eq!(val_env["data"]["status"], "ok");

    // 3. Request with query param ?envelope=1 -> Envelope JSON
    let res_query_env = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/health".into(),
            query: Some("envelope=1".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_query_env.status, 200);
    let val_query_env: Value = serde_json::from_slice(&res_query_env.body).unwrap();
    assert_eq!(val_query_env["code"], 0);
    assert_eq!(val_query_env["msg"], "success");
    assert!(val_query_env["request_id"].is_string());

    // 4. 404 Not Found with X-Envelope header -> Error Envelope
    let mut err_headers = HashMap::new();
    err_headers.insert("X-Envelope".into(), "true".into());
    err_headers.insert("X-Request-ID".into(), "req_err_001".into());

    let res_err_env = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/missing-id".into(),
            query: None,
            headers: err_headers,
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_err_env.status, 404);
    let val_err_env: Value = serde_json::from_slice(&res_err_env.body).unwrap();
    assert_eq!(val_err_env["code"], 40401);
    assert_eq!(val_err_env["request_id"], "req_err_001");
    assert!(val_err_env["data"].is_null());
}

#[tokio::test]
async fn test_http_lifecycle_events_broadcasting() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let mut sub = server.hub().attach();

    // 1. Create workspace -> event.workspace.created on "global"
    let ws_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspaces".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "root": "/tmp/test-lifecycle-ws",
                "name": "Test Workspace",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(ws_res.status, 201);
    let ws_val: Value = serde_json::from_slice(&ws_res.body).unwrap();
    let ws_id = ws_val["id"].as_str().unwrap().to_string();

    let ev1 = sub.recv().await.unwrap();
    assert_eq!(&*ev1.session_id, "global");
    assert_eq!(ev1.event.event_type(), "event.workspace.created");
    if let crate::events::EngineEvent::Custom(v) = &ev1.event {
        assert_eq!(v["workspace"]["root"], "/tmp/test-lifecycle-ws");
    } else {
        panic!("Expected EngineEvent::Custom for event.workspace.created");
    }

    // 2. Delete workspace -> event.workspace.deleted on "global"
    let ws_del_res = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/workspaces/{ws_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(ws_del_res.status, 200);

    let ev2 = sub.recv().await.unwrap();
    assert_eq!(&*ev2.session_id, "global");
    assert_eq!(ev2.event.event_type(), "event.workspace.deleted");
    if let crate::events::EngineEvent::Custom(v) = &ev2.event {
        assert_eq!(v["workspace_id"], ws_id);
        assert_eq!(v["root"], "/tmp/test-lifecycle-ws");
    } else {
        panic!("Expected EngineEvent::Custom for event.workspace.deleted");
    }

    // 3. Create session -> event.session.created on session_id lane
    let sess_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "title": "Lifecycle Session",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(sess_res.status, 200);
    let sess_val: Value = serde_json::from_slice(&sess_res.body).unwrap();
    let session_id = sess_val["id"].as_str().unwrap().to_string();

    let ev3 = sub.recv().await.unwrap();
    assert_eq!(&*ev3.session_id, &session_id);
    assert_eq!(ev3.event.event_type(), "event.session.created");
    if let crate::events::EngineEvent::Custom(v) = &ev3.event {
        assert_eq!(v["sessionId"], session_id);
        assert_eq!(v["session"]["title"], "Lifecycle Session");
    } else {
        panic!("Expected EngineEvent::Custom for event.session.created");
    }

    // 4. Update profile -> session.meta.updated on session_id lane
    let prof_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{session_id}:profile"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "title": "Renamed Lifecycle Session",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(prof_res.status, 200);

    let ev4 = sub.recv().await.unwrap();
    assert_eq!(&*ev4.session_id, &session_id);
    assert_eq!(ev4.event.event_type(), "session.meta.updated");
    if let crate::events::EngineEvent::Custom(v) = &ev4.event {
        assert_eq!(v["sessionId"], session_id);
        assert_eq!(v["title"], "Renamed Lifecycle Session");
    } else {
        panic!("Expected EngineEvent::Custom for session.meta.updated");
    }

    // 5. Delete session -> event.session.deleted on "global"
    let del_sess_res = server
        .handle_request(&HttpRequest {
            method: "DELETE".into(),
            path: format!("/api/v1/sessions/{session_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(del_sess_res.status, 200);

    let ev5 = sub.recv().await.unwrap();
    assert_eq!(&*ev5.session_id, "global");
    assert_eq!(ev5.event.event_type(), "event.session.deleted");
    if let crate::events::EngineEvent::Custom(v) = &ev5.event {
        assert_eq!(v["sessionId"], session_id);
    } else {
        panic!("Expected EngineEvent::Custom for event.session.deleted");
    }
}

#[tokio::test]
async fn test_http_session_fs_routes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let work_dir = temp_dir.path();
    let server = HttpServer::in_memory().unwrap();

    // 1. Create session with cwd metadata pointing to temp_dir
    let sid = "sess-fs-test";
    server
        .store
        .create_session(sid, Some("FS Test Session"))
        .unwrap();
    server
        .store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": work_dir.to_string_lossy() }),
        )
        .unwrap();

    // 2. mkdir via POST /api/v1/sessions/:id/fs:mkdir
    let mkdir_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs:mkdir"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "path": "docs", "recursive": true })).unwrap(),
        })
        .await;
    assert_eq!(mkdir_res.status, 201);

    // 3. Write a test file in work_dir/docs/readme.txt
    let file_path = work_dir.join("docs").join("readme.txt");
    std::fs::write(&file_path, "Hello from native fs test!").unwrap();

    // 4. stat via POST /api/v1/sessions/:id:fs:stat
    let stat_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:fs:stat"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "path": "docs/readme.txt" })).unwrap(),
        })
        .await;
    assert_eq!(stat_res.status, 200);
    let stat_val: Value = serde_json::from_slice(&stat_res.body).unwrap();
    assert_eq!(stat_val["name"], "readme.txt");
    assert_eq!(stat_val["size"], 26);

    // 5. list via POST /api/v1/sessions/:id/fs/list
    let list_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs/list"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "path": "docs" })).unwrap(),
        })
        .await;
    assert_eq!(list_res.status, 200);
    let list_val: Value = serde_json::from_slice(&list_res.body).unwrap();
    assert_eq!(list_val["items"].as_array().unwrap().len(), 1);

    // 6. search via POST /api/v1/sessions/:id/fs:search
    let search_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs:search"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "query": "readme" })).unwrap(),
        })
        .await;
    assert_eq!(search_res.status, 200);
    let search_val: Value = serde_json::from_slice(&search_res.body).unwrap();
    assert_eq!(search_val["items"].as_array().unwrap().len(), 1);

    // 7. download via GET /api/v1/sessions/:id/fs/docs/readme.txt:download
    let dl_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/fs/docs/readme.txt:download"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(dl_res.status, 200);
    assert_eq!(
        String::from_utf8_lossy(&dl_res.body),
        "Hello from native fs test!"
    );

    // 8. git_status via POST /api/v1/sessions/:id/fs:git_status
    let git_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs:git_status"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(git_res.status, 200);
}

#[tokio::test]
async fn test_http_session_terminal_routes() {
    let temp_dir = tempfile::tempdir().unwrap();
    let work_dir = temp_dir.path();
    let server = HttpServer::in_memory().unwrap();

    // 1. Create session
    let sid = "sess-term-test";
    server
        .store
        .create_session(sid, Some("Terminal Test Session"))
        .unwrap();
    server
        .store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": work_dir.to_string_lossy() }),
        )
        .unwrap();

    // 2. Create terminal via POST /api/v1/sessions/:id/terminals
    let create_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/terminals"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "cols": 80,
                "rows": 24
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(create_res.status, 201);
    let term_val: Value = serde_json::from_slice(&create_res.body).unwrap();
    let tid = term_val["id"].as_str().unwrap();
    assert_eq!(term_val["status"], "running");

    // 3. List terminals via GET /api/v1/sessions/:id/terminals
    let list_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/terminals"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(list_res.status, 200);
    let list_val: Value = serde_json::from_slice(&list_res.body).unwrap();
    assert_eq!(list_val["items"].as_array().unwrap().len(), 1);

    // 4. Get terminal via GET /api/v1/sessions/:id/terminals/:tid
    let get_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/terminals/{tid}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(get_res.status, 200);
    let get_val: Value = serde_json::from_slice(&get_res.body).unwrap();
    assert_eq!(get_val["id"], tid);

    // 5. Resize via POST /api/v1/sessions/:id/terminals/:tid:resize
    let resize_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/terminals/{tid}:resize"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "cols": 100, "rows": 30 })).unwrap(),
        })
        .await;
    assert_eq!(resize_res.status, 200);

    // 6. Write via POST /api/v1/sessions/:id/terminals/:tid:write
    let write_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/terminals/{tid}:write"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "data": "echo hi\n" })).unwrap(),
        })
        .await;
    assert_eq!(write_res.status, 200);

    // 7. Output via GET /api/v1/sessions/:id/terminals/:tid/output
    let out_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/terminals/{tid}/output"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(out_res.status, 200);

    // 8. Close via POST /api/v1/sessions/:id/terminals/:tid:close
    let close_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/terminals/{tid}:close"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(close_res.status, 200);
    let close_val: Value = serde_json::from_slice(&close_res.body).unwrap();
    assert_eq!(close_val["closed"], true);
}

#[tokio::test]
async fn test_http_subagents_and_snapshot_routes() {
    let server = HttpServer::in_memory().unwrap();
    let sid = "sess-sub-test";
    server
        .store
        .create_session(sid, Some("Subagent Test Session"))
        .unwrap();

    // 1. Initial list empty
    let list_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/subagents".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(list_res.status, 200);
    let list_val: Value = serde_json::from_slice(&list_res.body).unwrap();
    assert_eq!(list_val["subagents"].as_array().unwrap().len(), 0);

    // 2. Spawn subagent directly in subagent_manager
    let sub_id = server
        .subagent_manager()
        .spawn("research", "Code Analyst")
        .await
        .unwrap();

    // 3. GET /api/v1/subagents now returns 1 item
    let list_res2 = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/subagents".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(list_res2.status, 200);
    let list_val2: Value = serde_json::from_slice(&list_res2.body).unwrap();
    let subs = list_val2["subagents"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["id"], sub_id);
    assert_eq!(subs[0]["role"], "Code Analyst");
    assert_eq!(subs[0]["type_name"], "research");

    // 4. GET /api/v1/sessions/:id/subagents
    let sess_subs_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/subagents"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(sess_subs_res.status, 200);

    // 5. GET /api/v1/subagents/:id
    let get_sub_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/subagents/{sub_id}"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(get_sub_res.status, 200);
    let get_val: Value = serde_json::from_slice(&get_sub_res.body).unwrap();
    assert_eq!(get_val["id"], sub_id);
    assert_eq!(get_val["role"], "Code Analyst");

    // 6. Snapshot reflects real subagents
    let snap_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/snapshot"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(snap_res.status, 200);
    let snap_val: Value = serde_json::from_slice(&snap_res.body).unwrap();
    let snap_subs = snap_val["subagents"].as_array().unwrap();
    assert_eq!(snap_subs.len(), 1);
    assert_eq!(snap_subs[0]["id"], sub_id);
    assert_eq!(snap_subs[0]["kind"], "subagent");
    assert_eq!(snap_subs[0]["status"], "running");
    assert_eq!(snap_subs[0]["subagent_phase"], "working");
    assert_eq!(snap_subs[0]["subagent_type"], "research");
    // A plain `Agent` subagent has no swarm identity, and the keys must be
    // absent rather than null so the client's `typeof === 'number'` /
    // `typeof === 'string'` discriminators read "not a member".
    assert!(
        snap_subs[0].get("swarm_index").is_none(),
        "an ordinary subagent carries no index: {}",
        snap_subs[0]
    );
    assert!(
        snap_subs[0].get("parent_tool_call_id").is_none(),
        "an ordinary subagent has no swarm call: {}",
        snap_subs[0]
    );

    // 7. Kill subagent via POST /api/v1/subagents/:id:kill
    let kill_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/subagents/{sub_id}:kill"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(kill_res.status, 200);
    let kill_val: Value = serde_json::from_slice(&kill_res.body).unwrap();
    assert_eq!(kill_val["killed"], true);

    // 8. Snapshot reflects terminated subagent state
    let snap_res2 = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/snapshot"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(snap_res2.status, 200);
    let snap_val2: Value = serde_json::from_slice(&snap_res2.body).unwrap();
    let snap_subs2 = snap_val2["subagents"].as_array().unwrap();
    assert_eq!(snap_subs2.len(), 1);
    assert_eq!(snap_subs2[0]["status"], "failed");
    assert_eq!(snap_subs2[0]["subagent_phase"], "failed");

    // 8b. A swarm member keeps its identity across the snapshot. This is
    // the assertion the missing `swarm_index` failed: a client that
    // resumes gets `swarm_index: undefined` for every member, cannot tell
    // them from plain children, and re-renders the whole batch as ordinary
    // subagent cards in the main conversation (v2 `role: 'member'`).
    let member_id = server
        .subagent_manager()
        .spawn("research", "Swarm worker")
        .await
        .unwrap();
    server
        .subagent_manager()
        .set_parent(
            &member_id,
            "main".into(),
            Some("call_swarm".into()),
            Some("src/a.rs".into()),
            Some(2),
        )
        .await;
    let snap_res3 = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/snapshot"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(snap_res3.status, 200);
    let snap_val3: Value = serde_json::from_slice(&snap_res3.body).unwrap();
    let member = snap_val3["subagents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == member_id)
        .expect("the member is in the snapshot roster");
    assert_eq!(
        member["swarm_index"], 2,
        "the member's batch position survives: {member}"
    );
    assert_eq!(member["parent_tool_call_id"], "call_swarm");

    // 9. Capabilities check
    let meta_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/meta".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(meta_res.status, 200);
    let meta_val: Value = serde_json::from_slice(&meta_res.body).unwrap();
    assert_eq!(meta_val["capabilities"]["subagents"], true);

    let cap_res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/capabilities".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(cap_res.status, 200);
    let cap_val: Value = serde_json::from_slice(&cap_res.body).unwrap();
    let caps = cap_val["capabilities"].as_array().unwrap();
    assert!(caps.iter().any(|c| c["id"] == "subagents"));
}

#[tokio::test]
async fn test_http_session_patch_and_undo() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    // 1. Create a session
    let create_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "Initial Title" })).unwrap(),
        })
        .await;
    assert_eq!(create_res.status, 200);
    let create_val: Value = serde_json::from_slice(&create_res.body).unwrap();
    let sid = create_val["id"].as_str().unwrap();

    // 2. Patch session title and custom metadata using RFC 6902 JSON patch
    let patch_ops = json!([
        { "op": "replace", "path": "/title", "value": "Updated Title" },
        { "op": "add", "path": "/metadata/custom_tag", "value": "production" }
    ]);

    let patch_res = server
        .handle_request(&HttpRequest {
            method: "PATCH".into(),
            path: format!("/api/v1/sessions/{sid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&patch_ops).unwrap(),
        })
        .await;
    assert_eq!(patch_res.status, 200);
    let patch_val: Value = serde_json::from_slice(&patch_res.body).unwrap();
    assert_eq!(patch_val["session"]["title"], "Updated Title");
    assert_eq!(patch_val["session"]["metadata"]["custom_tag"], "production");
    assert!(patch_val["applied_patch"].is_object() || patch_val["applied_patch"].is_array());
    assert!(patch_val["undo_patch"].is_object() || patch_val["undo_patch"].is_array());

    // 3. Partial object patch via POST :patch
    let partial_update = json!({
        "title": "Object Patched Title",
        "metadata": { "client_version": "1.2.3" }
    });
    let post_patch_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:patch"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&partial_update).unwrap(),
        })
        .await;
    assert_eq!(post_patch_res.status, 200);
    let post_patch_val: Value = serde_json::from_slice(&post_patch_res.body).unwrap();
    assert_eq!(post_patch_val["session"]["title"], "Object Patched Title");
    assert_eq!(
        post_patch_val["session"]["metadata"]["client_version"],
        "1.2.3"
    );
    assert_eq!(
        post_patch_val["session"]["metadata"]["custom_tag"],
        "production"
    );

    // 4. Undo the last patch
    let undo_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo_patch"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(undo_res.status, 200);
    let undo_val: Value = serde_json::from_slice(&undo_res.body).unwrap();
    assert_eq!(undo_val["session"]["title"], "Updated Title");
}

/// `POST :undo` with `revert_files` reverts the workspace files of the
/// turns it actually removed —the file history is keyed by turn *number*,
/// so passing the undo *count* used to restore the wrong turn.
#[tokio::test]
async fn test_http_undo_reverts_files_of_the_undone_turn() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-undo-files";
    store.create_session(sid, None).unwrap();
    store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": temp.path().display().to_string() }),
        )
        .unwrap();

    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store.save_turn(sid, "t1", 1, &msgs, None, None).unwrap();
    store.save_turn(sid, "t2", 2, &msgs, None, None).unwrap();
    std::fs::write(temp.path().join("a.txt"), "after-1").unwrap();
    std::fs::write(temp.path().join("b.txt"), "after-2").unwrap();
    store
        .record_file_change(sid, 1, "a.txt", Some("before-1"), Some("after-1"))
        .unwrap();
    store
        .record_file_change(sid, 2, "b.txt", Some("before-2"), Some("after-2"))
        .unwrap();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1, "revert_files": true })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["undone"], 1);

    // Only the removed turn's file is restored.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "before-2"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "after-1"
    );
}

/// `/undo` used to delete transcript rows while leaving `todo` / `plan` on
/// disk describing work the model could no longer see. These cover the
/// checkpoint stack being unwound LIFO, and — just as important — an *empty*
/// stack staying a non-error so sessions that never checkpointed are
/// unaffected.
#[tokio::test]
async fn undo_restores_state_domains_from_the_checkpoint_stack() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-undo-state";
    store.create_session(sid, None).unwrap();
    store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": temp.path().display().to_string() }),
        )
        .unwrap();

    // Turn 1 pushes the anchor, then mutates the domain. Undo state is
    // session-scoped now (v2 agent scope), so the checkpoints live in the
    // session store.
    let state = crate::storage::StateStore::for_session(temp.path(), sid).unwrap();
    state
        .write_domain("todo", &json!([{ "id": "T1" }]))
        .unwrap();
    state.checkpoint().unwrap();
    state
        .write_domain("todo", &json!([{ "id": "T1" }, { "id": "T2" }]))
        .unwrap();

    // Turn 2 pushes a second anchor and mutates again.
    state.checkpoint().unwrap();
    state
        .write_domain(
            "todo",
            &json!([{ "id": "T1" }, { "id": "T2" }, { "id": "T3" }]),
        )
        .unwrap();
    assert_eq!(state.checkpoint_depth(), 2);

    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store.save_turn(sid, "t1", 1, &msgs, None, None).unwrap();
    store.save_turn(sid, "t2", 2, &msgs, None, None).unwrap();

    // Undoing both turns must unwind both snapshots and land on the
    // pre-turn-1 value, not merely one level up.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 2 })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["undone"], 2);

    assert_eq!(state.checkpoint_depth(), 0, "both snapshots are consumed");
    assert_eq!(
        state.read_domain("todo"),
        Some(json!([{ "id": "T1" }])),
        "LIFO unwind must reach the earliest anchor, not stop one level short"
    );
}

/// An empty checkpoint stack is normal, not a failure: sessions that never
/// went through `StateStoreCallbacks::checkpoint` must still undo cleanly.
/// Pinned because the wired-up route now touches the state store at all.
#[tokio::test]
async fn undo_succeeds_when_no_checkpoint_was_ever_taken() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-undo-no-ckpt";
    store.create_session(sid, None).unwrap();
    store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": temp.path().display().to_string() }),
        )
        .unwrap();

    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store.save_turn(sid, "t1", 1, &msgs, None, None).unwrap();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1 })).unwrap(),
        })
        .await;
    assert_eq!(
        res.status, 200,
        "an undo inside the available anchors must not require a checkpoint"
    );

    // Asking for more anchors than exist is unavailable, independent of
    // the checkpoint stack.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 3 })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 400, "insufficient undo count must be refused");
}

#[tokio::test]
async fn test_http_compact_without_summarizer_preserves_history() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let server = HttpServer::with_hub(store.clone(), hub.clone());
    let sid = "sess-compact-http";
    store.create_session(sid, None).unwrap();
    for i in 1..=12 {
        let filler = "x".repeat(400);
        store
            .save_turn(
                sid,
                &format!("t{i}"),
                i,
                &[
                    crate::turn_loop::types::LLMMessage::user(format!("u{i} {filler}")),
                    crate::turn_loop::types::LLMMessage::assistant(format!("a{i} {filler}")),
                ],
                None,
                None,
            )
            .unwrap();
    }

    let original_history = serde_json::to_value(store.load_session_history(sid).unwrap()).unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let collector = seen.clone();
    hub.bus_for(sid).subscribe(move |event| {
        if let crate::events::types::EngineEvent::Custom(value) = event {
            collector
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(value.clone());
        }
    });

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:compact"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "instruction": "keep decisions" })).unwrap(),
        })
        .await;
    // No engine is bound, so the mandatory-summary prerequisites fail
    // before any history is touched: a 503 naming the configuration
    // problem, not a mid-request 500. History is untouched and no
    // completion event is published.
    assert_eq!(res.status, 503);
    assert!(
        serde_json::from_slice::<Value>(&res.body).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("no engine"),
    );
    assert_eq!(
        serde_json::to_value(store.load_session_history(sid).unwrap()).unwrap(),
        original_history,
    );
    let events = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "compaction.completed")
    );
}

/// A host whose `llm_chat` answers with a fixed summary, so the `:compact`
/// route's summarizer call can be observed end to end.
struct SummarizingHost {
    summary: String,
}

impl crate::callbacks::HostCallbacks for SummarizingHost {
    fn llm_chat(
        &self,
        _: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        let summary = self.summary.clone();
        Box::pin(async move {
            Ok(crate::rpc::types::LlmChatResponse {
                content: summary,
                tool_calls: vec![],
                thinking: vec![],
                finish_reason: Some("stop".to_string()),
                usage: crate::rpc::types::TokenUsage::default(),
            })
        })
    }

    fn execute_tool(
        &self,
        _: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        Box::pin(async { Err("not used".into()) })
    }

    fn check_permission(
        &self,
        _: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }
}

/// `POST :compact` folds the omitted prefix into the session model's
/// written summary. The endpoint used to write the fixed placeholder even
/// with an engine attached, so the same action produced different history
/// depending on whether the client went through REST or NAPI.
#[tokio::test]
async fn the_compact_route_writes_the_llm_summary_not_the_placeholder() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let sid = "sess-compact-summary";
    store.create_session(sid, None).unwrap();
    // Give the session a workspace and seed its todo domain: the route
    // appends the rendered list to the summary (v2 `postProcessSummary`).
    let ws = tempfile::tempdir().unwrap();
    let workspace = store
        .create_workspace(&ws.path().to_string_lossy(), None)
        .unwrap();
    store
        .create_session_with_workspace(sid, None, Some(&workspace.id))
        .unwrap();
    let state = crate::storage::StateStore::for_workspace(ws.path()).unwrap();
    state
        .write_domain(
            "todo",
            &json!([{ "id": "t1", "title": "ship the fix", "status": "in_progress" }]),
        )
        .unwrap();
    for i in 1..=12 {
        let filler = "x".repeat(400);
        store
            .save_turn(
                sid,
                &format!("t{i}"),
                i,
                &[
                    crate::turn_loop::types::LLMMessage::user(format!("u{i} {filler}")),
                    crate::turn_loop::types::LLMMessage::assistant(format!("a{i} {filler}")),
                ],
                None,
                None,
            )
            .unwrap();
    }
    // A trailing user message is never a safe split point
    // (`can_split_after` refuses user messages), so the fold stops one
    // short of the end — exactly the distinction between `compactedCount`
    // (messages folded) and the pre-compact length.
    store
        .save_turn(
            sid,
            "t-tail",
            13,
            &[crate::turn_loop::types::LLMMessage::user(
                "a trailing question".to_string(),
            )],
            None,
            None,
        )
        .unwrap();
    let pre_len = store.load_session_history(sid).unwrap().len();

    // A provider-backed spec: `build_llm_for_spec` resolves it to a
    // host-proxy LLM, which is the seam the factory below answers.
    let engine = ServerEngine::new(
        crate::pipeline::PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "test-model".into(),
            providers: vec![crate::pipeline::PipelineProvider {
                name: "test".into(),
                system_prompt: "sys".into(),
                model: "test-model".into(),
                native: None,
            }],
            native_llm: None,
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: true,
            shell_path: None,
            policy_snapshot: None,
            github_token: None,
            github_base_url: None,
            subagent_timeout_ms: None,
            agent_tool_veto: None,
            tools_veto: None,
            todo_tool_veto: None,
            tower_worktree_root: None,
            tower_enabled: false,
            tool_select: false,
            sandbox_mode: None,
            sandbox_policy: None,
            caller_agent_id: None,
            session_id: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            background: crate::storage::BackgroundLimits::default(),
        },
        hub.clone(),
        store.clone(),
    );
    engine.set_host_factory(Arc::new(|_session_id| {
        Arc::new(SummarizingHost {
            summary: "The user sent twelve filler messages.".to_string(),
        })
    }));
    let server = HttpServer::with_hub(store.clone(), hub).with_engine(engine);

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:compact"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["compacted"], true);
    assert_eq!(
        body["compactedCount"].as_u64().unwrap() as usize,
        pre_len - 1,
        "compactedCount is the folded range (first kept index), not the pre-compact length"
    );

    let history = store.load_session_history(sid).unwrap();
    let summary_message = history
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the endpoint must store the model's summary, not the placeholder");
    let summary_pos = summary_message
        .content
        .find("The user sent twelve filler messages.")
        .expect("the model's summary must be stored verbatim");
    let todos_pos = summary_message
        .content
        .find("## TODO List")
        .expect("the todo list must ride after the summary");
    assert!(
        summary_pos < todos_pos,
        "v2 postProcessSummary appends the todos after the summary"
    );
    assert!(
        summary_message.content.contains("ship the fix"),
        "the appended list is the session's own todo state"
    );
}

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

#[test]
fn prompt_content_maps_uploaded_files_to_media_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let png = files
        .save("photo.png", "image/png", None, &[0x89, 0x50, 0x4e, 0x47])
        .unwrap();
    let pdf = files
        .save("doc.pdf", "application/pdf", None, b"%PDF-1.4")
        .unwrap();

    let content = vec![
        json!({ "type": "text", "text": "look at this" }),
        json!({ "type": "image", "source": { "kind": "file", "file_id": png.id } }),
        json!({
            "type": "file",
            "file_id": pdf.id,
            "name": "doc.pdf",
            "media_type": "application/pdf",
            "size": 8,
        }),
    ];
    let (prompt, blocks) = prompt_content_to_blocks(&content, &files).unwrap();
    assert_eq!(prompt, "look at this");
    assert_eq!(blocks.len(), 2);
    // The bytes stay in the store: the engine's resolver reads them once
    // it knows which model the turn runs on.
    match &blocks[0] {
        crate::rpc::types::ContentBlock::MediaRef { file_id, kind } => {
            assert_eq!(file_id, &png.id);
            assert_eq!(*kind, crate::rpc::types::MediaKind::Image);
        }
        other => panic!("expected a media reference, got {other:?}"),
    }
    match &blocks[1] {
        crate::rpc::types::ContentBlock::Text { text } => {
            assert!(text.contains("doc.pdf"));
            assert!(text.contains("application/pdf"));
        }
        other => panic!("expected text fallback, got {other:?}"),
    }
}

/// ROADMAP #3747b: an inline base64 image over the model's pixel/byte
/// budget is compressed at intake, the original persisted, and a caption
/// naming both precedes the image — the HTTP path's half of v2's
/// `resolvePromptMediaFiles` (the TUI path does it host-side).
#[test]
fn prompt_content_compresses_an_over_budget_inline_image_with_a_caption() {
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let img = image::RgbImage::from_fn(4000, 3000, |x, y| {
        image::Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8])
    });
    let mut png = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(&png);

    let content = vec![
        json!({ "type": "text", "text": "look at this" }),
        json!({
            "type": "image",
            "name": "shot.png",
            "source": { "kind": "base64", "media_type": "image/png", "data": data },
        }),
    ];
    let (prompt, blocks) = prompt_content_to_blocks(&content, &files).unwrap();
    assert_eq!(prompt, "look at this");
    assert_eq!(blocks.len(), 2, "the caption precedes the compressed image");

    match &blocks[0] {
        crate::rpc::types::ContentBlock::Text { text } => {
            assert!(
                text.starts_with(
                    "<system>Image compressed to fit model limits: original 4000x3000,"
                )
            );
            assert!(text.contains("call Read on that path"));
        }
        other => panic!("expected the compression caption, got {other:?}"),
    }
    match &blocks[1] {
        crate::rpc::types::ContentBlock::Image {
            media_type,
            data: sent,
            name,
        } => {
            assert_eq!(name.as_deref(), Some("shot.png"));
            assert!(media_type.starts_with("image/"));
            assert_ne!(sent, &data, "the sent bytes are the compressed ones");
        }
        other => panic!("expected the compressed image, got {other:?}"),
    }
}

#[test]
fn prompt_content_rejects_an_unknown_file_id() {
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let content =
        vec![json!({ "type": "image", "source": { "kind": "file", "file_id": "f_missing" } })];
    assert!(prompt_content_to_blocks(&content, &files).is_err());
}

/// `POST /api/v1/prompts` used to mint an id and answer
/// `{"status":"enqueued"}` without touching the queue. It now delegates to
/// the session submit path: missing session / unknown session / no engine
/// all refuse instead of issuing a receipt for a prompt that never runs.
#[tokio::test]
async fn collection_prompt_route_refuses_instead_of_faking_enqueue() {
    let server = HttpServer::in_memory().unwrap();

    let missing_session = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "prompt": "hi" })).unwrap(),
        })
        .await;
    assert_eq!(missing_session.status, 400, "session_id is required");
    assert!(
        !String::from_utf8_lossy(&missing_session.body).contains("enqueued"),
        "the fake enqueue receipt is still being served"
    );

    server
        .store
        .create_session("sess-collection", Some("c"))
        .unwrap();
    let unknown_session = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "session_id": "sess-does-not-exist",
                "prompt": "hi",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(unknown_session.status, 404);

    let no_engine = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "session_id": "sess-collection",
                "prompt": "hi",
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(no_engine.status, 503, "no engine attached");

    // Nothing was ever admitted to the queue by the fake path.
    assert!(
        server.prompt_queue.snapshot("sess-collection").0.is_none(),
        "a refused submit must not leave a phantom active prompt"
    );
}

/// `fs:open` / `fs:reveal` used to answer success without launching
/// anything. They now resolve the path (denying traversal) and report the
/// launcher's error rather than a fabricated `{"opened":true}`. The launch
/// itself is environment-dependent, so the assertions cover the paths that
/// are deterministic: a traversal attempt and a missing `path` field.
#[tokio::test]
async fn fs_open_routes_do_not_report_success_for_rejected_paths() {
    let server = HttpServer::in_memory().unwrap();
    let sid = "sess-fs-open";
    server.store.create_session(sid, Some("fs")).unwrap();

    let missing_field = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs:open"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(missing_field.status, 400);

    let traversal = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs:reveal"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "path": "../../etc/passwd" })).unwrap(),
        })
        .await;
    assert_ne!(
        traversal.status, 200,
        "a traversal path must not report success"
    );
    assert!(
        !String::from_utf8_lossy(&traversal.body).contains("\"revealed\":true"),
        "the fake reveal success is still being served"
    );
}

/// `fs:open-in` is wired: an unknown app id reports the valid set instead
/// of answering `{"opened":true}`.
#[tokio::test]
async fn fs_open_in_rejects_unknown_app() {
    let server = HttpServer::in_memory().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.txt"), "x").unwrap();
    let ws = server
        .store
        .create_workspace(&dir.path().to_string_lossy(), Some("open-in WS"))
        .unwrap();
    let sid = "sess-open-in";
    server
        .store
        .create_session_with_workspace(sid, Some("oi"), Some(&ws.id))
        .unwrap();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}/fs:open-in"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "path": "x.txt", "app": "photoshop" })).unwrap(),
        })
        .await;
    assert_ne!(res.status, 200, "an unknown app must not report success");
    assert!(
        String::from_utf8_lossy(&res.body).contains("Unsupported app"),
        "{}",
        String::from_utf8_lossy(&res.body)
    );
}

// A session whose turn is streaming right now answers history with
// `in_flight` — the same turn/step entity ids the live deltas carry —
// so a reconnecting client splices at the right position (upstream
// `projection.inFlight`).

// The official Web bundle's listSessionsV2/archiveSessions/restoreSessions
// contract (upstream routes/v2/sessions.ts): a domain-grouped page plus
// per-item batch results.
#[tokio::test]
async fn v2_sessions_contract_serves_the_official_web_bundle() {
    let server = HttpServer::in_memory().unwrap();
    server
        .store
        .create_workspace("G:/kimi/kimi-code", Some("kimi-code"))
        .ok();
    let ws_id = server.store.list_workspaces().unwrap_or_default()[0]
        .id
        .clone();
    server
        .store
        .create_session_with_workspace("sess-v2-a", Some("Contract"), Some(&ws_id))
        .unwrap();
    server
        .store
        .save_turn(
            "sess-v2-a",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user("hi")],
            None,
            None,
        )
        .unwrap();

    // GET /api/v2/sessions answers the v2SessionPageSchema envelope with
    // domain-grouped items.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(
        body["total"],
        body["items"].as_array().map(|i| i.len()).unwrap()
    );
    assert_eq!(body["has_more"], false);
    assert!(body["next_page_token"].is_null());
    let first = &body["items"][0];
    assert_eq!(first["id"], "sess-v2-a");
    // Title may be null for a session created without one; the bundle
    // falls back to last_prompt then the id prefix.
    assert!(first["meta"]["title"].is_null() || first["meta"]["title"].is_string());
    assert!(first["meta"]["archived"].is_boolean());
    assert!(
        ["running", "approval", "question", "failed", "idle"]
            .contains(&first["activity"]["status"].as_str().unwrap())
    );

    // view=by_workspace (the sessions sidebar's first load) returns
    // per-workspace groups: the workspace object plus the first
    // group.page_size sessions of that workspace.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: Some("view=by_workspace&group.page_size=5&meta.has_prompt=true".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let groups = body["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "one workspace holds the session");
    assert_eq!(groups[0]["workspace"]["cwd"], "G:/kimi/kimi-code");
    assert_eq!(groups[0]["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(groups[0]["total"], 1);

    // Batch archive: {ids} in, per-item results out — a missing session
    // folds into its own item instead of failing the batch.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v2/sessions:archive".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "ids": ["sess-v2-a", "sess-v2-missing"]
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["succeeded"], 1);
    assert_eq!(body["failed"], 1);
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][1]["ok"], false);

    // The archived flag is visible on the item after the batch.
    let res = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v2/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let archived = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "sess-v2-a")
        .unwrap()["meta"]["archived"]
        .clone();
    assert_eq!(archived, true);

    // Restore flips it back.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v2/sessions:restore".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "ids": ["sess-v2-a"] })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["succeeded"], 1);
}

/// Every shape here is read off the shipped `dist-web` bundle's API client;
/// each assertion fails against the response the route used to serve, which
/// is what made the corresponding Web feature unusable.
#[tokio::test]
async fn the_web_bundle_contract_holds_for_the_shapes_it_maps_directly() {
    let server = HttpServer::in_memory().unwrap();
    server
        .store
        .create_workspace("G:/kimi/kimi-code", Some("kimi-code"))
        .ok();
    let ws_id = server.store.list_workspaces().unwrap_or_default()[0]
        .id
        .clone();
    server
        .store
        .create_session_with_workspace("sess-wire", Some("Wire"), Some(&ws_id))
        .unwrap();
    server
        .store
        .save_turn(
            "sess-wire",
            "turn-1",
            1,
            &[crate::turn_loop::types::LLMMessage::user("hi")],
            None,
            None,
        )
        .unwrap();
    // The outcome of the last turn, as the engine persists it at the end of
    // one. Without this on the wire the value is only ever a live event, so
    // a client that attaches after the turn — or after a restart — never
    // learns how it ended.
    let updated_before = server
        .store
        .get_session("sess-wire")
        .unwrap()
        .unwrap()
        .updated_at;
    server
        .store
        .set_last_turn_reason("sess-wire", Some("cancelled"))
        .unwrap();

    // Every shape here is read off the shipped `dist-web` bundle's API client;
    // each assertion fails against the response the route used to serve, which
    // is what made the corresponding Web feature unusable.
    async fn get(server: &HttpServer, path: &str) -> HttpResponse {
        server
            .handle_request(&HttpRequest {
                method: "GET".into(),
                path: path.into(),
                query: None,
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await
    }

    // v2 `GET /sessions/{session_id}` answers the bare session document
    // (`sessionSchema`), which is why the mapper can read `metadata.cwd`
    // straight off the top level.
    let res = get(&server, "/api/v1/sessions/sess-wire").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["id"], "sess-wire");
    // `format_wire_session` falls back to the process cwd when the session
    // never recorded one; the contract being pinned is that `metadata.cwd`
    // is a string at the top level, which is what the mapper dereferences.
    assert!(
        body["metadata"]["cwd"].is_string(),
        "the mapper reads `e.metadata.cwd` unconditionally"
    );
    assert!(body["agent_config"]["model"].is_string());
    assert!(
        body.get("messages").is_none(),
        "messages live on `/messages`, not on the session document"
    );
    // The persisted turn outcome, read back by a client that did not see the
    // live event. `null` is the honest answer for a session whose turn never
    // ended, which is why the value is asserted rather than its presence.
    assert_eq!(
        body["last_turn_reason"], "cancelled",
        "the wire must carry the persisted outcome: {body}"
    );
    // v2 passes `touchUpdatedAt: false` for this write, because the mirror
    // records a turn that already ended elsewhere; treating it as activity
    // would reorder the session list on every turn end.
    let updated_after = server
        .store
        .get_session("sess-wire")
        .unwrap()
        .unwrap()
        .updated_at;
    assert_eq!(
        updated_after, updated_before,
        "the mirror must not read as activity"
    );

    // v2 `listMessagesResponseSchema` is `{items, has_more}`.
    let res = get(&server, "/api/v1/sessions/sess-wire/messages").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(body.get("items").is_some() || body.get("messages").is_some());

    // v2 `listTasks` answers `pageResponseSchema(taskSchema)`: `{items}`,
    // every element carrying the protocol field names including
    // `run_in_background`.
    let res = get(&server, "/api/v1/sessions/sess-wire/tasks").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let items = body["items"].as_array().expect("`items` key");
    assert!(items.is_empty(), "no task was spawned in this test");

    // v2 `getSessionGoalResponseSchema = goalSnapshotSchema.nullable()`, so
    // an inactive goal is `null` — never a wrapper object.
    let res = get(&server, "/api/v1/sessions/sess-wire/goal").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(
        body.is_null(),
        "no active goal reads as null, matching goalSnapshotSchema.nullable()"
    );

    // `getSessionStatus` reads the session's real profile; these used to be
    // the literals "auto" / "medium" with five fields absent entirely.
    let res = get(&server, "/api/v1/sessions/sess-wire/status").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    for key in [
        "permission",
        "thinking_level",
        "plan_mode",
        "swarm_mode",
        "tower_mode",
        "max_context_tokens",
        "context_usage",
    ] {
        assert!(body.get(key).is_some(), "status field `{key}` is served");
    }
    assert!(body["max_context_tokens"].as_u64().unwrap() > 0);

    // `listCapabilities` filters on `supported`, so the elements must be
    // objects — a bare id array made every row disappear.
    let res = get(&server, "/api/v1/capabilities").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    let caps = body["capabilities"].as_array().unwrap();
    assert!(!caps.is_empty());
    assert!(caps.iter().all(|c| c["supported"] == true));
    assert!(caps.iter().all(|c| c["id"].is_string()));

    // `pollOAuthLogin` reads snake_case and treats `authenticated` as the
    // terminal success; no flow is running, so the body is null.
    let res = get(&server, "/api/v1/oauth/login").await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(body.is_null(), "no live flow reads as null");

    // The bundle's `installPlugin` posts `{ source }`, which used to 400 as
    // "Missing plugin id" because only `id` / `name` were read.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/plugins".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "source": "definitely-not-a-plugin" })).unwrap(),
        })
        .await;
    assert_ne!(
        res.status, 400,
        "a `source`-only body must reach the installer, not be rejected as malformed"
    );

    // The bundle spells the suggest route with a single colon; only the
    // `search` sibling accepted that form, so every file-mention request
    // paid a 404 round-trip first.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/workspace/fs:suggest".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_ne!(res.status, 404, "single-colon fs:suggest is routed");
}

/// The engine's export must describe itself the way the host's exporter
/// does: a manifest head naming the session, and log members only when they
/// were asked for.
#[test]
fn session_export_archive_names_itself_and_gates_the_log_members() {
    use std::io::Read as _;

    let export = crate::session::sqlite_store::SessionExport {
        session: crate::session::sqlite_store::SessionSummary {
            session_id: "sess-manifest".into(),
            title: Some("T".into()),
            created_at: 1,
            updated_at: 2,
            workspace_id: None,
            archived: false,
            parent_session_id: None,
        },
        messages: Vec::new(),
        turns_count: 0,
        exported_at: "2026-10-03T00:00:00Z".into(),
    };

    let names_of = |bytes: Vec<u8>| -> (Vec<String>, Value) {
        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("archive readable");
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        let mut manifest = Vec::new();
        archive
            .by_name("export-manifest.json")
            .expect("manifest present")
            .read_to_end(&mut manifest)
            .unwrap();
        (names, serde_json::from_slice(&manifest).unwrap())
    };

    // Default: the manifest rides along, no logs, and the key that names the
    // primary log is absent rather than null — the host's e2e pins that
    // absence, so a null here would be a different document.
    let (names, manifest) = names_of(build_session_export_zip_with(&export, Vec::new()).unwrap());
    assert_eq!(
        names,
        vec!["export-manifest.json", "session.json", "transcript.md"]
    );
    assert_eq!(manifest["sessionId"], "sess-manifest");
    assert_eq!(manifest["exportedAt"], "2026-10-03T00:00:00Z");
    assert_eq!(manifest["kimiCodeVersion"], env!("CARGO_PKG_VERSION"));
    assert_eq!(manifest["os"], std::env::consts::OS);
    assert!(
        manifest.get("globalLogPath").is_none(),
        "omitted, not written as null"
    );

    // With logs resolved they ride along after the session members, and the
    // manifest names the un-suffixed one as the primary entry.
    let (names, manifest) = names_of(
        build_session_export_zip_with(
            &export,
            vec![
                ("logs/kimi-code.log".into(), b"new".to_vec()),
                ("logs/kimi-code.log.1".into(), b"old".to_vec()),
            ],
        )
        .unwrap(),
    );
    assert_eq!(
        names,
        vec![
            "export-manifest.json",
            "session.json",
            "transcript.md",
            "logs/kimi-code.log",
            "logs/kimi-code.log.1"
        ]
    );
    assert_eq!(manifest["globalLogPath"], "logs/kimi-code.log");
}

/// The log selection and member naming have to match the host exporter and
/// `apps/vis`: `kimi-code.log` plus its `.N` rotations, nothing else.
#[test]
fn global_log_members_take_the_rotations_and_nothing_else() {
    let dir = std::env::temp_dir().join(format!(
        "kimi-log-members-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("kimi-code.log"), b"newest").unwrap();
    std::fs::write(dir.join("kimi-code.log.1"), b"older").unwrap();
    std::fs::write(dir.join("kimi-code.log.2"), b"oldest").unwrap();
    std::fs::write(dir.join("kimi-code-desktop.log"), b"other").unwrap();
    std::fs::create_dir(dir.join("kimi-code.log.dir")).unwrap();

    let members = global_log_members_in(&dir);
    let names: Vec<&str> = members.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "logs/kimi-code.log",
            "logs/kimi-code.log.1",
            "logs/kimi-code.log.2"
        ],
        "sorted, logs/-prefixed, and neither the desktop log nor a directory"
    );
    assert_eq!(members[0].1, b"newest".to_vec());

    std::fs::remove_dir_all(&dir).unwrap();
}
