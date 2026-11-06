use super::super::*;
use super::common::*;

/// A completed turn answers with the ACP `end_turn`, not the engine's Rust
/// enum name — a strict client parses `stopReason` against the ACP enum.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acp_prompt_reports_acp_stop_reason() {
    let (base_url, mock) = spawn_completing_openai_server().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let server = AcpServer::with_engine(store, engine);

    let new_req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
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

    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": { "sessionId": sid, "prompt": "hello" }
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server.handle_message(&prompt.to_string()),
    )
    .await
    .expect("the turn must settle")
    .unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp.result.unwrap()["stopReason"], "end_turn");

    mock.abort();
}

/// A real turn must stream its output to the client. This is the
/// end-to-end counterpart of `test_raw_turn_events_are_forwarded_to_the_client`:
/// it drives the actual engine through `session/prompt` and asserts the
/// client received the model's text, so a mapper that silently drops every
/// production event shape fails here instead of passing unit tests that
/// hand-build their input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_real_turn_streams_session_updates() {
    let (base_url, mock) = spawn_completing_openai_server().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let server = Arc::new(AcpServer::with_engine(store, engine));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let new_req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
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

    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": { "sessionId": sid, "prompt": "hello" }
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server.handle_message(&prompt.to_string()),
    )
    .await
    .expect("the turn must settle")
    .unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    assert!(
        updates.iter().any(
            |update| update["update"]["sessionUpdate"] == "agent_message_chunk"
                && update["update"]["content"]["text"] == "hi"
        ),
        "the model's text must reach the client as agent_message_chunk: {updates:?}"
    );

    mock.abort();
}

/// `session/new` advertises the builtin command palette and the initial
/// title (v2 `emitAvailableCommandsUpdate` + `session_info_update`).
#[tokio::test]
async fn test_acp_new_session_emits_commands_and_info() {
    let server = AcpServer::in_memory().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": { "title": "My Session" }
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

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    let commands = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "available_commands_update")
        .unwrap_or_else(|| panic!("missing available_commands_update: {updates:?}"));
    let names: Vec<&str> = commands["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["compact", "status", "usage", "mcp", "tasks", "help"]
    );
    assert_eq!(commands["sessionId"], sid);

    let info = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "session_info_update")
        .unwrap_or_else(|| panic!("missing session_info_update: {updates:?}"));
    assert_eq!(info["update"]["title"], "My Session");
}

/// The advertised builtins answer locally: no model turn runs, the text
/// comes back as one `agent_message_chunk`, and `/usage` reads the
/// session's durable counters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acp_builtin_commands_run_locally() {
    let (base_url, mock) = spawn_completing_openai_server().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let server = AcpServer::with_engine(store.clone(), engine);

    let new_req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
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

    crate::session::usage::record_turn(
        &store,
        &sid,
        "mock-model",
        &crate::rpc::types::TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        },
    )
    .unwrap();

    let prompt = |id: u64, text: &str| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": text }
        })
    };

    let help = server
        .handle_message(&prompt(2, "/help").to_string())
        .await
        .unwrap();
    let help = help.result.expect("help answers");
    assert_eq!(help["stopReason"], "end_turn");
    assert!(help["content"].as_str().unwrap().contains("/status"));

    let status = server
        .handle_message(&prompt(3, "/status").to_string())
        .await
        .unwrap()
        .result
        .expect("status answers");
    let status_text = status["content"].as_str().unwrap();
    assert!(
        status_text.contains(&format!("Session: {sid}")),
        "{status_text}"
    );
    assert!(status_text.contains("Model: mock-model"), "{status_text}");

    let usage = server
        .handle_message(&prompt(4, "/usage").to_string())
        .await
        .unwrap()
        .result
        .expect("usage answers");
    let usage_text = usage["content"].as_str().unwrap();
    assert!(
        usage_text.contains("Session total: 10 input, 5 output"),
        "{usage_text}"
    );

    mock.abort();
}

/// A title change outside ACP (a generated or host-renamed title) reaches
/// the client as `session_info_update` at the next prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acp_session_info_update_follows_title_change() {
    let (base_url, mock) = spawn_completing_openai_server().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let server = AcpServer::with_engine(store.clone(), engine);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": { "title": "First" }
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
    while rx.try_recv().is_ok() {}

    server
        .store
        .update_session_title(&sid, Some("Second"))
        .unwrap();
    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": { "sessionId": sid, "prompt": "/status" }
    });
    server.handle_message(&prompt.to_string()).await.unwrap();

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    let info = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "session_info_update")
        .unwrap_or_else(|| panic!("missing title update: {updates:?}"));
    assert_eq!(info["update"]["title"], "Second");

    mock.abort();
}

/// After a turn the client gets the v2 `usage_update` (context used against
/// the configured model window) and `plan` (the todo list the agent left)
/// notifications, not just the final response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_real_turn_emits_usage_and_plan_updates() {
    let (base_url, mock) = spawn_completing_openai_server().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let mut config = crate::config::KimiConfig::default();
    config.models.insert(
        "mock-model".into(),
        crate::config::ModelAliasConfig {
            max_context_size: Some(42_000),
            ..Default::default()
        },
    );
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
    let server = Arc::new(AcpServer::with_engine(store, engine));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let workspace = tempfile::tempdir().unwrap();
    let new_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "session/new",
        "params": { "cwd": workspace.path().to_str().unwrap() }
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

    // The todo tools write this domain; the ACP server reads the same one.
    let state = crate::storage::StateStore::for_workspace(workspace.path()).unwrap();
    state
        .write_domain(
            "todo",
            &json!([
                { "id": "T1", "title": "Read the code", "status": "pending" },
                { "id": "T2", "title": "Apply the fix", "status": "done" }
            ]),
        )
        .unwrap();

    let prompt = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/prompt",
        "params": { "sessionId": sid, "prompt": "hello" }
    });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server.handle_message(&prompt.to_string()),
    )
    .await
    .expect("the turn must settle")
    .unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    let usage = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "usage_update")
        .unwrap_or_else(|| panic!("missing usage_update: {updates:?}"));
    assert_eq!(usage["sessionId"], sid);
    assert_eq!(usage["update"]["size"], 42_000);
    assert!(
        usage["update"]["used"].as_u64().unwrap_or(0) > 0,
        "used must reflect the persisted conversation: {usage:?}"
    );

    let plan = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "plan")
        .unwrap_or_else(|| panic!("missing plan: {updates:?}"));
    assert_eq!(
        plan["update"]["entries"],
        json!([
            { "content": "Read the code", "priority": "medium", "status": "pending" },
            { "content": "Apply the fix", "priority": "medium", "status": "completed" },
        ])
    );

    mock.abort();
}

/// `$/cancel_request` reaches a turn that is still streaming and settles
/// the prompt with the ACP `cancelled` stop reason — the response the ACP
/// cancellation contract requires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acp_cancel_request_stops_a_running_turn() {
    let (base_url, mock) = spawn_hanging_openai_server().await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let server = Arc::new(AcpServer::with_engine(store, engine.clone()));

    let new_req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
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

    // The prompt runs on its own task, exactly as `run_stdio` dispatches
    // it: the reader has to stay free to deliver the cancellation.
    let prompt_server = server.clone();
    let prompt_sid = sid.clone();
    let prompt = tokio::spawn(async move {
        prompt_server
            .handle_message(
                &json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "session/prompt",
                    "params": { "sessionId": prompt_sid, "prompt": "hello" }
                })
                .to_string(),
            )
            .await
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !engine.is_turn_active(&sid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the turn never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let cancel = json!({
        "jsonrpc": "2.0",
        "method": "$/cancel_request",
        "params": { "requestId": 7 }
    });
    assert!(
        server.handle_message(&cancel.to_string()).await.is_none(),
        "a notification must never be answered"
    );

    let resp = tokio::time::timeout(std::time::Duration::from_secs(20), prompt)
        .await
        .expect("a cancelled prompt must settle promptly")
        .unwrap()
        .unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");
    assert_eq!(resp.result.unwrap()["stopReason"], "cancelled");

    mock.abort();
}

/// v2 `driveSkillActivation` + `driveUnknownCommand`: a `/skill:` command
/// renders through the engine's own scan and drives a normal turn with the
/// activation origin, while an unknown slash command is answered locally
/// and never reaches the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acp_skill_commands_activate_and_unknown_commands_answer_locally() {
    let (base_url, mock) = spawn_completing_openai_server().await;
    let skills_root = tempfile::tempdir().unwrap();
    let skill_dir = skills_root.path().join("demo");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill for ACP\n---\n\nDo the demo: $ARGUMENTS\n",
    )
    .unwrap();

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm_and_skills(
        &store,
        &base_url,
        vec![skills_root.path().to_path_buf()],
    );
    let server = AcpServer::with_engine(store.clone(), engine);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);

    let new_req = json!({
        "jsonrpc": "2.0", "id": 1, "method": "session/new", "params": {}
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

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    let commands = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "available_commands_update")
        .expect("session/new advertises commands");
    let names: Vec<&str> = commands["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"skill:demo"),
        "the project skill must be advertised: {names:?}"
    );

    let prompt = |id: u64, text: &str| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": { "sessionId": sid, "prompt": text }
        })
    };

    // Unknown slash commands never reach the model.
    let unknown = server
        .handle_message(&prompt(2, "/nope now").to_string())
        .await
        .unwrap()
        .result
        .expect("an unknown command answers");
    assert_eq!(unknown["stopReason"], "end_turn");
    assert!(
        unknown["content"]
            .as_str()
            .unwrap()
            .contains("Unknown ACP command: /nope"),
        "{unknown}"
    );

    // The skill activation runs a normal turn against the engine.
    let activated = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        server.handle_message(&prompt(3, "/skill:demo hello world").to_string()),
    )
    .await
    .expect("the skill turn must settle")
    .unwrap()
    .result
    .expect("the skill activation answers");
    assert_eq!(activated["stopReason"], "end_turn");

    let history = store.load_session_history(&sid).unwrap_or_default();
    let activation = history
        .iter()
        .find(|message| message.content.contains("<skill-loaded"))
        .expect("the rendered skill prompt is part of the turn");
    assert!(
        activation.content.contains("Do the demo: hello world"),
        "the activation must expand $ARGUMENTS: {}",
        activation.content
    );
    let turns = store.list_turns(&sid).unwrap();
    let origin = turns
        .iter()
        .find_map(|turn| turn.origin.as_ref())
        .expect("the turn carries an origin");
    assert_eq!(
        origin.get("kind").and_then(|kind| kind.as_str()),
        Some("skill_activation"),
        "the turn carries the v2 skill activation origin"
    );

    mock.abort();
}
