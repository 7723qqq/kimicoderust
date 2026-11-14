use super::super::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_envelope_negotiation() {
    let server = HttpServer::in_memory().unwrap();

    let res_bare = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/healthz".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(res_bare.status, 200);
    let val_bare: Value = serde_json::from_slice(&res_bare.body).unwrap();
    assert_eq!(val_bare["ok"], true);
    assert!(val_bare.get("code").is_none());

    let mut headers = HashMap::new();
    headers.insert("X-Envelope".into(), "true".into());
    headers.insert("X-Request-ID".into(), "req_client_789".into());

    let res_env = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/healthz".into(),
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
    assert_eq!(val_env["data"]["ok"], true);

    let res_query_env = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/healthz".into(),
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

    let file_path = work_dir.join("docs").join("readme.txt");
    std::fs::write(&file_path, "Hello from native fs test!").unwrap();

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

    let sub_id = server
        .subagent_manager()
        .spawn("research", "Code Analyst")
        .await
        .unwrap();

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
