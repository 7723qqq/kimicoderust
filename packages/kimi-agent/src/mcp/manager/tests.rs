use super::*;
use crate::mcp::sse::test_helpers::spawn_mock_mcp_sse_server;
use serde_json::json;

/// v2 `resolveDeferred` (`connection-manager.ts:244-250`): the declaration
/// wins, a positive threshold defers at or above the count, and `0` disables
/// automatic deferral.
#[test]
fn auto_defer_threshold_resolution_matches_v2() {
    assert!(!resolve_deferred(Some(false), 100, 24, Some(true)));
    assert!(resolve_deferred(Some(true), 0, 24, None));

    assert!(resolve_deferred(None, 24, 24, None));
    assert!(!resolve_deferred(None, 23, 24, None));
    assert!(!resolve_deferred(None, 23, 0, None));
    assert!(resolve_deferred(None, 0, 0, Some(true)), "0 disables");
    assert!(
        resolve_deferred(None, 23, 24, Some(true)),
        "default applies"
    );
}

/// v2 `parseAutoDeferToolThresholdEnv`: integers only, capped at 1e6.
#[test]
fn auto_defer_threshold_env_parsing_matches_v2() {
    assert_eq!(parse_auto_defer_tool_threshold(Some("16")), Some(16));
    assert_eq!(parse_auto_defer_tool_threshold(Some(" 16 ")), Some(16));
    assert_eq!(parse_auto_defer_tool_threshold(Some("0")), Some(0));
    assert_eq!(parse_auto_defer_tool_threshold(Some("1.5")), None);
    assert_eq!(parse_auto_defer_tool_threshold(Some("1000001")), None);
    assert_eq!(parse_auto_defer_tool_threshold(Some("")), None);
    assert_eq!(parse_auto_defer_tool_threshold(None), None);
}

#[tokio::test]
async fn test_mcp_manager_discovery_and_call() {
    let manager = McpManager::new();
    let client = McpClient::mock("github");
    manager.add_client(client).await;

    // Only the qualified name exists: plain aliases were removed
    // (last-writer-wins across servers could misroute a model call).
    assert!(manager.handles("mcp__github__github_sample_tool").await);
    assert!(!manager.handles("github_sample_tool").await);
    assert!(!manager.handles("nonexistent_tool").await);

    // 1. Call via the qualified name
    let res_qualified = manager
        .call_tool(
            "mcp__github__github_sample_tool",
            &json!({ "query": "kimi" }),
            None,
        )
        .await
        .expect("call_tool via qualified name failed");

    assert!(!res_qualified.is_error);
    assert_eq!(
        res_qualified.content,
        "Mock execution of github_sample_tool with {\"query\":\"kimi\"}"
    );
    assert!(
        res_qualified.note.is_none(),
        "note = {:?}",
        res_qualified.note
    );

    // 2. A bare tool name is not routable to an MCP server
    let res_plain = manager
        .call_tool("github_sample_tool", &json!({ "query": "kimi" }), None)
        .await;
    assert!(res_plain.is_none(), "plain tool names must not be routed");

    // 3. Call unknown tool returns None
    let res_missing = manager.call_tool("unknown_tool", &json!({}), None).await;
    assert!(res_missing.is_none());
}

/// v2 #4719 / #4733: a same-name MCP tool whose definition changed is held
/// pending. The running session keeps the active definition it was shown;
/// a new session (list_live) would see the replacement.
#[tokio::test]
async fn a_changed_same_name_mcp_tool_stays_pinned_for_this_session() {
    let manager = McpManager::new();
    let old = vec![McpTool {
        name: "search".into(),
        description: Some("old search".into()),
        input_schema: json!({ "type": "object" }),
    }];
    manager
        .add_client(McpClient::mock_with_tools("srv", old))
        .await;
    let first = manager.list_tool_infos().await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].description, "old search");

    let updated = vec![McpTool {
        name: "search".into(),
        description: Some("updated search".into()),
        input_schema: json!({ "type": "object" }),
    }];
    manager
        .add_client(McpClient::mock_with_tools("srv", updated))
        .await;
    let pinned = manager.list_tool_infos().await;
    assert_eq!(pinned.len(), 1, "no duplicate declaration");
    assert_eq!(
        pinned[0].description, "old search",
        "active schema stays pinned"
    );
    assert!(
        manager.has_pending_tool("mcp__srv__search").await,
        "the replacement is registered as pending"
    );
    assert_eq!(
        manager.list_live_tool_infos().await[0].description,
        "updated search",
        "a new session would see the update"
    );

    assert!(manager.remove_server("srv").await);
    assert!(manager.list_tool_infos().await.is_empty());
}

#[tokio::test]
async fn test_mcp_manager_list_tool_infos() {
    let manager = McpManager::new();
    let client_a = McpClient::mock("github");
    let client_b = McpClient::mock("slack");
    manager.add_client(client_a).await;
    manager.add_client(client_b).await;

    let mut infos = manager.list_tool_infos().await;
    infos.sort_by(|a, b| a.name.cmp(&b.name));

    // Only the namespaced form is exposed to the model, aliases are not leaked
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[0].name, "mcp__github__github_sample_tool");
    assert_eq!(infos[0].description, "Sample mock MCP tool");
    assert_eq!(
        infos[0].input_schema,
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            }
        })
    );

    assert_eq!(infos[1].name, "mcp__slack__slack_sample_tool");
    assert_eq!(infos[1].description, "Sample mock MCP tool");
}

#[tokio::test]
async fn test_mcp_manager_server_entries() {
    let manager = McpManager::new();
    // Add in reverse alphabetical order to verify alphabetical sorting
    manager.add_client(McpClient::mock("zeta")).await;
    manager.add_client(McpClient::mock("alpha")).await;

    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 2);

    // Verify sorted by server name
    assert_eq!(entries[0].name, "alpha");
    assert_eq!(entries[0].transport, "mock");
    assert_eq!(entries[0].status, "connected");
    assert_eq!(entries[0].tool_count, 1);
    assert_eq!(entries[0].error, None);
    assert_eq!(entries[0].tools.len(), 1);
    assert_eq!(entries[0].tools[0].name, "alpha_sample_tool");
    assert_eq!(entries[0].tools[0].description, "Sample mock MCP tool");

    assert_eq!(entries[1].name, "zeta");
    assert_eq!(entries[1].transport, "mock");
    assert_eq!(entries[1].status, "connected");
    assert_eq!(entries[1].tool_count, 1);
    assert_eq!(entries[1].tools[0].name, "zeta_sample_tool");
}

#[tokio::test]
async fn test_mcp_manager_with_sse_client_and_call_tool() {
    let (sse_url, _shutdown) = spawn_mock_mcp_sse_server(None, None, None).await;

    let client = McpClient::connect_sse("calc_server", &sse_url, HashMap::new(), None)
        .await
        .expect("SSE client connect failed");

    let manager = McpManager::new();
    manager.add_client(client).await;

    // Discovered tools are indexed by qualified name only (no plain alias)
    assert!(!manager.handles("calculate").await);
    assert!(manager.handles("mcp__calc_server__calculate").await);
    assert!(!manager.handles("echo").await);
    assert!(manager.handles("mcp__calc_server__echo").await);

    // Call tool over SSE
    let res = manager
        .call_tool(
            "mcp__calc_server__calculate",
            &json!({ "expression": "10+32" }),
            None,
        )
        .await
        .expect("tool call failed");

    assert!(!res.is_error);
    assert_eq!(res.content, "result: 42");
    assert!(res.note.is_none(), "note = {:?}", res.note);

    // Application-level error tool call
    let err_res = manager
        .call_tool("mcp__calc_server__trigger_tool_error", &json!({}), None)
        .await
        .expect("tool call failed");
    assert!(err_res.is_error);
    assert_eq!(err_res.content, "custom tool failure");
    assert!(err_res.note.is_none(), "note = {:?}", err_res.note);
}

#[tokio::test]
async fn test_mcp_manager_spawn_from_config() {
    let (sse_url, _shutdown) = spawn_mock_mcp_sse_server(None, None, None).await;

    let mut configs = HashMap::new();
    configs.insert(
        "valid_sse".to_string(),
        crate::config::McpServerConfig {
            command: None,
            args: None,
            env: None,
            url: Some(sse_url),
            headers: None,
            // The mock speaks legacy HTTP+SSE, which is opt-in now that a
            // bare url defaults to Streamable HTTP.
            transport: Some("sse".into()),
            ..Default::default()
        },
    );
    // Invalid/unreachable SSE endpoint
    configs.insert(
        "invalid_sse".to_string(),
        crate::config::McpServerConfig {
            command: None,
            args: None,
            env: None,
            url: Some("http://127.0.0.1:1/nonexistent_sse".into()),
            headers: None,
            transport: Some("sse".into()),
            ..Default::default()
        },
    );
    // Invalid stdio command
    configs.insert(
        "invalid_stdio".to_string(),
        crate::config::McpServerConfig {
            command: Some("definitely_missing_binary_404".into()),
            args: None,
            env: None,
            url: None,
            headers: None,
            ..Default::default()
        },
    );

    let manager = std::sync::Arc::new(McpManager::new());
    let config = crate::config::KimiConfig {
        mcp_servers: configs,
        // This test is about the connect-failure status; the startup retry
        // has its own tests.
        mcp: crate::config::McpConfig {
            reconnect_retry_enabled: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    manager.spawn_from_config(&config).await;

    // The valid server connects and registers tools; invalid ones are gracefully skipped
    assert!(manager.handles("mcp__valid_sse__calculate").await);
    assert!(!manager.handles("mcp__invalid_sse__calculate").await);
    assert!(!manager.handles("mcp__invalid_stdio__calculate").await);

    // Failed servers surface as failed entries with their spawn error
    // instead of being silently skipped (v2 marks failed entries).
    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 3, "all configured servers must be visible");
    let by_name = |name: &str| entries.iter().find(|e| e.name == name).unwrap();
    assert_eq!(by_name("valid_sse").status, "connected");
    assert_eq!(by_name("invalid_sse").status, "failed");
    assert!(by_name("invalid_sse").error.is_some());
    assert_eq!(by_name("invalid_stdio").status, "failed");
    assert!(by_name("invalid_stdio").error.is_some());
}

/// `spawn_from_config` connects servers in parallel (v2 `connectAllNow` +
/// `Promise.allSettled`): two servers that each time out after 300ms must
/// finish in ~300ms total, not the ~600ms a serial loop would take.
#[tokio::test]
async fn test_spawn_from_config_parallel() {
    let spawn_hanging = || async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept and hold every connection open without ever responding.
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        addr
    };
    let addr1 = spawn_hanging().await;
    let addr2 = spawn_hanging().await;

    let mut configs = HashMap::new();
    configs.insert(
        "slow1".to_string(),
        crate::config::McpServerConfig {
            url: Some(format!("http://{addr1}/sse")),
            transport: Some("sse".into()),
            startup_timeout_ms: Some(300),
            ..Default::default()
        },
    );
    configs.insert(
        "slow2".to_string(),
        crate::config::McpServerConfig {
            url: Some(format!("http://{addr2}/sse")),
            transport: Some("sse".into()),
            startup_timeout_ms: Some(300),
            ..Default::default()
        },
    );

    let manager = std::sync::Arc::new(McpManager::new());
    let config = crate::config::KimiConfig {
        mcp_servers: configs,
        // This test is about the connect-failure status; the startup retry
        // has its own tests.
        mcp: crate::config::McpConfig {
            reconnect_retry_enabled: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    let start = std::time::Instant::now();
    manager.spawn_from_config(&config).await;
    let elapsed = start.elapsed();

    // Parallel: ~300ms (the slowest server). Serial would be ~600ms.
    assert!(
        elapsed < Duration::from_millis(550),
        "parallel connect took {elapsed:?}, expected ~300ms"
    );
    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        assert_eq!(entry.status, "failed", "both hanging servers time out");
    }
}

/// `connect_all` starts the connects and returns without waiting for them
/// (v2 `connectAll`, connection-manager.ts:170-181). Session creation uses
/// it so a slow server no longer blocks the session; the readiness it
/// flips is what the tool table awaits instead.
#[tokio::test]
async fn test_connect_all_returns_before_the_connects_settle() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept and hold every connection open without ever responding, so
    // the connect can only end through its startup timeout.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let manager = Arc::new(McpManager::new());
    let start = std::time::Instant::now();
    manager.connect_all(vec![(
        "slow".to_string(),
        McpServerRecipe::Sse {
            url: format!("http://{addr}/sse"),
            headers: HashMap::new(),
            bearer_token_env_var: None,
        },
        McpServerOptions {
            startup_timeout_ms: Some(300),
            ..Default::default()
        },
    )]);
    assert!(
        start.elapsed() < Duration::from_millis(150),
        "connect_all blocked for {:?}; it must only start the connects",
        start.elapsed()
    );

    manager.wait_for_initial_load().await;
    assert!(
        start.elapsed() >= Duration::from_millis(250),
        "readiness settled before the connect could time out"
    );

    // Once settled, waiting again is free.
    let after = std::time::Instant::now();
    manager.wait_for_initial_load().await;
    assert!(after.elapsed() < Duration::from_millis(50));
}

/// A manager that never started an initial load must not block a consumer
/// awaiting readiness. The server/REPL path connects through
/// `spawn_from_config` (which awaits its own connects) and the napi path
/// can register clients directly, so readiness has to start as "nothing to
/// wait for" — otherwise `list_tools` would hang forever on those managers.
#[tokio::test]
async fn test_wait_for_initial_load_returns_without_a_load() {
    let manager = McpManager::new();
    let start = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(1), manager.wait_for_initial_load())
        .await
        .expect("a manager with no initial load must not block");
    assert!(start.elapsed() < Duration::from_millis(100));
}

/// Reconnecting a server whose endpoint died must surface `failed` with
/// the error instead of pretending to be connected (v2 marks failed
/// entries, connection-manager.ts:366-372).
#[tokio::test]
async fn test_reconnect_marks_failed_when_endpoint_dies() {
    let (sse_url, shutdown) = spawn_mock_mcp_sse_server(None, None, None).await;

    let mut configs = HashMap::new();
    configs.insert(
        "srv".to_string(),
        crate::config::McpServerConfig {
            command: None,
            args: None,
            env: None,
            url: Some(sse_url),
            headers: None,
            // The mock speaks legacy HTTP+SSE, which is opt-in now that a
            // bare url defaults to Streamable HTTP.
            transport: Some("sse".into()),
            ..Default::default()
        },
    );
    let manager = std::sync::Arc::new(McpManager::new());
    let config = crate::config::KimiConfig {
        mcp_servers: configs,
        // This test is about the connect-failure status; the startup retry
        // has its own tests.
        mcp: crate::config::McpConfig {
            reconnect_retry_enabled: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    manager.spawn_from_config(&config).await;
    assert_eq!(manager.server_entries().await[0].status, "connected");

    // Kill the mock server, then reconnect against the stored recipe.
    let _ = shutdown.send(());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(manager.reconnect("srv").await.is_err());
    let entry = &manager.server_entries().await[0];
    assert_eq!(entry.status, "failed");
    assert!(entry.error.is_some());
}

/// Reconnecting an unconfigured server is an error, matching the TS
/// manager's unknown-name behavior.
#[tokio::test]
async fn test_reconnect_unknown_server_errors() {
    let manager = McpManager::new();
    assert!(manager.reconnect("nope").await.is_err());
}

/// A session surfaces the roster's degradations at startup: a server that
/// failed to connect, and one waiting on authorization. A healthy roster
/// and a deliberately disabled server produce nothing.
#[tokio::test]
async fn test_session_warnings_report_failed_and_needs_auth_servers() {
    let manager = McpManager::new();
    assert!(manager.session_warnings().await.is_empty());

    // A stdio command that cannot spawn: the connect fails outright.
    manager
        .configure(
            "broken",
            McpServerRecipe::Stdio {
                command: "definitely-not-a-real-binary-xyz".into(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("the spawn must fail");

    let warnings = manager.session_warnings().await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0].code, "mcp.server_failed");
    assert_eq!(warnings[0].severity, "warning");
    assert!(warnings[0].message.contains("broken"), "{warnings:?}");

    // A disabled server is the user's own choice, not a warning.
    manager
        .configure(
            "off",
            McpServerRecipe::Mock,
            McpServerOptions {
                enabled: false,
                ..Default::default()
            },
        )
        .await
        .expect("a disabled server never connects");
    let warnings = manager.session_warnings().await;
    assert_eq!(
        warnings.len(),
        1,
        "the disabled server adds nothing: {warnings:?}"
    );
}

/// Reconnecting a disabled server is an error (v2 throws
/// `MCP_SERVER_DISABLED`, connection-manager.ts:146-164).
#[tokio::test]
async fn test_reconnect_disabled_server_errors() {
    let manager = McpManager::new();
    manager
        .configure(
            "off",
            McpServerRecipe::Mock,
            McpServerOptions {
                enabled: false,
                ..Default::default()
            },
        )
        .await
        .expect("disabled server registers without connecting");
    assert_eq!(manager.server_entries().await[0].status, "disabled");

    let err = manager
        .reconnect("off")
        .await
        .expect_err("disabled server must not reconnect");
    assert_eq!(err, "MCP server is disabled: off");
}

/// A panicking status listener must not break the connection manager or
/// prevent other listeners from receiving the entry (v2 wraps listener
/// calls in try/catch, connection-manager.ts:435-441).
#[tokio::test]
async fn test_panicking_listener_does_not_break_emit() {
    let manager = McpManager::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let recorder = seen.clone();
    manager.on_status_change(Box::new(move |_| {
        panic!("listener fault");
    }));
    manager.on_status_change(Box::new(move |entry| {
        recorder.lock().unwrap().push(entry.status);
    }));

    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");

    let statuses = seen.lock().unwrap();
    assert_eq!(
        statuses.len(),
        2,
        "the healthy listener still receives both transitions"
    );
    assert_eq!(statuses[0], "pending");
    assert_eq!(statuses[1], "connected");
}

/// `reconnect_after_current` waits for any in-flight reconnect, then
/// starts a fresh one (v2 `reconnectAfterCurrent`,
/// connection-manager.ts:297-301). A completed reconnect clears its
/// in-flight marker, so a later `reconnect` reconnects instead of
/// returning on a stale notify.
#[tokio::test]
async fn test_reconnect_after_current_reconnects() {
    let manager = McpManager::new();
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");
    assert_eq!(manager.server_entries().await[0].status, "connected");

    // No in-flight reconnect: reconnect_after_current just reconnects.
    manager
        .reconnect_after_current("mock")
        .await
        .expect("reconnect succeeds");
    assert_eq!(manager.server_entries().await[0].status, "connected");

    // The in-flight marker was cleared, so a plain reconnect also works.
    manager
        .reconnect("mock")
        .await
        .expect("second reconnect succeeds");
    assert_eq!(manager.server_entries().await[0].status, "connected");
}

/// Qualified names are sanitized before they reach the model
/// (v2 `tool-naming.ts` `qualifyMcpToolName`).
#[tokio::test]
async fn test_qualified_tool_names_are_sanitized() {
    let manager = McpManager::new();
    manager.add_client(McpClient::mock("My Search")).await;

    assert!(
        manager
            .handles("mcp__My_Search__My_Search_sample_tool")
            .await
    );
    let infos = manager.list_tool_infos().await;
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "mcp__My_Search__My_Search_sample_tool");
}

/// `enabledTools` / `disabledTools` filter what the model sees, while
/// inspection still reports every advertised tool (v2
/// `computeEnabledNames` / `rawTools`).
#[tokio::test]
async fn test_tool_filter_limits_model_visibility() {
    let (sse_url, _shutdown) = spawn_mock_mcp_sse_server(None, None, None).await;
    let manager = McpManager::new();
    manager
        .set_tool_filter(
            "filtered",
            true,
            Some(vec!["calculate".to_string(), "echo".to_string()]),
            Some(vec!["echo".to_string()]),
        )
        .await;

    let client = McpClient::connect_sse("filtered", &sse_url, HashMap::new(), None)
        .await
        .expect("SSE client connect failed");
    manager.add_client(client).await;

    let infos = manager.list_tool_infos().await;
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].name, "mcp__filtered__calculate");
    assert!(manager.handles("mcp__filtered__calculate").await);
    assert!(!manager.handles("mcp__filtered__echo").await);

    let entries = manager.server_entries().await;
    assert_eq!(entries[0].tool_count, 1);
    assert_eq!(entries[0].tools.len(), 1);
    assert_eq!(entries[0].tools[0].name, "calculate");

    let inspected = manager
        .inspect_server("filtered")
        .await
        .expect("inspect failed");
    assert_eq!(
        inspected.len(),
        3,
        "inspection reports every advertised tool"
    );
}

/// A qualified tool name that duplicates another tool of the same server
/// is dropped (v2 `registerMcpServer` same-server collision,
/// mcpService.ts:186-215). "My Tool" and "My_Tool" both sanitize to
/// `mcp__srv__My_Tool`.
#[tokio::test]
async fn test_same_server_tool_collision_drops_losing_tool() {
    let manager = McpManager::new();
    let tools = vec![
        McpTool {
            name: "My Tool".into(),
            description: Some("first".into()),
            input_schema: json!({ "type": "object" }),
        },
        McpTool {
            name: "My_Tool".into(),
            description: Some("second".into()),
            input_schema: json!({ "type": "object" }),
        },
    ];
    manager
        .add_client(McpClient::mock_with_tools("srv", tools))
        .await;

    assert!(manager.handles("mcp__srv__My_Tool").await);
    let infos = manager.list_tool_infos().await;
    assert_eq!(infos.len(), 1, "the losing tool is dropped");
    assert_eq!(infos[0].description, "first", "the first registration wins");
}

/// A qualified tool name already registered by another server is dropped
/// (v2 `registerMcpServer` other-server collision). Server names that
/// sanitize identically ("My Server" vs "My_Server") collide on the same
/// qualified prefix.
#[tokio::test]
async fn test_cross_server_tool_collision_drops_losing_tool() {
    let manager = McpManager::new();
    let tools = vec![McpTool {
        name: "calculate".into(),
        description: Some("calc".into()),
        input_schema: json!({ "type": "object" }),
    }];
    manager
        .add_client(McpClient::mock_with_tools("My Server", tools.clone()))
        .await;
    manager
        .add_client(McpClient::mock_with_tools("My_Server", tools))
        .await;

    assert!(manager.handles("mcp__My_Server__calculate").await);
    let infos = manager.list_tool_infos().await;
    assert_eq!(infos.len(), 1, "the second server's duplicate is dropped");
    assert_eq!(infos[0].description, "calc");
}

/// Two servers exposing a tool with the same plain name both keep their
/// distinct qualified names, and each qualified call routes to its own
/// server (regression for the last-writer-wins plain alias).
#[tokio::test]
async fn test_same_tool_name_on_two_servers_routes_by_qualified_name() {
    let manager = McpManager::new();
    manager
        .add_client(McpClient::mock_with_tools(
            "alpha",
            vec![McpTool {
                name: "ping".into(),
                description: Some("alpha ping".into()),
                input_schema: json!({ "type": "object" }),
            }],
        ))
        .await;
    manager
        .add_client(McpClient::mock_with_tools(
            "beta",
            vec![McpTool {
                name: "ping".into(),
                description: Some("beta ping".into()),
                input_schema: json!({ "type": "object" }),
            }],
        ))
        .await;

    assert!(manager.handles("mcp__alpha__ping").await);
    assert!(manager.handles("mcp__beta__ping").await);
    let infos = manager.list_tool_infos().await;
    assert_eq!(infos.len(), 2, "both qualified tools are advertised");

    let alpha = manager
        .call_tool("mcp__alpha__ping", &json!({}), None)
        .await
        .expect("alpha call must resolve");
    assert!(alpha.note.is_none(), "note = {:?}", alpha.note);
    let beta = manager
        .call_tool("mcp__beta__ping", &json!({}), None)
        .await
        .expect("beta call must resolve");
    assert!(beta.note.is_none(), "note = {:?}", beta.note);
}

/// `enabled: false` keeps the server listed as `disabled` and never
/// connects it (v2 connection-manager.ts:193-204).
#[tokio::test]
async fn test_disabled_server_is_listed_without_connecting() {
    let mut configs = HashMap::new();
    configs.insert(
        "off".to_string(),
        crate::config::McpServerConfig {
            command: Some("definitely_missing_binary_404".into()),
            args: None,
            env: None,
            url: None,
            headers: None,
            enabled: Some(false),
            ..Default::default()
        },
    );

    let manager = std::sync::Arc::new(McpManager::new());
    let config = crate::config::KimiConfig {
        mcp_servers: configs,
        ..Default::default()
    };
    manager.spawn_from_config(&config).await;

    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "disabled");
    assert_eq!(entries[0].tool_count, 0);
    assert!(entries[0].error.is_none());
    assert!(!manager.handles("mcp__off__anything").await);
}

/// A dead client hides its tools and zeroes `toolCount` (v2
/// `watchForUnexpectedClose` + `toPublicEntry`).
#[tokio::test]
async fn test_closed_client_hides_tools_and_zeroes_count() {
    let manager = McpManager::new();
    manager.add_client(McpClient::mock("github")).await;
    assert_eq!(manager.list_tool_infos().await.len(), 1);

    let client = {
        let clients = manager.clients.read().await;
        clients.get("github").cloned().expect("client missing")
    };
    client.close().await;

    assert!(manager.list_tool_infos().await.is_empty());
    let entries = manager.server_entries().await;
    assert_eq!(entries[0].status, "failed");
    assert_eq!(entries[0].tool_count, 0);
    assert!(entries[0].tools.is_empty());
    assert_eq!(
        entries[0].error.as_deref(),
        Some("server closed unexpectedly")
    );
}

/// A stdio child that dies after the handshake must flip the entry to
/// `failed` and notify status listeners (v2 `watchForUnexpectedClose`,
/// connection-manager.ts:321-341).
#[tokio::test]
async fn test_unexpected_close_marks_failed_and_emits() {
    let init = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
    let list = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
    let dir = std::env::temp_dir();
    let (cmd, args, script) = if cfg!(windows) {
        let path = dir.join(format!("kimi_mcp_die_mgr_{}.bat", std::process::id()));
        // Only the first `set /p` is safe as a barrier: the client writes
        // `initialize` and then waits for its reply, so exactly one line
        // is in flight. After that reply it writes
        // `notifications/initialized` and `tools/list` back to back, and a
        // later `set /p` can swallow both — the third read then blocked
        // forever and the connect hit the 30s startup timeout (flaky under
        // a loaded suite). Waiting instead is deterministic: the wait
        // outlasts the client's own write, so `tools/list` is registered
        // before its reply is echoed and the reply can never be dropped.
        std::fs::write(
                &path,
                format!(
                    "@echo off\r\nset /p _=\r\n@echo {init}\r\n@ping -n 3 127.0.0.1 >nul\r\n@echo {list}\r\n@ping -n 2 127.0.0.1 >nul\r\n@exit /b 0\r\n"
                ),
            )
            .expect("write die script");
        (
            "cmd",
            vec!["/c".to_string(), path.to_string_lossy().into_owned()],
            path,
        )
    } else {
        let path = dir.join(format!("kimi_mcp_die_mgr_{}.sh", std::process::id()));
        // Answer each request as it arrives, then die: a script that echoes
        // both responses up front races the client's second write, and the
        // unmatched response is dropped, so the handshake never completes.
        std::fs::write(
            &path,
            format!(
                "read -r _; echo '{init}'\nread -r _\nread -r _; echo '{list}'\nsleep 1\nexit 0\n"
            ),
        )
        .expect("write die script");
        ("sh", vec![path.to_string_lossy().into_owned()], path)
    };

    let manager = McpManager::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let recorder = std::sync::Mutex::new(Some(tx));
    manager.on_status_change(Box::new(move |entry| {
        if entry.status == "failed"
            && let Some(tx) = recorder.lock().unwrap().take()
        {
            let _ = tx.send(entry);
        }
    }));

    manager
        .configure(
            "die",
            McpServerRecipe::Stdio {
                command: cmd.to_string(),
                args: args.clone(),
                env: HashMap::new(),
                cwd: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect("server connects");

    // The entry arrives only after the spawned process is reaped, so the
    // budget must cover process startup+exit under scheduler load (a 5s
    // window flaked when a second test suite ran concurrently).
    let entry = tokio::time::timeout(Duration::from_secs(30), rx)
        .await
        .expect("unexpected close must notify listeners")
        .expect("failed entry must be sent");
    assert_eq!(entry.status, "failed");
    assert!(
        entry
            .error
            .as_deref()
            .unwrap_or("")
            .contains("closed unexpectedly"),
        "unexpected error: {:?}",
        entry.error
    );
    assert_eq!(entry.tool_count, 0);

    let _ = std::fs::remove_file(script);
}

/// v2 precedence: per-server → env → `[mcp]` section → 30s default
/// (connection-manager.ts:310-314, 389-390).
#[test]
fn test_timeout_resolution_precedence() {
    assert_eq!(resolve_startup_timeout(Some(5), Some(6), Some(7)), 5);
    assert_eq!(resolve_startup_timeout(None, Some(6), Some(7)), 6);
    assert_eq!(resolve_startup_timeout(None, None, Some(7)), 7);
    assert_eq!(
        resolve_startup_timeout(None, None, None),
        DEFAULT_MCP_STARTUP_TIMEOUT_MS
    );

    assert_eq!(resolve_tool_timeout(Some(5), Some(6), Some(7)), Some(5));
    assert_eq!(resolve_tool_timeout(None, Some(6), Some(7)), Some(6));
    assert_eq!(resolve_tool_timeout(None, None, Some(7)), Some(7));
    assert_eq!(resolve_tool_timeout(None, None, None), None);
}

/// `[mcp]` defaults apply when the server declares no timeout of its own.
#[tokio::test]
async fn test_global_defaults_apply_to_servers_without_timeouts() {
    let manager = McpManager::new();
    manager.set_defaults(crate::config::McpConfig {
        startup_timeout_ms: Some(2_000),
        tool_timeout_ms: Some(3_000),
        ..Default::default()
    });
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");

    let timeouts = manager.timeouts.read().await;
    let resolved = timeouts.get("mock").expect("timeouts recorded");
    assert_eq!(resolved.startup, Duration::from_millis(2_000));
    assert_eq!(resolved.tool, Some(Duration::from_millis(3_000)));
}

/// `on_status_change` fans the public entry out on every status transition
/// (v2 `onStatusChange` + `emit`, connection-manager.ts:360-378): a mock
/// server goes `pending` on configure then `connected` after discovery.
#[tokio::test]
async fn test_on_status_change_fires_on_transitions() {
    let manager = McpManager::new();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<McpServerEntry>::new()));
    let recorder = seen.clone();
    let sub = manager.on_status_change(Box::new(move |entry| {
        recorder.lock().unwrap().push(entry);
    }));

    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");

    {
        let entries = seen.lock().unwrap();
        assert_eq!(entries.len(), 2, "pending then connected");
        assert_eq!(entries[0].name, "mock");
        assert_eq!(entries[0].status, "pending");
        assert_eq!(entries[1].name, "mock");
        assert_eq!(entries[1].status, "connected");
        assert_eq!(entries[1].tool_count, 1);
    }

    // Unsubscribing stops further notifications.
    assert!(manager.unsubscribe_status(sub));
    manager.mark_removed("mock").await;
    assert_eq!(seen.lock().unwrap().len(), 2, "no emit after unsubscribe");
}

/// The resolved tool timeout reaches the client (v2 passes
/// `toolCallTimeoutMs` into every client).
#[tokio::test]
async fn test_tool_timeout_reaches_client() {
    let manager = McpManager::new();
    manager
        .configure(
            "mock",
            McpServerRecipe::Mock,
            McpServerOptions {
                tool_timeout_ms: Some(1_500),
                ..Default::default()
            },
        )
        .await
        .expect("mock server connects");

    let client = {
        let clients = manager.clients.read().await;
        clients.get("mock").cloned().expect("client registered")
    };
    assert_eq!(client.tool_timeout(), Some(Duration::from_millis(1_500)));
}

/// A server that accepts the connection but never answers the handshake
/// must fail with the v2 timeout text instead of hanging the caller
/// (connection-manager.ts:594-611).
#[tokio::test]
async fn test_startup_timeout_marks_failed() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Accept and hold every connection open without ever responding.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });

    let manager = McpManager::new();
    // This test is about the connect-failure status; the startup retry has its
    // own tests.
    manager.set_defaults(crate::config::McpConfig {
        reconnect_retry_enabled: Some(false),
        ..Default::default()
    });
    let err = manager
        .configure(
            "hanging",
            McpServerRecipe::Sse {
                url: format!("http://{addr}/sse"),
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions {
                startup_timeout_ms: Some(150),
                ..Default::default()
            },
        )
        .await
        .expect_err("handshake must time out");
    assert_eq!(err, "Timed out after 150ms");

    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "failed");
    assert_eq!(entries[0].error.as_deref(), Some("Timed out after 150ms"));
    assert_eq!(entries[0].tool_count, 0);
}

/// v2 `rescheduleRetries` (connection-manager.ts:501-505): a policy change
/// moves the due time of a pending retry and keeps its streak, so tightening
/// the knob takes effect instead of waiting out the old schedule.
#[tokio::test]
async fn rescheduling_retries_moves_the_due_time_and_keeps_the_streak() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager.set_defaults(crate::config::McpConfig {
        reconnect_retry_enabled: Some(true),
        reconnect_initial_delay_ms: Some(60_000),
        reconnect_max_delay_ms: Some(60_000),
        ..Default::default()
    });
    manager.start_retry_ticker();
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock connects");
    manager
        .mark_connect_failure("mock", &McpError::transport("boom"), "boom")
        .await;
    let before = manager
        .server_entries()
        .await
        .into_iter()
        .find(|entry| entry.name == "mock")
        .and_then(|entry| entry.next_retry_at);
    assert!(before.is_some(), "a transient failure schedules a retry");

    // The host tightens the knobs; the already-scheduled wait moves with it.
    manager.set_defaults(crate::config::McpConfig {
        reconnect_retry_enabled: Some(true),
        reconnect_initial_delay_ms: Some(1),
        reconnect_max_delay_ms: Some(1),
        ..Default::default()
    });
    manager.reschedule_retries().await;
    let after = manager
        .server_entries()
        .await
        .into_iter()
        .find(|entry| entry.name == "mock")
        .and_then(|entry| entry.next_retry_at);
    assert!(
        after.is_some_and(|after| before.is_some_and(|before| after < before)),
        "the rescheduled retry must be due earlier: {before:?} -> {after:?}"
    );
    manager.shutdown().await;
}

/// v2 "isolates permanent startup and discovery failures without retry or
/// blocking the rest": a server that can never start lands `failed` with no
/// retry, and its neighbour still connects in the same load.
#[tokio::test]
async fn a_permanent_failure_does_not_block_the_other_servers() {
    let manager = std::sync::Arc::new(McpManager::new());
    let handle = manager.connect_all(vec![
        (
            "broken".to_string(),
            McpServerRecipe::Stdio {
                command: "definitely_nonexistent_command_9999".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
            },
            McpServerOptions::default(),
        ),
        (
            "good".to_string(),
            McpServerRecipe::Mock,
            McpServerOptions::default(),
        ),
    ]);
    handle.await.expect("connect_all joins its tasks");

    let entries = manager.server_entries().await;
    let good = entries
        .iter()
        .find(|entry| entry.name == "good")
        .expect("the healthy server is listed");
    assert_eq!(good.status, "connected");
    assert_eq!(good.tool_count, 1);
    let broken = entries
        .iter()
        .find(|entry| entry.name == "broken")
        .expect("the failing server is listed");
    assert_eq!(broken.status, "failed");
    assert!(
        broken.next_retry_at.is_none(),
        "a command that cannot spawn is permanent — retrying repeats the error"
    );
    manager.shutdown().await;
}

/// v2 "markRemoved tombstones the entry" (connection-manager.test.ts:195-241):
/// the entry stays listed as `removed` with no tools, `reconnect` refuses it
/// (it resolves through the live view, hence "Unknown MCP server"), a missing
/// name reports `false`, and a fresh `configure` revives it.
#[tokio::test]
async fn mark_removed_tombstones_the_entry_until_a_fresh_connect() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock connects");
    assert_eq!(status_of(&manager, "mock").await, "connected");
    assert!(!manager.server_entries().await[0].tools.is_empty());

    assert!(manager.mark_removed("mock").await);
    let entries = manager.server_entries().await;
    let entry = entries
        .iter()
        .find(|entry| entry.name == "mock")
        .expect("a tombstone stays listed");
    assert_eq!(entry.status, "removed");
    assert_eq!(entry.tool_count, 0);
    assert!(manager.clients.read().await.get("mock").is_none());

    let err = manager
        .reconnect("mock")
        .await
        .expect_err("a tombstone is not reconnectable");
    assert_eq!(err, "Unknown MCP server: mock");
    assert_eq!(status_of(&manager, "mock").await, "removed");
    assert!(!manager.mark_removed("missing").await);

    // A fresh configuration is what revives it (v2 `connect`/`upsert`).
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("a fresh connect revives the server");
    assert_eq!(status_of(&manager, "mock").await, "connected");
    manager.shutdown().await;
}

/// v2 "markRemoved … in-flight connects invalidated": a handshake that lands
/// after the removal is discarded — the entry keeps its tombstone and no
/// client is registered.
#[tokio::test]
async fn a_handshake_that_lands_after_removal_does_not_revive_the_entry() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("slow-initialize").await;
    let manager = std::sync::Arc::new(McpManager::new());
    let connecting = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .configure(
                    "slow",
                    McpServerRecipe::Http {
                        url,
                        headers: HashMap::new(),
                        bearer_token_env_var: None,
                    },
                    McpServerOptions {
                        startup_timeout_ms: Some(5_000),
                        ..Default::default()
                    },
                )
                .await
        })
    };
    // The mock holds `initialize` for 300 ms; the removal lands inside it.
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(manager.mark_removed("slow").await);

    let outcome = connecting.await.expect("the connect task joins");
    assert!(
        outcome.is_err(),
        "the discarded attempt reports why it did not register"
    );
    assert_eq!(status_of(&manager, "slow").await, "removed");
    assert!(
        manager.clients.read().await.get("slow").is_none(),
        "the late handshake must not register a client for a tombstone"
    );
    manager.shutdown().await;
}

/// v2 `withTimeout` wraps connect **and** discovery
/// (connection-manager.ts:594-611): a server that answers `initialize` but
/// never returns `tools/list` must fail on the startup budget instead of
/// hanging until the caller gives up.
#[tokio::test]
async fn a_hanging_tools_list_fails_on_the_startup_budget() {
    let (url, seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("hang-tools").await;
    let manager = McpManager::new();
    manager.set_defaults(crate::config::McpConfig {
        reconnect_retry_enabled: Some(false),
        ..Default::default()
    });
    let started = std::time::Instant::now();
    let err = manager
        .configure(
            "slow-tools",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions {
                startup_timeout_ms: Some(150),
                ..Default::default()
            },
        )
        .await
        .expect_err("discovery must not outlive the startup budget");
    assert_eq!(err, "Timed out after 150ms");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the startup budget, not the caller's patience, must end the connect"
    );
    // The handshake itself succeeded — discovery is the half that timed out.
    let methods: Vec<String> = seen
        .lock()
        .await
        .iter()
        .map(|request| request.method.clone())
        .collect();
    assert!(methods.iter().any(|method| method == "initialize"));
    assert!(methods.iter().any(|method| method == "tools/list"));
}

/// A Streamable HTTP server is discovered through the `http` recipe
/// (v2 `HttpMcpClient`).
#[tokio::test]
async fn test_http_recipe_discovers_tools() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("json").await;
    let manager = McpManager::new();
    manager
        .configure(
            "http-srv",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect("HTTP server connects");

    assert!(manager.handles("mcp__http-srv__echo").await);
    let entries = manager.server_entries().await;
    assert_eq!(entries[0].transport, "http");
    assert_eq!(entries[0].status, "connected");
    assert_eq!(entries[0].tool_count, 1);

    let res = manager
        .call_tool("mcp__http-srv__echo", &json!({}), None)
        .await
        .expect("tool call failed");
    assert!(!res.is_error);
    assert_eq!(res.content, "ok");
}

/// A bare `url` defaults to Streamable HTTP; legacy SSE needs an explicit
/// transport (v2 config preprocess, config-schema.ts:58-65).
#[test]
fn test_recipe_from_config_transport_defaults() {
    let url_only = crate::config::McpServerConfig {
        url: Some("http://example.test/mcp".into()),
        ..Default::default()
    };
    assert!(matches!(
        recipe_from_config(&url_only),
        Some(McpServerRecipe::Http { .. })
    ));

    let explicit_sse = crate::config::McpServerConfig {
        url: Some("http://example.test/sse".into()),
        transport: Some("sse".into()),
        ..Default::default()
    };
    assert!(matches!(
        recipe_from_config(&explicit_sse),
        Some(McpServerRecipe::Sse { .. })
    ));

    let command_only = crate::config::McpServerConfig {
        command: Some("mcp-server".into()),
        ..Default::default()
    };
    assert!(matches!(
        recipe_from_config(&command_only),
        Some(McpServerRecipe::Stdio { .. })
    ));

    let unknown = crate::config::McpServerConfig {
        url: Some("http://example.test/mcp".into()),
        transport: Some("carrier-pigeon".into()),
        ..Default::default()
    };
    assert!(recipe_from_config(&unknown).is_none());
}

/// `shutdown` closes every client and forgets all entries
/// (v2 `shutdown`, connection-manager.ts:303-308).
#[tokio::test]
async fn test_shutdown_closes_every_server() {
    let manager = McpManager::new();
    manager.add_client(McpClient::mock("alpha")).await;
    manager.add_client(McpClient::mock("beta")).await;
    let clients: Vec<Arc<McpClient>> = {
        let guard = manager.clients.read().await;
        guard.values().cloned().collect()
    };
    assert_eq!(clients.len(), 2);
    assert_eq!(manager.server_entries().await.len(), 2);

    manager.shutdown().await;

    assert!(manager.server_entries().await.is_empty());
    assert!(manager.list_tool_infos().await.is_empty());
    for client in clients {
        assert!(client.is_closed(), "shutdown must close every client");
    }
}

/// `mark_removed` keeps the entry visible with status `removed` and drops
/// its tools (v2 `markRemoved`, connection-manager.ts:221-232).
#[tokio::test]
async fn test_mark_removed_keeps_entry_without_tools() {
    let manager = McpManager::new();
    manager
        .configure("gone", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");
    assert!(manager.handles("mcp__gone__gone_sample_tool").await);
    assert!(manager.mark_removed("gone").await);

    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "removed");
    assert_eq!(entries[0].tool_count, 0);
    assert!(entries[0].tools.is_empty());
    assert!(!manager.handles("mcp__gone__gone_sample_tool").await);
    assert!(!manager.mark_removed("never-configured").await);
}

/// A tool whose inputSchema is not a JSON object fails the whole server
/// (v2 `assertMcpInputSchema`, mcpCore/types.ts:44-55).
#[tokio::test]
async fn test_invalid_input_schema_fails_the_server() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("bad-schema").await;
    let manager = McpManager::new();
    let err = manager
        .configure(
            "bad-schema",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("a non-object inputSchema must fail the server");
    assert_eq!(
        err,
        "Invalid inputSchema for MCP tool \"echo\": schema must be a JSON object"
    );

    let entries = manager.server_entries().await;
    assert_eq!(entries[0].status, "failed");
    assert_eq!(entries[0].error.as_deref(), Some(err.as_str()));
    assert!(!manager.handles("mcp__bad-schema__echo").await);
}

/// `bearerTokenEnvVar` resolves to an `Authorization: Bearer …` header at
/// connect time, replacing any static Authorization header
/// (v2 `buildMcpRemoteHeaders`, client-remote.ts:9-23).
#[tokio::test]
async fn test_bearer_token_env_var_is_sent() {
    let var = format!("KIMI_TEST_MCP_BEARER_{}", std::process::id());
    // SAFETY: the variable name is unique to this test process and no
    // other test reads it.
    unsafe { std::env::set_var(&var, "s3cret") };

    let (url, seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("json").await;
    let manager = McpManager::new();
    manager
        .configure(
            "auth",
            McpServerRecipe::Http {
                url,
                headers: HashMap::from([("Authorization".to_string(), "Bearer stale".to_string())]),
                bearer_token_env_var: Some(var.clone()),
            },
            McpServerOptions::default(),
        )
        .await
        .expect("bearer-token server connects");

    let requests = seen.lock().await.clone();
    assert!(!requests.is_empty());
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer s3cret"),
            "the resolved token replaces the static Authorization header"
        );
    }

    // SAFETY: see above.
    unsafe { std::env::remove_var(&var) };
}

/// A missing or empty bearer token variable fails the server with the v2
/// message (client-remote.ts:12-16).
#[tokio::test]
async fn test_missing_bearer_token_env_var_fails_the_server() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("json").await;
    let manager = McpManager::new();
    let err = manager
        .configure(
            "auth",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: Some("KIMI_TEST_MCP_MISSING_BEARER".into()),
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("a missing token must fail the server");
    assert_eq!(
        err,
        "MCP HTTP bearer token env var \"KIMI_TEST_MCP_MISSING_BEARER\" is not set or is empty"
    );
    let entries = manager.server_entries().await;
    assert_eq!(entries[0].status, "failed");
    assert_eq!(entries[0].error.as_deref(), Some(err.as_str()));
}

/// A 401 marks the server `needs-auth` instead of `failed`, and an OAuth
/// credential from the store is injected as a bearer token
/// (v2 `shouldMarkNeedsAuth` + `resolveOAuthProvider`).
#[tokio::test]
async fn test_oauth_token_injection_and_needs_auth_status() {
    // 1. No credentials + 401 → needs-auth. The OAuth service must be
    // installed for the flip (v2 `shouldMarkNeedsAuth` returns false when
    // `oauthService === undefined`).
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401").await;
    let manager = McpManager::new();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mcp::oauth::McpOAuthFileStore::new(dir.path()));
    manager.set_oauth_service(Arc::new(crate::mcp::oauth::McpOAuthService::new(store)));
    manager
        .configure(
            "auth-srv",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("401 must fail the connect");
    let entries = manager.server_entries().await;
    assert_eq!(entries[0].status, "needs-auth");

    // 2. With stored credentials the token is sent.
    let (url, seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("json").await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mcp::oauth::McpOAuthFileStore::new(dir.path()));
    let key = crate::mcp::oauth::mcp_oauth_store_key("oauth-srv", &url).unwrap();
    store
        .write(
            &key,
            &crate::mcp::oauth::McpOAuthTokens {
                access_token: "oauth-token".into(),
                refresh_token: None,
                expires_at_ms: None,
                token_endpoint: None,
                client_id: None,
                client_secret: None,
                resource: None,
                obtained_at_ms: None,
            },
        )
        .unwrap();
    manager.set_oauth_service(Arc::new(crate::mcp::oauth::McpOAuthService::new(store)));
    manager
        .configure(
            "oauth-srv",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect("token-authenticated server connects");

    let requests = seen.lock().await.clone();
    assert!(!requests.is_empty());
    assert_eq!(
        requests[0].authorization.as_deref(),
        Some("Bearer oauth-token")
    );
}

/// v2's two remaining `needs-auth` edges (`shouldMarkNeedsAuth` +
/// `peekRejectedGrant`):
///
/// - without an OAuth service a 401 is `failed`, not `needs-auth` — no login
///   could supply a token, so "run /mcp-config login" would be a lie;
/// - a stored grant the server rejects flips the entry to `needs-auth` **and**
///   drops the stale record, so the next connect does not replay it
///   (v2 "flips cached OAuth credentials that require reauth into needs-auth").
#[tokio::test]
async fn a_401_without_oauth_fails_and_a_rejected_grant_flips_to_needs_auth() {
    // 1. No OAuth service installed: the 401 stays a plain failure.
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401").await;
    let manager = McpManager::new();
    manager
        .configure(
            "plain",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("401 must fail the connect");
    assert_eq!(
        status_of(&manager, "plain").await,
        "failed",
        "without an OAuth service the 401 cannot be repaired by logging in"
    );

    // 2. A stored grant the server rejects: the startup path lands needs-auth
    //    with the login hint. v2's startup transition (`failureTarget` →
    //    `needsAuth`, connection-manager.ts:594) does **not** invalidate the
    //    stored grant — only the runtime `markNeedsAuth` path does, through
    //    `peekRejectedGrant` + `invalidateTokensIfCurrent` (:452-473). The
    //    fork keeps the same asymmetry; the tool-call test below covers the
    //    invalidating path.
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401").await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mcp::oauth::McpOAuthFileStore::new(dir.path()));
    let key = crate::mcp::oauth::mcp_oauth_store_key("stale", &url).unwrap();
    store
        .write(
            &key,
            &crate::mcp::oauth::McpOAuthTokens {
                access_token: "stale-token".into(),
                refresh_token: Some("stale-refresh".into()),
                expires_at_ms: None,
                token_endpoint: None,
                client_id: None,
                client_secret: None,
                resource: None,
                obtained_at_ms: Some(chrono::Utc::now().timestamp_millis() - 60_000),
            },
        )
        .unwrap();
    manager.set_oauth_service(Arc::new(crate::mcp::oauth::McpOAuthService::new(
        store.clone(),
    )));
    manager
        .configure(
            "stale",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("a rejected grant fails the connect");
    let entries = manager.server_entries().await;
    let entry = entries
        .iter()
        .find(|entry| entry.name == "stale")
        .expect("the entry is listed");
    assert_eq!(entry.status, "needs-auth");
    assert!(
        entry
            .error
            .as_deref()
            .is_some_and(|error| error.contains("run /mcp-config login stale")),
        "the hint names the server: {:?}",
        entry.error
    );
    assert!(
        store
            .read::<crate::mcp::oauth::McpOAuthTokens>(&key)
            .is_some(),
        "v2 keeps the rejectable grant on the startup path; only markNeedsAuth clears it"
    );
}

/// v2 client-http.test.ts "strips case-variant authorization headers before
/// injecting the bearer": HTTP header names are case-insensitive, so a static
/// `authorization` must not survive next to the injected `Authorization`.
#[test]
fn a_bearer_env_token_strips_case_variant_authorization_headers() {
    let var = format!("KIMI_TEST_MCP_BEARER_CASE_{}", std::process::id());
    // SAFETY: the variable name is unique to this test process and no other
    // test reads it.
    unsafe { std::env::set_var(&var, "s3cret") };
    let headers = HashMap::from([
        ("Authorization".to_string(), "Bearer upper".to_string()),
        ("authorization".to_string(), "Bearer lower".to_string()),
        ("X-Api-Key".to_string(), "kept".to_string()),
    ]);
    let resolved = resolve_bearer_headers("HTTP", &headers, Some(var.as_str()))
        .expect("a set env var resolves");
    let authorizations: Vec<&String> = resolved
        .keys()
        .filter(|key| key.eq_ignore_ascii_case("authorization"))
        .collect();
    assert_eq!(
        authorizations.len(),
        1,
        "exactly one Authorization header survives"
    );
    assert_eq!(authorizations[0].as_str(), "Authorization");
    assert_eq!(
        resolved.get("Authorization").map(String::as_str),
        Some("Bearer s3cret")
    );
    assert_eq!(resolved.get("X-Api-Key").map(String::as_str), Some("kept"));
    // SAFETY: see above.
    unsafe { std::env::remove_var(&var) };
}

/// A 401 on a *tool call* — the handshake already succeeded — flips the
/// server to `needs-auth`, drops its tools, clears the rejected grant and
/// hands the caller an actionable message (v2 `markNeedsAuth` from the
/// tool wrapper, connection-manager.ts:307-347 + tools/mcp.ts:78-91, #3846).
#[tokio::test]
async fn test_tool_call_401_flips_server_to_needs_auth() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401-on-call").await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mcp::oauth::McpOAuthFileStore::new(dir.path()));
    let key = crate::mcp::oauth::mcp_oauth_store_key("stale-srv", &url).unwrap();
    // Obtained well before this connection, so it is the credential the
    // 401 rejects rather than a login landing alongside the connect.
    store
        .write(
            &key,
            &crate::mcp::oauth::McpOAuthTokens {
                access_token: "stale-token".into(),
                refresh_token: None,
                expires_at_ms: None,
                token_endpoint: None,
                client_id: None,
                client_secret: None,
                resource: None,
                obtained_at_ms: Some(chrono::Utc::now().timestamp_millis() - 60_000),
            },
        )
        .unwrap();
    let manager = McpManager::new();
    manager.set_oauth_service(Arc::new(crate::mcp::oauth::McpOAuthService::new(
        store.clone(),
    )));
    manager
        .configure(
            "stale-srv",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect("the handshake succeeds even though every call is rejected");
    assert!(manager.handles("mcp__stale-srv__echo").await);

    let result = manager
        .call_tool("mcp__stale-srv__echo", &json!({}), None)
        .await
        .expect("a failed call still returns a tool result");

    assert!(result.is_error);
    assert!(
        result.content.contains("401 Unauthorized"),
        "unexpected content: {}",
        result.content
    );
    assert!(
        result.content.contains("/mcp-config login stale-srv"),
        "the message must name the remedy: {}",
        result.content
    );

    let entries = manager.server_entries().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].status, "needs-auth");
    assert_eq!(entries[0].tool_count, 0);
    assert!(
        !manager.handles("mcp__stale-srv__echo").await,
        "a needs-auth server must stop advertising tools"
    );
    assert!(
        store
            .read::<crate::mcp::oauth::McpOAuthTokens>(&key)
            .is_none(),
        "the rejected grant must not be replayed on the next connect"
    );

    // A second call finds no tool at all: the flip dropped both the client
    // and the cached tool, so the model cannot retry into the dead server.
    assert!(
        manager
            .call_tool("mcp__stale-srv__echo", &json!({}), None)
            .await
            .is_none()
    );
    assert_eq!(manager.server_entries().await[0].status, "needs-auth");
}

/// A grant obtained moments ago — at or after this connection — is a login
/// landing concurrently, so the old connection's 401 must neither flip the
/// entry nor invalidate that credential (v2 `isConcurrentGrant`).
#[tokio::test]
async fn test_concurrent_grant_survives_a_call_401() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401-on-call").await;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(crate::mcp::oauth::McpOAuthFileStore::new(dir.path()));
    let key = crate::mcp::oauth::mcp_oauth_store_key("racing-srv", &url).unwrap();
    let manager = McpManager::new();
    manager.set_oauth_service(Arc::new(crate::mcp::oauth::McpOAuthService::new(
        store.clone(),
    )));
    manager
        .configure(
            "racing-srv",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect("an unauthenticated handshake still succeeds");
    assert!(manager.handles("mcp__racing-srv__echo").await);

    // The user completes the OAuth login while this connection is live.
    store
        .write(
            &key,
            &crate::mcp::oauth::McpOAuthTokens {
                access_token: "fresh-token".into(),
                refresh_token: None,
                expires_at_ms: None,
                token_endpoint: None,
                client_id: None,
                client_secret: None,
                resource: None,
                obtained_at_ms: Some(chrono::Utc::now().timestamp_millis()),
            },
        )
        .unwrap();

    let result = manager
        .call_tool("mcp__racing-srv__echo", &json!({}), None)
        .await
        .expect("a failed call still returns a tool result");

    assert!(result.is_error);
    assert!(
        result.content.starts_with("MCP execution error:"),
        "a concurrent login must not be reported as needs-auth: {}",
        result.content
    );
    let entries = manager.server_entries().await;
    assert_eq!(entries[0].status, "connected");
    assert!(
        manager.handles("mcp__racing-srv__echo").await,
        "the tool list must survive a rejected call"
    );
    assert_eq!(
        store
            .read::<crate::mcp::oauth::McpOAuthTokens>(&key)
            .map(|t| t.access_token),
        Some("fresh-token".to_string()),
        "a concurrent grant must not be invalidated"
    );
}

/// An image an MCP server returns is preserved into the attachment store
/// and handed to the model as a reference the resolver can inline, instead
/// of being flattened into a text preview (v2 #3688).
#[tokio::test]
async fn test_mcp_media_is_preserved_and_delivered_as_a_reference() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("image").await;
    let dir = tempfile::tempdir().unwrap();
    let store = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let manager = McpManager::new();
    manager.set_attachment_store(store.clone()).await;
    manager
        .configure(
            "media-srv",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect("connect succeeds");

    let result = manager
        .call_tool("mcp__media-srv__echo", &json!({}), None)
        .await
        .expect("the call returns a result");

    assert!(
        result.content.contains("Original attachment saved at:"),
        "the notice must name the preserved original: {}",
        result.content
    );
    assert!(
        result
            .parts
            .iter()
            .any(|part| matches!(part, crate::rpc::types::ContentBlock::MediaRef { .. })),
        "the image must ride inside the result parts: {:?}",
        result.parts
    );
    assert_eq!(
        store.list().unwrap().len(),
        1,
        "the original must be on disk"
    );
}

/// v2 `shouldMarkNeedsAuth` decision matrix (connection-manager.ts:383-393):
/// only remote servers without a static credential flip to `needs-auth`,
/// and only on a classified HTTP 401 (never on message text sniffing).
#[test]
fn test_should_mark_needs_auth_matrix() {
    let http = McpServerRecipe::Http {
        url: "http://example.test/mcp".into(),
        headers: HashMap::new(),
        bearer_token_env_var: None,
    };
    let stdio = McpServerRecipe::Stdio {
        command: "mcp-server".into(),
        args: Vec::new(),
        env: HashMap::new(),
        cwd: None,
    };
    let with_headers = McpServerRecipe::Http {
        url: "http://example.test/mcp".into(),
        headers: HashMap::from([("X-Api-Key".to_string(), "k".to_string())]),
        bearer_token_env_var: None,
    };
    let with_bearer = McpServerRecipe::Http {
        url: "http://example.test/mcp".into(),
        headers: HashMap::new(),
        bearer_token_env_var: Some("KIMI_MCP_TOKEN".into()),
    };

    // No OAuth service installed → never needs-auth.
    assert!(!should_mark_needs_auth(&http, false, false, true));
    // stdio servers never participate in the OAuth flow.
    assert!(!should_mark_needs_auth(&stdio, false, true, true));
    // A pinned static credential means the 401 is a bad header.
    assert!(!should_mark_needs_auth(&with_headers, false, true, true));
    assert!(!should_mark_needs_auth(&with_bearer, false, true, true));
    // Remote without credentials + a classified 401 → needs-auth.
    assert!(should_mark_needs_auth(&http, false, true, true));
    // Non-401 failures (500, connection refused, timeout, …) stay failed,
    // even if their text happens to contain "401" or "Unauthorized".
    assert!(!should_mark_needs_auth(&http, false, true, false));
    // v2 `config.headers !== undefined && config.auth !== 'oauth'`: an
    // explicit `auth = "oauth"` says the extra headers are unrelated to the
    // credential, so the 401 still means "log in".
    assert!(should_mark_needs_auth(&with_headers, true, true, true));
    // A bearer env var is a static credential either way — no flip.
    assert!(!should_mark_needs_auth(&with_bearer, true, true, true));
}

/// A static `headers` block on a 401 server must surface `failed`, not
/// `needs-auth` — the real error is a bad header, not a missing OAuth
/// token (v2 `shouldMarkNeedsAuth` headers exemption).
#[tokio::test]
async fn test_static_headers_401_is_failed_not_needs_auth() {
    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401").await;
    let manager = McpManager::new();
    manager
        .configure(
            "static-headers",
            McpServerRecipe::Http {
                url,
                headers: HashMap::from([("X-Api-Key".to_string(), "k".to_string())]),
                bearer_token_env_var: None,
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("401 must fail the connect");
    let entries = manager.server_entries().await;
    assert_eq!(
        entries[0].status, "failed",
        "static headers must not flip the entry to needs-auth"
    );
}

/// A pinned `bearerTokenEnvVar` on a 401 server must surface `failed`, not
/// `needs-auth` (v2 `shouldMarkNeedsAuth` bearer exemption).
#[tokio::test]
async fn test_bearer_env_var_401_is_failed_not_needs_auth() {
    let var = format!("KIMI_TEST_MCP_BEARER_401_{}", std::process::id());
    // SAFETY: the variable name is unique to this test process and no
    // other test reads it.
    unsafe { std::env::set_var(&var, "bad-token") };

    let (url, _seen, _shutdown) =
        crate::mcp::http::test_helpers::spawn_mock_http_server("401").await;
    let manager = McpManager::new();
    manager
        .configure(
            "bearer-401",
            McpServerRecipe::Http {
                url,
                headers: HashMap::new(),
                bearer_token_env_var: Some(var.clone()),
            },
            McpServerOptions::default(),
        )
        .await
        .expect_err("401 must fail the connect");
    let entries = manager.server_entries().await;
    assert_eq!(
        entries[0].status, "failed",
        "a pinned bearer token must not flip the entry to needs-auth"
    );

    // SAFETY: see above.
    unsafe { std::env::remove_var(&var) };
}

/// v2 #4210: a server that declares no `deferred` inherits the `[mcp]
/// defaultDeferred` section value; an explicit per-server value wins over it.
#[tokio::test]
async fn test_default_deferred_applies_and_a_per_server_value_wins() {
    let manager = McpManager::new();
    manager.set_defaults(crate::config::McpConfig {
        default_deferred: Some(true),
        ..Default::default()
    });
    manager
        .configure(
            "inherit",
            McpServerRecipe::Mock,
            McpServerOptions::default(),
        )
        .await
        .unwrap();
    manager.add_client(McpClient::mock("inherit")).await;
    manager
        .configure(
            "explicit",
            McpServerRecipe::Mock,
            McpServerOptions {
                deferred: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    manager.add_client(McpClient::mock("explicit")).await;

    let deferred = manager.deferred_tool_names().await;
    assert!(
        deferred
            .iter()
            .any(|name| name.starts_with("mcp__inherit__")),
        "the section default must mark an undeclared server deferred: {deferred:?}"
    );
    assert!(
        !deferred
            .iter()
            .any(|name| name.starts_with("mcp__explicit__")),
        "an explicit per-server false must beat the section default: {deferred:?}"
    );
}

/// v2 `McpConnectionManagerOptions.stdioCwd`: a stdio server whose config omits
/// `cwd` starts in the manager's default — the workspace — and an explicit
/// `cwd` still wins over it.
#[test]
fn the_default_stdio_cwd_fills_a_missing_one() {
    let manager = McpManager::new();
    assert_eq!(manager.stdio_cwd_for(None), None, "no default, no cwd");
    manager.set_default_stdio_cwd("workspace-root");
    assert_eq!(
        manager.stdio_cwd_for(None).as_deref(),
        Some("workspace-root")
    );
    assert_eq!(
        manager.stdio_cwd_for(Some("explicit-dir")).as_deref(),
        Some("explicit-dir"),
        "an explicit cwd always wins"
    );
}

async fn status_of(manager: &McpManager, name: &str) -> String {
    manager
        .server_entries()
        .await
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.status)
        .unwrap_or_default()
}

/// v2 `failureTarget` + `retryOrFailed`: a transient startup failure schedules
/// a retry and a due retry walks the entry back through the connect attempt
/// into `connected`, clearing the streak; a configuration failure never
/// retries. The scheduled entry is public `failed` plus `nextRetryAt`
/// (v2 `publicStatusByState`), never a status of its own.
#[tokio::test]
async fn a_transient_startup_failure_schedules_a_retry() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager.set_defaults(crate::config::McpConfig {
        reconnect_initial_delay_ms: Some(1),
        reconnect_max_delay_ms: Some(1),
        ..Default::default()
    });
    // A retrying entry needs the scheduler that acts on its due time: without
    // it the policy reports `failed` on purpose.
    manager.start_retry_ticker();
    manager
        .configure("flaky", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");
    assert_eq!(status_of(&manager, "flaky").await, "connected");

    manager
        .mark_connect_failure(
            "flaky",
            &McpError::timeout("Timed out after 100ms"),
            "Timed out after 100ms",
        )
        .await;
    let entry = manager
        .server_entries()
        .await
        .into_iter()
        .find(|entry| entry.name == "flaky")
        .expect("the entry is resident");
    assert_eq!(entry.status, "failed");
    assert!(entry.next_retry_at.is_some(), "a retry time is public");
    {
        let servers = manager.servers.read().await;
        let state = servers.get("flaky").expect("the entry is resident");
        assert_eq!(state.status, "retry-waiting");
        assert!(state.retry_at_ms.is_some(), "a retry time is scheduled");
        assert_eq!(state.retry_streak, 1);
    }

    // The 1 ms delay jitters down to 0, so the entry is already due.
    manager.retry_due_servers().await;
    let entry = manager
        .server_entries()
        .await
        .into_iter()
        .find(|entry| entry.name == "flaky")
        .expect("the entry is resident");
    assert_eq!(entry.status, "connected");
    assert!(
        entry.next_retry_at.is_none(),
        "a live entry has no retry time"
    );
    {
        let servers = manager.servers.read().await;
        let state = servers.get("flaky").expect("the entry is resident");
        assert_eq!(state.retry_streak, 0, "a successful connect resets it");
        assert!(state.retry_at_ms.is_none());
    }
    manager.shutdown().await;
}

/// v2 suppresses the status events of the retry cycle itself: a due retry does
/// not flash `pending` (`retryDue`, connection-manager.ts:600-601) and a retry
/// that fails again does not re-emit `failed` — the public entry was already
/// failed. A listener sees `pending, connected, failed, connected`, which is
/// exactly what the v2 weak-evidence test asserts.
#[tokio::test]
async fn an_automatic_retry_does_not_re_emit_pending_or_failed() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager.set_defaults(crate::config::McpConfig {
        reconnect_initial_delay_ms: Some(1),
        reconnect_max_delay_ms: Some(1),
        ..Default::default()
    });
    manager.start_retry_ticker();
    manager
        .configure("flaky", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _subscription = manager.on_status_change(Box::new(move |entry| {
        let _ = tx.send(entry);
    }));

    let failure = McpError::timeout("Timed out after 100ms");
    manager
        .mark_connect_failure("flaky", &failure, "Timed out after 100ms")
        .await;
    let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("the first failure is reported")
        .expect("a status event");
    assert_eq!(first.status, "failed");

    // A second failure lands in retry-waiting again; the public entry stays
    // `failed`, so v2 emits nothing.
    manager
        .mark_connect_failure("flaky", &failure, "Timed out after 100ms")
        .await;
    // The 1 ms delay jitters down to 0: the retry is due now. Its transition
    // to `pending` is suppressed as well, so only the connect outcome shows.
    manager.retry_due_servers().await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("the recovery is reported")
        .expect("a status event");
    assert_eq!(second.status, "connected");
    assert!(
        rx.try_recv().is_err(),
        "no other transition may be reported"
    );
    manager.shutdown().await;
}

/// A command that cannot be spawned is a configuration failure (v2
/// `McpFailureKind` `config`): retrying it would only repeat the same error.
#[tokio::test]
async fn a_config_failure_does_not_schedule_a_retry() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager
        .configure("bad", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");

    manager
        .mark_connect_failure(
            "bad",
            &McpError::Config("Failed to spawn MCP server 'nope': not found".into()),
            "Failed to spawn MCP server 'nope': not found",
        )
        .await;

    assert_eq!(status_of(&manager, "bad").await, "failed");
    let servers = manager.servers.read().await;
    assert!(
        servers
            .get("bad")
            .expect("the entry is resident")
            .retry_at_ms
            .is_none(),
        "a permanent failure must not schedule a retry"
    );
}

/// v2 `retryOrFailed` + `nextRetryDelayMs`: a transient failure retries only
/// when the host enabled the retry **and** a scheduler exists to act on the
/// due time; otherwise it fails outright. The streak grows by one per failure.
#[test]
fn the_retry_policy_decides_between_retry_waiting_and_failed() {
    let policy = |enabled, ticker| RetryPolicy {
        enabled,
        ticker,
        initial_delay_ms: 5_000,
        max_delay_ms: 300_000,
    };
    let now = 1_000_000;

    let (status, due, streak) = next_failure_state(policy(true, true), 0, now);
    assert_eq!(status, "retry-waiting");
    assert_eq!(streak, 1);
    let delay = due.expect("a due time") - now;
    assert!(
        (2_500..=5_000).contains(&delay),
        "the first retry waits 5 s with 50-100 % jitter, got {delay}"
    );

    // A second failure doubles the nominal delay and keeps counting.
    let (_, due, streak) = next_failure_state(policy(true, true), 1, now);
    assert_eq!(streak, 2);
    let delay = due.expect("a due time") - now;
    assert!(
        (5_000..=10_000).contains(&delay),
        "the second retry doubles, got {delay}"
    );

    // Disabled, or no scheduler: failed, no due time, streak reset.
    assert_eq!(
        next_failure_state(policy(false, true), 3, now),
        ("failed", None, 0)
    );
    assert_eq!(
        next_failure_state(policy(true, false), 3, now),
        ("failed", None, 0)
    );
}

/// v2 `sessionEvidence` / `probeSucceeded`: weak evidence on a healthy session
/// is silent — the entry keeps its client, tools and streak, and no status
/// event is emitted because the public entry (`probing` → `connected`) never
/// changed.
#[tokio::test]
async fn weak_evidence_on_a_live_client_keeps_it_connected_silently() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager
        .configure("remote", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");
    let connected = manager.server_entries().await;
    assert_eq!(connected[0].status, "connected");
    assert_eq!(connected[0].tool_count, 1);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _subscription = manager.on_status_change(Box::new(move |entry| {
        let _ = tx.send(entry);
    }));

    let client = manager
        .clients
        .read()
        .await
        .get("remote")
        .cloned()
        .expect("the client is registered");
    manager
        .evidence_context("remote", client)
        .probe("Maximum reconnection attempts (2) exceeded.".into())
        .await;

    let after = manager.server_entries().await;
    assert_eq!(
        after[0].status, "connected",
        "a live probe restores connected"
    );
    assert_eq!(after[0].tool_count, 1, "the tools are untouched");
    let servers = manager.servers.read().await;
    assert_eq!(
        servers.get("remote").expect("entry").retry_streak,
        0,
        "a successful probe does not start a retry streak"
    );
    assert!(
        rx.try_recv().is_err(),
        "a healthy probe must not emit a status change"
    );
}

/// v2 `probeFailed`: a probe that cannot reach the session fails the entry on
/// the same ladder a dead transport uses, with `formatUnexpectedCloseError`'s
/// text, no tools and a scheduled retry exposed as `failed` + `nextRetryAt`.
#[tokio::test]
async fn weak_evidence_on_a_dead_client_fails_the_entry_and_schedules_a_retry() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager.set_defaults(crate::config::McpConfig {
        reconnect_initial_delay_ms: Some(1),
        reconnect_max_delay_ms: Some(1),
        ..Default::default()
    });
    manager.start_retry_ticker();
    manager
        .configure("remote", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock server connects");

    let client = manager
        .clients
        .read()
        .await
        .get("remote")
        .cloned()
        .expect("the client is registered");
    // A closed client cannot answer the ping: this is the "session is dead"
    // half of v2's weak-evidence branch, reached through `probeMcpLiveness`.
    client.close().await;

    manager
        .evidence_context("remote", client)
        .probe("Maximum reconnection attempts (2) exceeded.".into())
        .await;

    let entry = manager
        .server_entries()
        .await
        .into_iter()
        .find(|entry| entry.name == "remote")
        .expect("the entry is resident");
    assert_eq!(entry.status, "failed");
    assert_eq!(entry.tool_count, 0, "a dead session exposes no tools");
    assert!(
        entry.next_retry_at.is_some(),
        "the retry is scheduled and public"
    );
    let error = entry.error.expect("the failure reason is recorded");
    assert!(
        error.contains("MCP server \"remote\" closed unexpectedly"),
        "the reason names the server, got {error}"
    );
    assert!(
        error.contains("Maximum reconnection attempts (2) exceeded."),
        "the reason carries the transport's detail, got {error}"
    );
    {
        let servers = manager.servers.read().await;
        let state = servers.get("remote").expect("the entry is resident");
        assert_eq!(state.status, "retry-waiting");
        assert!(state.raw_tools.is_empty());
    }
}

/// v2 `shutdown` awaits the entry's operation tail, so a probe still running
/// when teardown starts is drained before the roster is cleared.
#[tokio::test]
async fn shutdown_waits_for_an_in_flight_probe() {
    let manager = std::sync::Arc::new(McpManager::new());
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let probe = tokio::spawn(async move {
        let _ = released.await;
    });
    manager
        .probe_tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(probe);

    let mut shutdown = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager.shutdown().await;
        })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "shutdown must not return while a probe is in flight"
    );
    let _ = release.send(());
    tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
        .await
        .expect("shutdown settles once the probe ends")
        .expect("shutdown does not panic");
}

/// v2 `resolveClientName` (connection-manager.ts:834): the identity the host
/// sets is what every connected client announces, not the builtin name.
#[tokio::test]
async fn a_connected_client_announces_the_managers_identity() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager.set_client_name(Some("kimi-code-cli".to_string()));
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock connects");
    let client = manager
        .clients
        .read()
        .await
        .get("mock")
        .cloned()
        .expect("the mock client is registered");
    assert_eq!(
        client.client_info()["name"],
        serde_json::Value::from("kimi-code-cli"),
        "the manager's identity must reach the client that initializes"
    );
    manager.shutdown().await;
}

/// v2 `upsert` (connection-manager.ts:334-353): a live entry whose
/// configuration is unchanged is returned as it is, so running the same
/// configuration twice must not retire a connected client.
#[tokio::test]
async fn configuring_a_live_server_with_the_same_config_is_a_no_op() {
    let manager = std::sync::Arc::new(McpManager::new());
    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("mock connects");
    let first = manager
        .clients
        .read()
        .await
        .get("mock")
        .cloned()
        .expect("the mock client is registered");

    manager
        .configure("mock", McpServerRecipe::Mock, McpServerOptions::default())
        .await
        .expect("the second configure succeeds");
    let second = manager
        .clients
        .read()
        .await
        .get("mock")
        .cloned()
        .expect("the mock client is still registered");
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "an unchanged config must keep the live client instead of reconnecting"
    );

    // A changed config is not a no-op: the filter differs, so the entry is
    // rebuilt and the old client replaced.
    manager
        .configure(
            "mock",
            McpServerRecipe::Mock,
            McpServerOptions {
                enabled_tools: Some(vec!["nothing".to_string()]),
                ..McpServerOptions::default()
            },
        )
        .await
        .expect("a changed config reconnects");
    let third = manager
        .clients
        .read()
        .await
        .get("mock")
        .cloned()
        .expect("the mock client is registered after the change");
    assert!(
        !std::sync::Arc::ptr_eq(&first, &third),
        "a changed config must reconfigure instead of returning the old entry"
    );
    manager.shutdown().await;
}

/// v2 `shutdown` cancels in-flight startup ("serializes replacement close and
/// tracks disabled or removed clients through shutdown"): a connect that is
/// still handshaking is not in `clients` yet, so draining `clients` would leave
/// its child running until the startup timeout. The abort has to drop the
/// connect future — and `kill_on_drop` takes the child with it — which shows up
/// as `connect_all`'s task settling immediately instead of at the timeout.
#[tokio::test]
async fn shutdown_aborts_an_in_flight_startup() {
    let manager = std::sync::Arc::new(McpManager::new());
    // A server that never speaks the protocol: it stays alive and silent, so
    // the handshake can only end through the startup timeout or an abort. One
    // process, not a shell wrapper — a surviving grandchild would keep the
    // runtime's child reaper alive and hide how fast the abort settles.
    let (command, args): (String, Vec<String>) = if cfg!(windows) {
        (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Start-Sleep -Seconds 30".to_string(),
            ],
        )
    } else {
        ("sleep".to_string(), vec!["30".to_string()])
    };
    let handle = manager.connect_all(vec![(
        "slow".to_string(),
        McpServerRecipe::Stdio {
            command,
            args,
            env: std::collections::HashMap::new(),
            cwd: None,
        },
        McpServerOptions::default(),
    )]);
    // Let the child spawn and the handshake begin before the teardown.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    manager.shutdown().await;

    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect(
            "connect_all must settle once shutdown aborts the startup, not at the 30 s timeout",
        );
    settled.expect("connect_all's task must not panic when aborted");
}

/// A second shutdown must be a no-op: the ledger is already disposed, so no
/// teardown step runs twice.
#[tokio::test]
async fn shutdown_is_idempotent() {
    let manager = McpManager::new();
    manager.shutdown().await;
    manager.shutdown().await;
    assert_eq!(
        manager.dispose.lock().await.state(),
        crate::primitives::teardown::LedgerState::Disposed
    );
}
