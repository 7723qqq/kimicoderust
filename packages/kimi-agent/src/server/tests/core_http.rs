use super::super::*;
use super::common::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_health_endpoint() {
    let server = HttpServer::in_memory().unwrap();
    let req = HttpRequest {
        method: "GET".into(),
        path: "/api/v1/healthz".into(),
        query: None,
        headers: HashMap::new(),
        body: Vec::new(),
    };
    let res = server.handle_request(&req).await;
    assert_eq!(res.status, 200);
    let val: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(val, json!({ "ok": true }));
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

/// v2 #4568: `PATCH /sessions/{id}/goal` edits the current objective in
/// place, without resetting counters or the goal id, and refuses an empty
/// objective or an unknown session.
#[tokio::test]
async fn patch_session_goal_edits_the_objective_in_place() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let dir = tempfile::tempdir().unwrap();
    let workspace = store
        .create_workspace(dir.path().to_str().unwrap(), None)
        .unwrap();
    store
        .create_session_with_workspace("sess-goal-edit", None, Some(&workspace.id))
        .unwrap();
    let server = HttpServer::with_hub(store.clone(), hub.clone())
        .with_engine(engine_without_a_model(store.clone(), hub));
    let session_store =
        crate::storage::StateStore::for_session(dir.path(), "sess-goal-edit").unwrap();
    for write in [
        json!({ "action": "create", "objective": "old objective" }),
        json!({ "action": "record_usage", "turns": 2 }),
    ] {
        let outcome = session_store.apply_write("goal", &write).unwrap();
        session_store.write_domain("goal", &outcome.stored).unwrap();
    }

    let patch = |objective: &str, session: &str| HttpRequest {
        method: "PATCH".into(),
        path: format!("/api/v1/sessions/{session}/goal"),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&json!({ "objective": objective })).unwrap(),
    };
    let res = server
        .handle_request(&patch("new objective", "sess-goal-edit"))
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["objective"], "new objective");
    assert_eq!(body["status"], "active");
    assert_eq!(body["turnsUsed"], 2, "editing must not reset counters");

    let empty = server.handle_request(&patch("   ", "sess-goal-edit")).await;
    assert_eq!(empty.status, 400);
    let unknown = server.handle_request(&patch("x", "sess-missing")).await;
    assert_eq!(unknown.status, 404);
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
