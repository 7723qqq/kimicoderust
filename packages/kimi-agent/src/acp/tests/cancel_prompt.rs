use super::super::*;

/// A second `session/prompt` for a session that already has one running is
/// refused instead of racing the first turn's cancel token
/// (v2 `assertNoActiveTurn`).
#[tokio::test]
async fn test_acp_second_prompt_for_a_busy_session_is_refused() {
    let server = AcpServer::in_memory().unwrap();
    assert!(server.begin_prompt("7", "sess-1"));
    assert!(
        !server.begin_prompt("8", "sess-1"),
        "one turn per session at a time"
    );
    assert!(
        server.begin_prompt("9", "sess-2"),
        "a different session is unaffected"
    );
    server.end_prompt("7");
    assert!(server.begin_prompt("10", "sess-1"));
}

/// `$/cancel_request` names the request to abort; the in-flight prompt's
/// session is what actually gets cancelled, and the notification itself is
/// never answered.
#[tokio::test]
async fn test_acp_cancel_request_targets_the_named_prompt() {
    let server = AcpServer::in_memory().unwrap();
    assert!(server.begin_prompt("7", "sess-1"));

    let cancel = json!({
        "jsonrpc": "2.0",
        "method": "$/cancel_request",
        "params": { "requestId": 7 }
    });
    assert!(
        server.handle_message(&cancel.to_string()).await.is_none(),
        "a notification must never be answered"
    );

    // The same message as a request answers, and an unknown request id is
    // reported as not cancelled rather than as a method-not-found.
    let unknown = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "$/cancel_request",
        "params": { "requestId": "no-such-request" }
    });
    let resp = server.handle_message(&unknown.to_string()).await.unwrap();
    assert_eq!(resp.result.unwrap()["cancelled"], false);
}

/// The turn's cancel token exists exactly while the prompt is in flight, and
/// `$/cancel_request` fires it: the permission/question bridge selects on this
/// token, so cancelling stops a client prompt nobody is going to answer.
#[tokio::test]
async fn test_acp_cancel_fires_the_in_flight_prompt_token() {
    let server = AcpServer::in_memory().unwrap();
    assert!(server.begin_prompt("7", "sess-1"));
    let token = server
        .prompt_cancels
        .lock()
        .unwrap()
        .get("sess-1")
        .cloned()
        .expect("a running turn owns a cancel token");
    assert!(!token.is_cancelled());

    let cancel = json!({
        "jsonrpc": "2.0",
        "method": "$/cancel_request",
        "params": { "requestId": 7 }
    });
    assert!(server.handle_message(&cancel.to_string()).await.is_none());
    assert!(token.is_cancelled(), "the bridge selects on this token");

    server.end_prompt("7");
    assert!(
        server.prompt_cancels.lock().unwrap().is_empty(),
        "the token must not outlive the turn"
    );
}
