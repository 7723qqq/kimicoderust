use super::super::*;

/// `session/new` advertises the canonical four modes (`modes.ts:24-46`).
#[tokio::test]
async fn test_acp_new_session_advertises_modes() {
    let server = AcpServer::in_memory().unwrap();
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": { "cwd": "/tmp/workspace" }
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let res = resp.result.unwrap();
    assert!(res["sessionId"].is_string());
    assert_eq!(res["modes"]["currentModeId"], "default");
    let available = res["modes"]["availableModes"].as_array().unwrap();
    assert_eq!(available.len(), 4);
    assert_eq!(available[0]["id"], "default");
    assert_eq!(available[3]["id"], "yolo");
}

/// `session/new` binds the requested `cwd` to a workspace and records it
/// in metadata, so `session/list`'s cwd filter and the engine's workspace
/// root both resolve it.
#[tokio::test]
async fn test_acp_new_session_records_cwd_workspace() {
    let server = AcpServer::in_memory().unwrap();
    let req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "session/new",
        "params": { "cwd": "/tmp/acp-work", "title": "ACP Work" }
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let session_id = resp.result.unwrap()["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let summary = server.store.get_session(&session_id).unwrap().unwrap();
    assert!(
        summary.workspace_id.is_some(),
        "a cwd must associate a workspace"
    );
    assert_eq!(server.session_cwd(&session_id), "/tmp/acp-work");
    let workspace = server
        .store
        .list_workspaces()
        .unwrap()
        .into_iter()
        .find(|workspace| workspace.root == "/tmp/acp-work")
        .expect("the cwd workspace is registered");
    assert_eq!(summary.workspace_id.as_deref(), Some(workspace.id.as_str()));
}

#[test]
fn test_acp_mcp_recipe_transports() {
    let stdio = acp_mcp_recipe(&json!({
        "name": "fs",
        "command": "npx",
        "args": ["-y", "server-files"],
        "env": { "TOKEN": "x" }
    }))
    .expect("stdio recipe");
    match stdio {
        McpServerRecipe::Stdio {
            command, args, env, ..
        } => {
            assert_eq!(command, "npx");
            assert_eq!(args, vec!["-y", "server-files"]);
            assert_eq!(env.get("TOKEN").map(String::as_str), Some("x"));
        }
        other => panic!("expected stdio, got {other:?}"),
    }

    let http = acp_mcp_recipe(&json!({
        "name": "web",
        "type": "http",
        "url": "https://example.test/mcp",
        "headers": { "X-Auth": "y" }
    }))
    .expect("http recipe");
    match http {
        McpServerRecipe::Http { url, headers, .. } => {
            assert_eq!(url, "https://example.test/mcp");
            assert_eq!(headers.get("X-Auth").map(String::as_str), Some("y"));
        }
        other => panic!("expected http, got {other:?}"),
    }

    let sse = acp_mcp_recipe(&json!({
        "name": "stream",
        "type": "sse",
        "url": "https://example.test/sse"
    }))
    .expect("sse recipe");
    assert!(matches!(sse, McpServerRecipe::Sse { .. }));
    assert!(acp_mcp_recipe(&json!({ "name": "empty" })).is_none());
}

/// `session/new` advertises `configOptions` = `[model, mode]` with the
/// canonical four-mode `mode` arm and a single-row `model` arm (no model
/// catalog, v2 `buildSessionConfigOptions`, config-options.ts). The
/// `thinking` arm is omitted because no catalog row declares thinking
/// support.
#[tokio::test]
async fn test_acp_new_session_advertises_config_options() {
    let server = AcpServer::in_memory().unwrap();
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {}
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let res = resp.result.unwrap();
    let options = res["configOptions"].as_array().unwrap();
    assert_eq!(
        options.len(),
        2,
        "model + mode, no thinking arm without a catalog"
    );

    let model = &options[0];
    assert_eq!(model["type"], "select");
    assert_eq!(model["id"], "model");
    assert_eq!(model["category"], "model");
    // No engine attached → the model arm is honest: empty currentValue and
    // no rows (v2 keeps the unbound defaults).
    assert_eq!(model["currentValue"], "");
    assert_eq!(model["options"].as_array().unwrap().len(), 0);

    let mode = &options[1];
    assert_eq!(mode["id"], "mode");
    assert_eq!(mode["category"], "mode");
    assert_eq!(mode["currentValue"], "default");
    let mode_options = mode["options"].as_array().unwrap();
    assert_eq!(mode_options.len(), 4);
    assert_eq!(mode_options[0]["value"], "default");
    assert_eq!(mode_options[0]["name"], "Default");
    assert_eq!(mode_options[3]["value"], "yolo");
}

/// With an engine attached, the `model` arm carries the engine's single
/// model as both `currentValue` and the one selectable row.
#[tokio::test]
async fn test_acp_config_options_model_arm_from_engine() {
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
    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/new",
        "params": {}
    });
    let resp = server.handle_message(&req.to_string()).await.unwrap();
    let res = resp.result.unwrap();
    let model = &res["configOptions"][0];
    assert_eq!(model["currentValue"], "kimi-k2");
    let rows = model["options"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"], "kimi-k2");
    assert_eq!(rows[0]["name"], "kimi-k2");
}
