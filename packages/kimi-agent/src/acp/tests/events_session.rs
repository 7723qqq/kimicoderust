use super::super::*;
use super::common::*;

/// The forwarding seam must carry the *raw* event shapes the turn loop
/// publishes, not just the typed variants. `run_turn` emits `llm.delta`
/// with no turn id and tool events that use `tool_name` + a string
/// `turn_id`; those are the payloads a live turn puts on the bus.
#[tokio::test]
async fn test_raw_turn_events_are_forwarded_to_the_client() {
    let server = AcpServer::in_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let bus = Arc::new(EventBus::new());
    let _subscription = server.forward_session_events("sess-1", &bus);

    bus.publish_json(json!({
        "type": "llm.delta",
        "part": { "type": "text", "text": "hi" },
    }));
    bus.publish_json(json!({
        "type": "tool.call.started",
        "turn_id": "turn-1",
        "tool_call_id": "call_1",
        "tool_name": "Read",
        "args": { "path": "a.txt" },
    }));
    bus.publish_json(json!({
        "type": "tool.native",
        "turn_id": "turn-1",
        "tool_call_id": "call_1",
        "tool_name": "Read",
        "arguments": { "path": "a.txt" },
        "content": "file body",
        "is_error": false,
        "note": "<system>1 line read.</system>",
    }));

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    assert_eq!(
        updates.len(),
        3,
        "every raw turn event must produce a session/update: {updates:?}"
    );
    assert_eq!(updates[0]["update"]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(updates[0]["update"]["content"]["text"], "hi");
    assert_eq!(updates[1]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(updates[1]["update"]["toolCallId"], "turn-1:call_1");
    assert_eq!(updates[2]["update"]["sessionUpdate"], "tool_call_update");
    assert_eq!(updates[2]["update"]["status"], "completed");
    assert_eq!(
        updates[2]["update"]["content"][0]["content"]["text"],
        "file body"
    );
    assert_eq!(
        updates[2]["update"]["note"], "<system>1 line read.</system>",
        "the model-facing note must reach the client on the terminal update"
    );
}

/// Engine events reach the client as `session/update` notifications while
/// a turn runs, and stop once the subscription is dropped.
#[tokio::test]
async fn test_session_update_notifications_are_forwarded() {
    use crate::events::types::EngineEvent;

    let server = AcpServer::in_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let bus = Arc::new(EventBus::new());
    let subscription = server.forward_session_events("sess-1", &bus);

    bus.publish(&EngineEvent::AssistantDelta {
        agent_id: "main".into(),
        turn_id: "turn-3".into(),
        delta: "hi".into(),
        block: None,
    });
    bus.publish(&EngineEvent::TurnStarted {
        agent_id: "main".into(),
        turn_id: "turn-3".into(),
        prompt: None,
    });

    match rx.try_recv().expect("one notification") {
        AcpOutbound::Notification(note) => {
            assert_eq!(note.method, "session/update");
            let params = note.params.expect("notification params");
            assert_eq!(params["sessionId"], "sess-1");
            assert_eq!(params["update"]["sessionUpdate"], "agent_message_chunk");
            assert_eq!(params["update"]["content"]["text"], "hi");
        }
        other => panic!("unexpected outbound message: {other:?}"),
    }
    assert!(rx.try_recv().is_err(), "unmapped events must not notify");

    bus.unsubscribe(subscription);
    bus.publish(&EngineEvent::AssistantDelta {
        agent_id: "main".into(),
        turn_id: "turn-3".into(),
        delta: "again".into(),
        block: None,
    });
    assert!(
        rx.try_recv().is_err(),
        "a dropped subscription stops forwarding"
    );
}

#[tokio::test]
async fn test_acp_session_flow() {
    let server = AcpServer::in_memory().unwrap();

    // 1. Create session
    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "title": "ACP Test Session" }
    });
    let resp = server.handle_message(&new_req.to_string()).await.unwrap();
    let sid = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. List sessions
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/list",
    });
    let resp = server.handle_message(&list_req.to_string()).await.unwrap();
    let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], sid);
    assert_eq!(sessions[0]["cwd"], "");
    assert_eq!(sessions[0]["title"], "ACP Test Session");
    assert!(sessions[0]["updatedAt"].is_string());

    // 3. Prompt session
    let prompt_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/prompt",
        "params": {
            "sessionId": sid,
            "prompt": "Hello ACP"
        }
    });
    let resp = server
        .handle_message(&prompt_req.to_string())
        .await
        .unwrap();
    // No engine is wired in this test, so the server refuses the prompt
    // instead of inventing an assistant reply (which it also used to persist
    // into the session's history).
    let err = resp
        .error
        .expect("a prompt with no engine wired must be refused, not faked");
    assert_eq!(err.code, -32000);
    assert!(
        err.message.contains("No LLM engine is available"),
        "unexpected error: {}",
        err.message
    );

    // 4. Load session history — replayed as `session/update` chunks, the
    // response carries the mode state (v2 `loadSession`).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);
    let load_req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/load",
        "params": { "sessionId": sid }
    });
    let resp = server.handle_message(&load_req.to_string()).await.unwrap();
    assert!(resp.error.is_none());
    assert_eq!(resp.result.unwrap()["modes"]["currentModeId"], "default");
    let mut replayed = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            replayed.push(note.params.unwrap());
        }
    }
    let replayed = drop_attach_updates(replayed);
    // Nothing is replayed: the refused prompt persists no turn at all, where
    // it used to leave a fabricated user+assistant pair in the history.
    assert_eq!(
        replayed.len(),
        0,
        "a refused prompt must not fabricate a turn"
    );

    // 5. Set mode (ACP wire name `modeId`; the legacy `mode` still parses)
    let mode_req = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "session/set_mode",
        "params": { "sessionId": sid, "modeId": "yolo" }
    });
    let resp = server.handle_message(&mode_req.to_string()).await.unwrap();
    assert_eq!(resp.result.unwrap()["modeId"], "yolo");

    // 6. Delete session
    let del_req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "session/delete",
        "params": { "sessionId": sid }
    });
    let resp = server.handle_message(&del_req.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp.result.unwrap(), json!({}));

    // 7. Deleting an unknown session is invalid params, not a false answer.
    let del_missing = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "session/delete",
        "params": { "sessionId": "sess-nope" }
    });
    let resp = server
        .handle_message(&del_missing.to_string())
        .await
        .unwrap();
    assert_eq!(resp.error.unwrap().message, "Unknown sessionId: sess-nope");
}

/// A stored tool result replays as `tool_call_update` (v2 `replay.ts`).
#[tokio::test]
async fn test_acp_load_replays_tool_results() {
    let server = AcpServer::in_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);
    server.store.create_session("sess-tools", None).unwrap();
    let messages = vec![
        crate::turn_loop::types::LLMMessage::user("run it"),
        crate::turn_loop::types::LLMMessage {
            role: "assistant".into(),
            content: "on it".into(),
            tool_calls: vec![crate::turn_loop::types::ToolCall {
                id: "call-1".into(),
                name: "Read".into(),
                arguments: json!({ "path": "a.txt" }),
                extras: None,
                args_parse_error: None,
            }],
            ..Default::default()
        },
        crate::turn_loop::types::LLMMessage {
            role: "tool".into(),
            content: "tool output".into(),
            tool_call_id: Some("call-1".into()),
            ..Default::default()
        },
    ];
    server
        .store
        .save_turn("sess-tools", "t1", 1, &messages, None, None)
        .unwrap();

    let load = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/load",
        "params": { "sessionId": "sess-tools" }
    });
    server.handle_message(&load.to_string()).await.unwrap();

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap());
        }
    }
    let updates = drop_attach_updates(updates);
    // user chunk, assistant chunk, tool_call create, tool_call_update.
    assert_eq!(updates.len(), 4, "unexpected replay: {updates:?}");
    assert_eq!(updates[0]["update"]["sessionUpdate"], "user_message_chunk");
    assert_eq!(updates[1]["update"]["sessionUpdate"], "agent_message_chunk");
    // The create carries the turn-namespaced id and the parsed args.
    assert_eq!(updates[2]["update"]["sessionUpdate"], "tool_call");
    assert_eq!(updates[2]["update"]["toolCallId"], "t1:call-1");
    assert_eq!(updates[2]["update"]["title"], "Read");
    assert_eq!(updates[2]["update"]["kind"], "read");
    assert_eq!(updates[2]["update"]["status"], "in_progress");
    assert_eq!(updates[2]["update"]["rawInput"]["path"], "a.txt");
    // The terminal lands on the same wire id the create opened.
    assert_eq!(updates[3]["update"]["sessionUpdate"], "tool_call_update");
    assert_eq!(updates[3]["update"]["toolCallId"], "t1:call-1");
    assert_eq!(updates[3]["update"]["status"], "completed");
    assert_eq!(updates[3]["update"]["rawOutput"], "tool output");
    assert_eq!(
        updates[3]["update"]["content"][0]["type"],
        json!("content"),
        "tool content must carry the ACP wrapper"
    );
}

/// `session/list` projects storage rows into the ACP `SessionInfo` shape,
/// resolves `cwd` from the session metadata, and keeps cwd-less sessions
/// when the request filters by cwd (v2 `sessionSummaryToSessionInfo` +
/// `filterSessionSummariesByCwd`).
#[tokio::test]
async fn test_acp_session_list_shape_and_cwd_filter() {
    let server = AcpServer::in_memory().unwrap();
    server.store.create_session("sess-a", Some("A")).unwrap();
    server.store.create_session("sess-b", Some("B")).unwrap();
    server
        .store
        .put_state("metadata", "sess-b", &json!({ "cwd": "/tmp/ws" }))
        .unwrap();

    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "session/list", "params": {} });
    let resp = server.handle_message(&list.to_string()).await.unwrap();
    let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
    assert_eq!(sessions.len(), 2);
    let session_b = sessions
        .iter()
        .find(|session| session["sessionId"] == "sess-b")
        .expect("sess-b listed");
    assert_eq!(session_b["cwd"], "/tmp/ws");
    assert_eq!(session_b["title"], "B");
    assert!(session_b["updatedAt"].is_string());

    let matching = json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/list",
        "params": { "cwd": "/tmp/ws" }
    });
    let resp = server.handle_message(&matching.to_string()).await.unwrap();
    let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
    assert_eq!(sessions.len(), 2, "a cwd-less session is always kept");

    let other = json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/list",
        "params": { "cwd": "/tmp/other" }
    });
    let resp = server.handle_message(&other.to_string()).await.unwrap();
    let sessions = resp.result.unwrap()["sessions"].as_array().unwrap().clone();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], "sess-a");
}

/// `session/resume` re-attaches without replay; `session/fork` copies the
/// source history into a new session (v2 `resumeSession` / `forkSession`).
#[tokio::test]
async fn test_acp_resume_and_fork() {
    let server = AcpServer::in_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {}
    });
    let sid = server
        .handle_message(&new_req.to_string())
        .await
        .unwrap()
        .result
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let prompt_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/prompt",
        "params": { "sessionId": sid, "prompt": "hi" }
    });
    let resp = server
        .handle_message(&prompt_req.to_string())
        .await
        .unwrap();
    // No engine is wired, so the prompt is refused instead of being answered
    // with an invented reply. Seed the history this test needs explicitly —
    // it used to come from that fabricated turn.
    assert!(
        resp.error.is_some(),
        "a prompt with no engine wired must be refused, not faked"
    );
    use crate::turn_loop::types::LLMMessage;
    server
        .store
        .save_turn(&sid, "turn-1", 1, &[LLMMessage::user("hi")], None, None)
        .expect("the seed turn must persist");

    // Drop the session/new attach notifications so the resume assertion
    // below measures only what resume itself sends.
    while rx.try_recv().is_ok() {}
    // resume: mode state only, no replayed chunks.
    let resume_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/resume",
        "params": { "sessionId": sid }
    });
    let resp = server
        .handle_message(&resume_req.to_string())
        .await
        .unwrap();
    assert_eq!(resp.result.unwrap()["modes"]["currentModeId"], "default");
    let mut resumed = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            resumed.push(note.params.unwrap_or_default());
        }
    }
    assert!(
        drop_attach_updates(resumed).is_empty(),
        "resume must not replay history (that is session/load)"
    );

    // fork: new id, copied history.
    let fork_req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/fork",
        "params": { "sessionId": sid }
    });
    let resp = server.handle_message(&fork_req.to_string()).await.unwrap();
    let forked = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(forked, sid);
    assert_eq!(
        server.store.load_session_history(&forked).unwrap()[0].content,
        "hi"
    );

    let unknown_fork = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "session/fork",
        "params": { "sessionId": "sess-nope" }
    });
    assert_eq!(
        server
            .handle_message(&unknown_fork.to_string())
            .await
            .unwrap()
            .error
            .unwrap()
            .message,
        "Unknown sessionId: sess-nope"
    );

    let unknown_resume = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "session/resume",
        "params": { "sessionId": "sess-nope" }
    });
    assert_eq!(
        server
            .handle_message(&unknown_resume.to_string())
            .await
            .unwrap()
            .error
            .unwrap()
            .message,
        "Unknown sessionId: sess-nope"
    );
}

/// `session/close` tears the session down best-effort and clears its local
/// mode; an unknown session is rejected (v2 `closeSession`).
#[tokio::test]
async fn test_acp_close_session_clears_mode() {
    let server = AcpServer::in_memory().unwrap();
    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {}
    });
    let sid = server
        .handle_message(&new_req.to_string())
        .await
        .unwrap()
        .result
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let set_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/set_mode",
        "params": { "sessionId": sid, "modeId": "plan" }
    });
    server.handle_message(&set_req.to_string()).await.unwrap();
    assert_eq!(server.current_mode(&sid), "plan");

    let close_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/close",
        "params": { "sessionId": sid }
    });
    let resp = server.handle_message(&close_req.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp.result.unwrap(), json!({}));
    assert_eq!(server.current_mode(&sid), "default");

    let unknown = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/close",
        "params": { "sessionId": "sess-nope" }
    });
    let resp = server.handle_message(&unknown.to_string()).await.unwrap();
    assert_eq!(resp.error.unwrap().message, "Unknown sessionId: sess-nope");
}

/// `session/new` records `additionalDirectories` on the session so the
/// engine can merge them into the authorized boundary at turn start
/// (v2 `sessions.create({ additionalDirs })`).
#[tokio::test]
async fn test_acp_new_session_records_additional_directories() {
    let server = AcpServer::in_memory().unwrap();
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "cwd": "/tmp/ws",
            "additionalDirectories": ["/tmp/extra-a", "/tmp/extra-b"]
        }
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let sid = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let metadata = server.store.get_state("metadata", &sid).unwrap().unwrap();
    assert_eq!(metadata["cwd"], "/tmp/ws");
    assert_eq!(
        metadata["additional_dirs"],
        json!(["/tmp/extra-a", "/tmp/extra-b"])
    );

    // A session created without the field carries no empty list.
    let plain = json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {}
    });
    let resp = server.handle_message(&plain.to_string()).await.unwrap();
    let plain_sid = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        server
            .store
            .get_state("metadata", &plain_sid)
            .unwrap()
            .is_none()
    );
}

/// ACP `mcpServers` are ephemeral per-session servers (v2
/// `sessions.create({ mcpServers })`): they must not land on the
/// process-wide manager every other session shares.
#[tokio::test]
async fn test_acp_new_session_scopes_mcp_servers_to_the_session() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, "http://127.0.0.1:1/v1");
    let shared = Arc::new(crate::mcp::manager::McpManager::new());
    engine.set_mcp_manager(shared.clone());
    let server = AcpServer::with_engine(store, engine.clone());

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {
            "mcpServers": [{
                "name": "editor-tools",
                "command": "node",
                "args": ["server.js"]
            }]
        }
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let sid = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let session_manager = engine
        .mcp_manager_for(&sid)
        .expect("the session has a manager");
    assert!(
        !Arc::ptr_eq(&session_manager, &shared),
        "the client's servers must not land on the shared manager"
    );
    let names = |entries: Vec<crate::mcp::manager::McpServerEntry>| {
        entries
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>()
    };
    assert!(
        names(session_manager.server_entries().await).contains(&"editor-tools".to_string()),
        "the session's own manager carries the client's server"
    );
    assert!(
        !names(shared.server_entries().await).contains(&"editor-tools".to_string()),
        "the shared manager stays untouched"
    );

    // A session created without the field keeps the shared manager.
    let plain = json!({
        "jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {}
    });
    let resp = server.handle_message(&plain.to_string()).await.unwrap();
    let plain_sid = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(Arc::ptr_eq(
        &engine.mcp_manager_for(&plain_sid).unwrap(),
        &shared
    ));
}

/// `session/set_model` (the custom extension) and the `model` arm of
/// `session/set_config_option` both persist the per-session override and
/// push `config_option_update` (v2 `setSessionModel` /
/// `setSessionConfigOption`).
#[tokio::test]
async fn test_acp_set_model_persists_and_notifies() {
    let server = AcpServer::in_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let new_req = json!({
        "jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {}
    });
    let sid = server
        .handle_message(&new_req.to_string())
        .await
        .unwrap()
        .result
        .unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let set_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/set_model",
        "params": { "sessionId": sid, "modelId": "kimi-k2" }
    });
    let resp = server.handle_message(&set_req.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp.result.unwrap(), json!({}));
    assert_eq!(
        server
            .store
            .get_state("agent_config", &sid)
            .unwrap()
            .unwrap()["model"],
        "kimi-k2"
    );
    assert_eq!(server.session_model(&sid), "kimi-k2");

    let params = next_non_attach_update(&mut rx);
    assert_eq!(params["update"]["sessionUpdate"], "config_option_update");
    assert_eq!(params["update"]["configOptions"][0]["id"], "model");
    assert_eq!(
        params["update"]["configOptions"][0]["currentValue"],
        "kimi-k2"
    );

    // The config-option route reaches the same state and answers with the
    // refreshed option surface.
    let config_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/set_config_option",
        "params": { "sessionId": sid, "configId": "model", "value": "kimi-k3" }
    });
    let resp = server
        .handle_message(&config_req.to_string())
        .await
        .unwrap();
    assert_eq!(
        resp.result.unwrap()["configOptions"][0]["currentValue"],
        "kimi-k3"
    );
    assert_eq!(server.session_model(&sid), "kimi-k3");

    // Unknown ids and unknown config ids stay invalid params.
    let unknown_session = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/set_model",
        "params": { "sessionId": "sess-nope", "modelId": "kimi-k2" }
    });
    assert_eq!(
        server
            .handle_message(&unknown_session.to_string())
            .await
            .unwrap()
            .error
            .unwrap()
            .message,
        "Unknown sessionId: sess-nope"
    );
    let unknown_config = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "session/set_config_option",
        "params": { "sessionId": sid, "configId": "thinking", "value": "on" }
    });
    assert_eq!(
        server
            .handle_message(&unknown_config.to_string())
            .await
            .unwrap()
            .error
            .unwrap()
            .message,
        "Unsupported configId: thinking"
    );
}
