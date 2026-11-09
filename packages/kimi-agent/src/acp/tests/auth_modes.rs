use super::super::*;
use super::common::*;

/// The auth gate refuses `session/new` with `auth_required` (`-32000`)
/// when no engine (model) is attached and auth is not disabled
/// (v2 `ensureAuthed`, server.ts:625-646).
#[tokio::test]
async fn test_acp_auth_gate_blocks_session_new_without_engine() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = AcpServer::new(store); // disable_auth = false, no engine
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {}
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32000);
    assert!(err.message.contains("Authentication required"));
}

/// `authenticate` validates `methodId` and re-checks the gate
/// (v2 `authenticate`, server.ts:380-390).
#[tokio::test]
async fn test_acp_authenticate_validates_method_and_gate() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = AcpServer::new(store);

    // Wrong methodId → invalid params.
    let bad = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "authenticate",
        "params": { "methodId": "oauth" }
    });
    let resp = server.handle_message(&bad.to_string()).await.unwrap();
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32602);

    // Correct methodId but no engine → auth_required.
    let good = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "authenticate",
        "params": { "methodId": "login" }
    });
    let resp = server.handle_message(&good.to_string()).await.unwrap();
    let err = resp.error.unwrap();
    assert_eq!(err.code, -32000);
}

/// With an engine attached, `authenticate('login')` succeeds and `logout`
/// is a no-op success (the Rust engine has no managed token to drop).
#[tokio::test]
async fn test_acp_authenticate_succeeds_with_engine() {
    use crate::pipeline::PipelineSpec;
    use crate::server::engine::ServerEngine;

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = ServerEngine::new(
        PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "kimi-k2".into(),
            providers: Vec::new(),
            native_llm: None,
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: false,
            shell_path: None,
            policy_snapshot: None,
            github_token: None,
            github_base_url: None,
            subagent_timeout_ms: None,
            agent_tool_veto: None,
            tools_veto: None,
            todo_tool_veto: None,
            tower_worktree_root: None,
            tower_enabled: false,
            notify_user_enabled: true,
            tool_select: false,
            sandbox_mode: None,
            sandbox_policy: None,
            caller_agent_id: None,
            session_id: None,
            secondary_model: None,
            image_read_byte_budget: None,
            image_max_edge_px: None,
            model_capabilities: None,
            skill_dirs: Vec::new(),
            plugin_skill_dirs: Vec::new(),
            merge_all_available_skills: true,
            background: crate::storage::BackgroundLimits::default(),
        },
        Arc::new(crate::server::hub::EventHub::new()),
        store.clone(),
    );
    let server = AcpServer::with_engine(store, Arc::new(engine));

    let auth = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "authenticate",
        "params": { "methodId": "login" }
    });
    let resp = server.handle_message(&auth.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "engine attached → authed");

    let logout = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "logout",
        "params": {}
    });
    let resp = server.handle_message(&logout.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "logout is a no-op success");
}

/// ACP clients send `prompt` as `ContentBlock[]`; text, text resources and
/// resource links survive, audio/image blocks are dropped
/// (v2 `acpBlocksToContentParts`, convert.ts:26-78).
#[tokio::test]
async fn test_acp_prompt_accepts_content_blocks() {
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

    let prompt_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/prompt",
        "params": {
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "look at this" },
                {
                    "type": "resource",
                    "resource": { "uri": "file:///tmp/a.txt", "text": "body" }
                },
                { "type": "resource_link", "uri": "file:///tmp/b.txt", "name": "b" },
                { "type": "image", "mimeType": "image/png", "data": "AAAA" }
            ]
        }
    });
    let resp = server
        .handle_message(&prompt_req.to_string())
        .await
        .unwrap();
    // No engine is wired: the prompt is refused instead of being answered
    // with an invented reply, so nothing is persisted for it. The
    // block→text projection therefore has to be asserted directly (below)
    // rather than through a fabricated history entry.
    let err = resp
        .error
        .expect("a prompt with no engine wired must be refused, not faked");
    assert_eq!(err.code, -32000);

    let prompt_value = json!([
        { "type": "text", "text": "look at this" },
        {
            "type": "resource",
            "resource": { "uri": "file:///tmp/a.txt", "text": "body" }
        },
        { "type": "resource_link", "uri": "file:///tmp/b.txt", "name": "b" },
        { "type": "image", "mimeType": "image/png", "data": "AAAA" }
    ]);
    let (text, media) =
        acp_prompt_to_parts(&prompt_value).expect("the prompt must project to text");
    assert_eq!(
        text,
        "look at this\n<resource uri=\"file:///tmp/a.txt\">body</resource>\n\
             <resource_link uri=\"file:///tmp/b.txt\" name=\"b\" />"
    );
    assert_eq!(media.len(), 1, "the image block becomes a media attachment");
}

/// ACP image blocks become native media blocks; the text projection keeps
/// only the textual blocks.
#[test]
fn test_acp_blocks_to_media_extracts_images() {
    let blocks = vec![
        json!({ "type": "text", "text": "look" }),
        json!({ "type": "image", "mimeType": "image/jpeg", "data": "AAAB" }),
        json!({ "type": "resource_link", "uri": "file:///b", "name": "b" }),
        json!({ "type": "image", "mimeType": "image/png", "data": "CCCC" }),
    ];
    let media = acp_blocks_to_media(&blocks);
    assert_eq!(media.len(), 2);
    match &media[0] {
        crate::rpc::types::ContentBlock::Image {
            media_type, data, ..
        } => {
            assert_eq!(media_type, "image/jpeg");
            assert_eq!(data, "AAAB");
        }
        other => panic!("expected image, got {other:?}"),
    }
    let (text, media) = acp_prompt_to_parts(&json!([
        { "type": "text", "text": "look" },
        { "type": "image", "mimeType": "image/png", "data": "CCCC" },
    ]))
    .unwrap();
    assert_eq!(text, "look");
    assert_eq!(media.len(), 1);
    assert!(acp_prompt_to_parts(&json!("plain")).is_some());
    assert!(acp_prompt_to_parts(&json!(42)).is_none());
}

/// `initialize` records the client's reverse-RPC capabilities.
#[tokio::test]
async fn test_acp_initialize_records_client_capabilities() {
    let server = AcpServer::in_memory().unwrap();
    assert!(!server.client_capabilities().fs.read_text_file);
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": true, "writeTextFile": true },
                "terminal": true,
            },
        }
    });
    server.handle_message(&req.to_string()).await.unwrap();
    let capabilities = server.client_capabilities();
    assert!(capabilities.fs.read_text_file);
    assert!(capabilities.fs.write_text_file);
    assert!(capabilities.terminal);
}

/// `session/set_mode` validates the mode id, persists the permission mode
/// the engine reads at turn start, and pushes `current_mode_update`
/// (v2 `setSessionMode` + `acpModeToToggles`).
#[tokio::test]
async fn test_acp_set_mode_persists_permission_and_notifies() {
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

    let set_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/set_mode",
        "params": { "sessionId": sid, "modeId": "yolo" }
    });
    let resp = server.handle_message(&set_req.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp.result.unwrap()["modeId"], "yolo");

    let metadata = server.store.get_state("metadata", &sid).unwrap().unwrap();
    assert_eq!(metadata["permission_mode"], "yolo");

    let params = next_non_attach_update(&mut rx);
    assert_eq!(params["sessionId"], sid);
    assert_eq!(params["update"]["sessionUpdate"], "current_mode_update");
    assert_eq!(params["update"]["currentModeId"], "yolo");
}

/// v2 `acpModeToToggles`: `plan` applies BOTH toggles — the plan domain
/// goes active and the permission mode lands. Only the permission
/// half used to reach the engine, so an ACP client selecting Plan
/// got manual permissions and no plan mode.
#[tokio::test]
async fn test_acp_set_mode_plan_activates_plan_mode() {
    let workspace = tempfile::TempDir::new().unwrap();
    let root = workspace.path().to_string_lossy().to_string();
    let server = AcpServer::in_memory().unwrap();

    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "cwd": root }
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
    let resp = server.handle_message(&set_req.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");

    // Permission half (pre-existing behavior).
    let metadata = server.store.get_state("metadata", &sid).unwrap().unwrap();
    assert_eq!(metadata["permission_mode"], "manual");

    // Plan half: the `plan` domain the engine reads is now active. The
    // domain is session-scoped (v2 agent scope), like the host's own
    // plan-mode entry it must not shadow.
    let state =
        crate::storage::state_store::StateStore::for_session(workspace.path(), &sid).unwrap();
    assert_eq!(
        state
            .read_domain_state("plan")
            .into_result()
            .unwrap()
            .unwrap()["active"],
        true,
        "plan mode must be active after session/set_mode plan"
    );

    // Leaving plan mode for another mode clears the domain.
    let back = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/set_mode",
        "params": { "sessionId": sid, "modeId": "default" }
    });
    let resp = server.handle_message(&back.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(
        state
            .read_domain_state("plan")
            .into_result()
            .unwrap()
            .unwrap()["active"],
        false,
        "leaving plan mode must clear the plan domain"
    );
}

/// Unknown ids are rejected with the v2 messages (server.ts:452-469).
#[tokio::test]
async fn test_acp_set_mode_rejects_unknown_ids() {
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

    let unknown_mode = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/set_mode",
        "params": { "sessionId": sid, "modeId": "turbo" }
    });
    let resp = server
        .handle_message(&unknown_mode.to_string())
        .await
        .unwrap();
    assert_eq!(resp.error.as_ref().unwrap().code, -32602);
    assert_eq!(resp.error.unwrap().message, "Unknown modeId: turbo");

    let unknown_session = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/set_mode",
        "params": { "sessionId": "sess-nope", "modeId": "auto" }
    });
    let resp = server
        .handle_message(&unknown_session.to_string())
        .await
        .unwrap();
    assert_eq!(resp.error.unwrap().message, "Unknown sessionId: sess-nope");
}

/// `session/set_config_option` routes the `mode` arm to the same handler.
#[tokio::test]
async fn test_acp_set_config_option_mode_arm() {
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

    let mode_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/set_config_option",
        "params": { "sessionId": sid, "configId": "mode", "value": "auto" }
    });
    let resp = server.handle_message(&mode_req.to_string()).await.unwrap();
    let options = resp.result.unwrap()["configOptions"].clone();
    let mode_option = options
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "mode")
        .expect("the mode option is advertised");
    assert_eq!(mode_option["currentValue"], "auto");
    assert_eq!(
        server.store.get_state("metadata", &sid).unwrap().unwrap()["permission_mode"],
        "auto"
    );

    let model_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "session/set_config_option",
        "params": { "sessionId": sid, "configId": "model", "value": "kimi-k2" }
    });
    let resp = server.handle_message(&model_req.to_string()).await.unwrap();
    // The `model` arm switches the session's model and answers with the
    // refreshed option set (v2 `setSessionConfigOption`).
    let options = resp.result.unwrap()["configOptions"].clone();
    let model_option = options
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "model")
        .expect("the model option is advertised");
    assert_eq!(model_option["currentValue"], "kimi-k2");

    let unknown_req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "session/set_config_option",
        "params": { "sessionId": sid, "configId": "nonsense", "value": "x" }
    });
    let resp = server
        .handle_message(&unknown_req.to_string())
        .await
        .unwrap();
    assert_eq!(
        resp.error.unwrap().message,
        "Unsupported configId: nonsense"
    );
}
