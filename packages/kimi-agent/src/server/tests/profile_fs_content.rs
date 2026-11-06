use super::super::*;
use std::collections::HashMap;

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
