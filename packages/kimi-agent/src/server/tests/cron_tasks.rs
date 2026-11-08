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
    // The entry still settled and stays queryable on the tasks surface.
    assert_eq!(runner.entry("task-gone").unwrap()["status"], "completed");
    assert_eq!(runner.pending_notification_count(Some("sess-gone")), 0);
    assert!(
        runner
            .take_pending_notifications(Some("sess-gone"))
            .is_empty()
    );
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
