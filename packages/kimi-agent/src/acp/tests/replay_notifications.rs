use super::super::*;
use super::common::*;

/// The `thinking` config option (v2 `buildThinkingOption`): it appears only
/// for a model that declares a thinking axis, follows the model's declared
/// efforts, clamps always-thinking models to `on`, and disappears when the
/// session switches to a model without the axis.
#[tokio::test]
async fn test_acp_thinking_option_follows_the_model_catalog() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, "http://127.0.0.1:1/v1");
    let config: crate::config::KimiConfig = r#"
[providers.acme]
type = "openai"
api_key = "k"

[models.thinky]
provider = "acme"
model = "thinky"
capabilities = ["thinking", "tool_use"]
support_efforts = ["low", "high"]
default_effort = "high"

[models.always]
provider = "acme"
model = "always"
capabilities = ["always_thinking"]
support_efforts = ["low", "high"]

[models.plain]
provider = "acme"
model = "plain"
"#
    .parse()
    .expect("parse config");
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
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
    let options = |response: &JsonRpcResponse| -> Vec<serde_json::Value> {
        response.result.clone().unwrap()["configOptions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    let thinking = |options: &[serde_json::Value]| -> Option<serde_json::Value> {
        options
            .iter()
            .find(|option| option["id"] == "thinking")
            .cloned()
    };

    // The engine's default model has no catalog row: no thinking option.
    let set_model = |id: u64, model: &str| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "model", "value": model }
        })
    };
    let initial = server
        .handle_message(&set_model(2, "thinky").to_string())
        .await
        .unwrap();
    let options_now = options(&initial);
    let option = thinking(&options_now).expect("a thinking-capable model advertises the toggle");
    assert_eq!(option["category"], "thought_level");
    assert_eq!(option["currentValue"], "high", "the declared default wins");
    let values: Vec<&str> = option["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, vec!["off", "low", "high"]);

    let set_thinking = |id: u64, value: &str| {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/set_config_option",
            "params": { "sessionId": sid, "configId": "thinking", "value": value }
        })
    };
    let chosen = server
        .handle_message(&set_thinking(3, "low").to_string())
        .await
        .unwrap();
    let option = thinking(&options(&chosen)).expect("the toggle stays advertised");
    assert_eq!(option["currentValue"], "low");
    assert_eq!(
        store.get_state("agent_config", &sid).unwrap().unwrap()["thinking"],
        "low"
    );

    let invalid = server
        .handle_message(&set_thinking(4, "turbo").to_string())
        .await
        .unwrap();
    assert!(
        invalid
            .error
            .unwrap()
            .message
            .contains("Invalid thinking level"),
        "unknown levels are refused"
    );

    // Always-thinking: `off` is not offered and a request for it clamps.
    // The stored effort level carries over while the new model accepts it.
    let always = server
        .handle_message(&set_model(5, "always").to_string())
        .await
        .unwrap();
    let option = thinking(&options(&always)).expect("always-thinking still advertises");
    assert_eq!(option["currentValue"], "low");
    let values: Vec<&str> = option["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["value"].as_str().unwrap())
        .collect();
    assert!(!values.contains(&"off"), "{values:?}");
    let clamped = server
        .handle_message(&set_thinking(6, "off").to_string())
        .await
        .unwrap();
    assert_eq!(
        thinking(&options(&clamped)).unwrap()["currentValue"],
        "on",
        "an always-thinking model clamps off to on"
    );

    // A model without the axis drops the option again.
    let plain = server
        .handle_message(&set_model(7, "plain").to_string())
        .await
        .unwrap();
    assert!(
        thinking(&options(&plain)).is_none(),
        "a non-thinking model advertises no toggle"
    );
}

/// `session/load` replays the transcript and the session's carried state:
/// the todo list behind the ACP `plan` notification and the context usage
/// the reattached client cannot derive from messages alone.
#[tokio::test]
async fn test_acp_session_load_replays_plan_and_usage() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let workspace = tempfile::tempdir().unwrap();
    let engine = engine_with_native_llm(&store, "http://127.0.0.1:1/v1");
    let config: crate::config::KimiConfig = r#"
[providers.acme]
type = "openai"
api_key = "k"

[models.mock-model]
provider = "acme"
model = "mock-model"
max_context_size = 100000
"#
    .parse()
    .expect("parse config");
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
    let server = AcpServer::with_engine(store.clone(), engine);

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

    let state = crate::storage::StateStore::for_session(workspace.path(), &sid).unwrap();
    state
        .write_domain(
            "todo",
            &json!([{ "id": "T1", "title": "Replay me", "status": "in_progress" }]),
        )
        .unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AcpOutbound>();
    server.set_notification_sink(tx);
    let load_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session/load",
        "params": { "sessionId": sid }
    });
    let resp = server.handle_message(&load_req.to_string()).await.unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");

    let mut updates = Vec::new();
    while let Ok(message) = rx.try_recv() {
        if let AcpOutbound::Notification(note) = message {
            updates.push(note.params.unwrap_or_default());
        }
    }
    let plan = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "plan")
        .unwrap_or_else(|| panic!("missing replayed plan: {updates:?}"));
    assert_eq!(plan["update"]["entries"][0]["content"], "Replay me");
    let usage = updates
        .iter()
        .find(|update| update["update"]["sessionUpdate"] == "usage_update")
        .unwrap_or_else(|| panic!("missing replayed usage: {updates:?}"));
    assert_eq!(usage["update"]["size"], 100000);
}

/// v2 `loop.notify` on the ACP entry point: a background completion that
/// is waiting when a prompt runs becomes its own model turn after the
/// prompt response, carrying the rendered notification and task origin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_acp_prompt_drains_background_task_notifications() {
    let (base_url, requests, mock) = spawn_repeating_openai_server(2).await;
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let engine = engine_with_native_llm(&store, &base_url);
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    engine
        .subagent_manager()
        .set_task_runner(runner.clone())
        .await;
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

    runner
        .spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: Some(sid.as_str()),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
            },
            "t-wake".into(),
            "wake job".into(),
            async { crate::storage::TaskOutcome::Completed("wake output".to_string()) },
        )
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while runner.pending_notification_count(Some(&sid)) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the task never queued its notification"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

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
    .expect("the prompt must settle")
    .unwrap();
    assert!(resp.error.is_none(), "unexpected error: {resp:?}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while requests.lock().unwrap().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the notification turn never reached the model"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let bodies = requests.lock().unwrap();
    assert!(
        bodies[1].contains("Background task t-wake (completed) finished: wake job"),
        "the wake turn must carry the rendered notification: {}",
        bodies[1]
    );
    assert!(
        bodies[1].contains("wake output"),
        "the wake turn must carry the output preview: {}",
        bodies[1]
    );
    drop(bodies);

    let turns = store.list_turns(&sid).unwrap();
    assert!(
        turns.iter().any(|turn| {
            turn.origin
                .as_ref()
                .and_then(|origin| origin.get("kind"))
                .and_then(|kind| kind.as_str())
                == Some("task")
        }),
        "the wake turn records the v2 task origin: {turns:?}"
    );

    mock.abort();
}
