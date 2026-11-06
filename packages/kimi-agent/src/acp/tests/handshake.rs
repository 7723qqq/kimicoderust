use super::super::*;

#[tokio::test]
async fn test_acp_initialize() {
    let server = AcpServer::in_memory().unwrap();
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": { "fs": { "readTextFile": true } },
            "clientInfo": { "name": "zed", "version": "1.0.0" }
        }
    });

    let resp = server.handle_message(&req.to_string()).await.unwrap();
    assert_eq!(resp.id, Some(json!(1)));
    assert!(resp.error.is_none());
    let res = resp.result.unwrap();
    // ACP spec shape: numeric protocolVersion + camelCase capabilities.
    assert_eq!(res["protocolVersion"], 1);
    assert!(res.get("protocol_version").is_none());
    assert_eq!(res["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        res["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );
    assert_eq!(
        res["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
    assert!(res["agentCapabilities"]["sessionCapabilities"]["list"].is_object());
    assert!(res["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
    assert!(res["agentCapabilities"]["sessionCapabilities"]["fork"].is_object());
    assert_eq!(res["agentCapabilities"]["mcpCapabilities"]["http"], true);
    assert_eq!(res["authMethods"][0]["id"], "login");
    assert_eq!(res["authMethods"][0]["type"], "terminal");
    assert_eq!(res["authMethods"][0]["args"], json!(["--login"]));
    assert_eq!(res["agentInfo"]["name"], "Kimi Code CLI");
}

/// A client below the minimum revision still receives the server's current
/// one (v2 `negotiateVersion`, acp-server/src/version.ts:38-50).
#[tokio::test]
async fn test_acp_initialize_negotiates_version() {
    let server = AcpServer::in_memory().unwrap();
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": 0 }
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    assert_eq!(resp.result.unwrap()["protocolVersion"], 1);
}

/// Notifications carry no id and must never be answered
/// (v2 registers `session/cancel` as `onNotification`, server.ts:717).
#[tokio::test]
async fn test_acp_notification_is_not_answered() {
    let server = AcpServer::in_memory().unwrap();
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": "sess-1" }
    });
    assert!(
        server
            .handle_message(&notification.to_string())
            .await
            .is_none()
    );

    // Unknown notifications are dropped silently too.
    let unknown = json!({
        "jsonrpc": "2.0",
        "method": "no/such/notification",
        "params": {}
    });
    assert!(server.handle_message(&unknown.to_string()).await.is_none());

    // The same cancel sent as a request still gets an answer.
    let request = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "session/cancel",
        "params": { "sessionId": "sess-1" }
    });
    let resp = server.handle_message(&request.to_string()).await.unwrap();
    assert_eq!(resp.id, Some(json!(9)));
}
