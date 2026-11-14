use super::super::*;
use super::common::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_plugins_endpoints() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

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

    assert_eq!(val_inst["version"], "1.11.3");

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

    assert_eq!(val_acp["result"]["protocolVersion"], 1);
    assert!(val_acp["result"]["agentCapabilities"].is_object());

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

    server.oauth_manager = Arc::new(crate::server::oauth::OAuthManager::with_hosts(
        "http://127.0.0.1:1".into(),
        "http://127.0.0.1:1".into(),
        Some(std::env::temp_dir().join(format!("kimi-oauth-srv-test-{}", fastrand::u32(..)))),
    ));

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

    assert!(
        matches!(
            val_poll["status"].as_str(),
            Some("denied" | "expired" | "cancelled")
        ),
        "got {}",
        val_poll["status"]
    );

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

    assert_eq!(val_logout["logged_out"], true);
    assert_eq!(val_logout["provider"], "kimi");

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
