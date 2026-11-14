use super::super::*;
use std::collections::HashMap;

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

    runner
        .spawn_task("task-1".into(), "test task".into(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            "task output".into()
        })
        .unwrap();

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

    let tasks = val_list["items"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["id"], "task-1");
    assert_eq!(tasks[0]["taskId"], "task-1", "the legacy key survives too");

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

    server.store.create_session("sess-task", None).unwrap();
    runner
        .spawn_task("task-cancel".into(), "cancel task".into(), async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            "ok".into()
        })
        .unwrap();

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
                    timeout_ms: None,
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

        let fired = |ty: &str| events.lock().unwrap().iter().any(|e| e["type"] == ty);
        let terminated = fired("task.terminated");
        assert_eq!(runner.pending_notification_count(Some("sess-live")), 1);
        (fired("task.started"), terminated)
    };
    assert!(created_live && terminated_live);

    runner
        .spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: Some("sess-gone"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
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

    assert_eq!(runner.entry("task-gone").unwrap()["status"], "completed");
    assert_eq!(runner.pending_notification_count(Some("sess-gone")), 0);
    assert!(
        runner
            .take_pending_notifications(Some("sess-gone"))
            .is_empty()
    );
}

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

    let restarted = HttpServer::new(store.clone());
    let entries = restarted.load_cron_entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].id, id);
    assert_eq!(entries[0].prompt, "standup notes");
    assert_eq!(entries[0].cron, "*/5 * * * *");
    assert!(entries[0].recurring);

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

#[tokio::test]
async fn test_session_warnings_route_reports_mcp_failures() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    store
        .create_session("sess-warn", Some("Warn Session"))
        .unwrap();

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
