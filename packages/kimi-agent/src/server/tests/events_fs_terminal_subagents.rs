use super::super::*;
use std::collections::HashMap;

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
