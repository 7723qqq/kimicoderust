use super::super::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_meta_and_config_endpoints() {
    let server = HttpServer::in_memory().unwrap();

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

    assert!(caps.iter().any(|c| c["id"] == "task_runner"));
    assert!(caps.iter().any(|c| c["id"] == "cron_scheduler"));
    assert!(caps.iter().any(|c| c["id"] == "gui_store"));
    assert!(
        caps.iter().all(|c| c["supported"] == true),
        "every built-in capability is compiled in"
    );

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

#[tokio::test]
async fn a_shutting_down_server_refuses_new_turns() {
    let server = HttpServer::in_memory().unwrap();
    server.request_shutdown();
    for path in ["/api/v1/sessions/s1/prompt", "/api/v1/sessions/s1/prompts"] {
        let res = server
            .handle_request(&HttpRequest {
                method: "POST".into(),
                path: path.into(),
                query: None,
                headers: HashMap::new(),
                body: Vec::new(),
            })
            .await;
        assert_eq!(res.status, 503, "{path}");
        let val: Value = serde_json::from_slice(&res.body).unwrap();
        assert!(
            val["error"].as_str().unwrap().contains("shutting down"),
            "{path}: {val}"
        );
    }
}

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

    let res = fork_with(json!({ "turnIndex": "one" })).await;
    assert_eq!(res.status, 400, "a string turnIndex must be refused");

    let res = fork_with(json!({ "turnIndex": -1 })).await;
    assert_eq!(res.status, 400);

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

    let res_fork = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-test/fork".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "Forked Branch" })).unwrap(),
        })
        .await;

    assert_eq!(res_fork.status, 200);
    let val_fork: Value = serde_json::from_slice(&res_fork.body).unwrap();
    let new_sid = val_fork["id"].as_str().unwrap();
    assert_eq!(val_fork["title"], "Forked Branch");

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
