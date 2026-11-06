use super::super::*;
use std::collections::HashMap;

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
