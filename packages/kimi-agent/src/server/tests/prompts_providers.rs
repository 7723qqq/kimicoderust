use super::super::*;
use super::common::*;
use std::collections::HashMap;

/// The full server path for an unconsumed steer (v2's semantics; the
/// session path's session-scoped queue behaves the same): a prompt runs
/// against a model endpoint that hangs mid-stream, a queued prompt is
/// steered into the running turn, the turn is aborted before the steer is
/// drained — the cancel check runs at the step top, ahead of
/// `drain_steers` — and the next prompt's turn must carry the steered text
/// exactly once. Before `release_turn_steer_state`, the steer died with the
/// aborted turn and the text appeared nowhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_steer_survives_a_cancel_and_joins_the_next_turn_over_rest() {
    // A local OpenAI-compatible endpoint: the first request streams one
    // delta and then holds the connection (only a cancellation ends that
    // turn); every later request answers a complete turn.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let first_request = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mock = {
        let first_request = first_request.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let first_request = first_request.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let hanging = first_request.swap(false, std::sync::atomic::Ordering::SeqCst);
                    let body = if hanging {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hel\"}}]}\n\n".to_string()
                    } else {
                        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                             data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                             data: [DONE]\n\n"
                            .to_string()
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}"
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.flush().await;
                    if hanging {
                        // Hold the connection open: the turn only ends through cancellation.
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    }
                });
            }
        })
    };

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let engine = ServerEngine::new(
        crate::pipeline::PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "mock-model".into(),
            providers: Vec::new(),
            native_llm: Some(crate::rpc::types::NativeLlmConfig {
                protocol: "openai".into(),
                base_url: format!("http://{addr}/v1"),
                api_key: "test-key".into(),
                model: "mock-model".into(),
                ..Default::default()
            }),
            workspace_root: None,
            native_tools: false,
            extra_roots: Vec::new(),
            rust_self_contained: true,
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
        hub.clone(),
        store.clone(),
    );
    let server = HttpServer::with_hub(store.clone(), hub).with_engine(engine);
    let engine = server.engine().expect("the server holds its engine");
    store.create_session("sess-steer-cancel", None).unwrap();

    let post = |path: &str, body: Value| HttpRequest {
        method: "POST".into(),
        path: path.into(),
        query: None,
        headers: HashMap::new(),
        body: serde_json::to_vec(&body).unwrap(),
    };
    async fn wait_until(deadline_secs: u64, mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(deadline_secs);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "condition not met within {deadline_secs}s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // 1. The first prompt runs against the hanging endpoint.
    let first = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts",
            json!({ "prompt": "first" }),
        ))
        .await;
    assert_eq!(first.status, 200);
    let first_item: Value = serde_json::from_slice(&first.body).unwrap();
    let first_id = first_item["prompt_id"].as_str().unwrap().to_string();
    assert_eq!(first_item["status"], "running");
    wait_until(10, || engine.is_turn_active("sess-steer-cancel")).await;
    // `is_turn_active` is set when the turn is admitted, which is *before* its
    // first step head drains steering. A steer submitted in that window is
    // ordinary steering — the first head consumes it — so the cancel case this
    // test pins needs the turn to be inside the model request first. Wait for
    // the mock to have read that request ("hel" is already streaming); on a
    // cold start the checkpoint write before the first step head is slow enough
    // that the steer could otherwise arrive first and be drained.
    wait_until(10, || {
        !first_request.load(std::sync::atomic::Ordering::SeqCst)
    })
    .await;

    // 2. A second prompt queues behind it and is steered into the running turn.
    let second = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts",
            json!({ "prompt": "steered-text" }),
        ))
        .await;
    let second_item: Value = serde_json::from_slice(&second.body).unwrap();
    let second_id = second_item["prompt_id"].as_str().unwrap().to_string();
    assert_eq!(second_item["status"], "queued");

    let steer = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts:steer",
            json!({ "prompt_ids": [second_id] }),
        ))
        .await;
    assert_eq!(steer.status, 200);

    // 3. Abort the running turn before its next step head drains the steer.
    let abort = server
        .handle_request(&post(
            &format!("/api/v1/sessions/sess-steer-cancel/prompts/{first_id}:abort"),
            json!({}),
        ))
        .await;
    assert_eq!(abort.status, 200);
    wait_until(10, || !engine.is_turn_active("sess-steer-cancel")).await;
    assert_eq!(
        engine.queued_steer_count("sess-steer-cancel"),
        1,
        "the undrained steer survives the cancelled turn"
    );

    // 4. The next prompt's turn drains the survivor at its first step head.
    let third = server
        .handle_request(&post(
            "/api/v1/sessions/sess-steer-cancel/prompts",
            json!({ "prompt": "third" }),
        ))
        .await;
    assert_eq!(third.status, 200);

    // 5. The steered text is persisted exactly once, inside the new turn.
    let steered_turn = std::sync::Mutex::new(None);
    wait_until(10, || {
        let messages = store.load_session_messages("sess-steer-cancel").unwrap();
        let hits: Vec<&crate::session::sqlite_store::StoredMessage> = messages
            .iter()
            .filter(|m| m.message.content.contains("steered-text"))
            .collect();
        if hits.len() > 1 {
            panic!(
                "the steered text must appear exactly once, saw {}",
                hits.len()
            );
        }
        if let Some(hit) = hits.first() {
            *steered_turn.lock().unwrap() = Some(hit.turn_id.clone());
        }
        !hits.is_empty()
    })
    .await;
    let steered_turn = steered_turn.into_inner().unwrap();
    let messages = store.load_session_messages("sess-steer-cancel").unwrap();
    let third_turn = messages
        .iter()
        .find(|m| m.message.content == "third")
        .map(|m| m.turn_id.clone())
        .expect("the third prompt is persisted");
    assert_eq!(
        steered_turn.as_deref(),
        Some(third_turn.as_str()),
        "the rescued steer belongs to the turn that ran after the cancel"
    );
    mock.abort();
}

#[tokio::test]
async fn prompt_queue_routes_reflect_state() {
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-pq", None).unwrap();

    // Idle session: no active prompt and an empty queue.
    let list = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: "/api/v1/sessions/sess-pq/prompts".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(list.status, 200);
    let list_body: Value = serde_json::from_slice(&list.body).unwrap();
    assert!(list_body["active"].is_null());
    assert_eq!(list_body["queued"], json!([]));

    // Steering an unknown prompt id is PROMPT_NOT_FOUND, not a fake success.
    let steer = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-pq/prompts:steer".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "prompt_ids": ["nope"] })).unwrap(),
        })
        .await;
    assert_eq!(steer.status, 404);

    // Aborting an unknown prompt is PROMPT_NOT_FOUND, not a fake success.
    let abort = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-pq/prompts/nope:abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(abort.status, 404);
    let abort_body: Value = serde_json::from_slice(&abort.body).unwrap();
    assert_eq!(abort_body["code"], 40402);
}

/// ROADMAP §7.6: the web UI's abort button sends the live
/// `event.message.created` id (`msg-u{turn}`). Before the message-id
/// resolution landed, the route answered 404 for exactly the id the
/// button holds.
#[tokio::test]
async fn abort_resolves_the_live_user_message_id_at_the_route() {
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-abort", None).unwrap();

    let (item, run) = server.prompt_queue.admit(
        "sess-abort",
        json!({
            "prompt_id": "prompt-1",
            "user_message_id": "msg-prompt-1",
            "content": [{ "type": "text", "text": "hi" }],
            "created_at": "2026-01-01T00:00:00Z",
        }),
        "hi".into(),
        Vec::new(),
        None,
    );
    assert!(run.is_some(), "the first prompt is active");
    assert_eq!(item["status"], "running");
    // The driver stamps the turn the live event names.
    server.prompt_queue.stamp_active_turn("sess-abort", 2);

    let abort = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions/sess-abort/prompts/msg-u2:abort".into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(abort.status, 200);
    let abort_body: Value = serde_json::from_slice(&abort.body).unwrap();
    assert_eq!(abort_body["aborted"], true);
    assert!(server.prompt_queue.active_is_cancelled("sess-abort"));
}

#[tokio::test]
async fn prompt_submission_options_persist_to_session_profile() {
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-opt", None).unwrap();
    let body = json!({
        "content": [{ "type": "text", "text": "hi" }],
        "model": "alias-2",
        "thinking": "high",
        "permission_mode": "yolo",
        "plan_mode": true,
        "disabled_tools": ["Bash", "Bash", "WebSearch"],
        "profile": "coder",
        "metadata": { "title": "T" },
    });
    apply_prompt_submission_options(&server, "sess-opt", &body).unwrap();

    let config = server
        .store()
        .get_state("agent_config", "sess-opt")
        .unwrap()
        .unwrap();
    assert_eq!(config["model"], "alias-2");
    assert_eq!(config["thinking"], "high");
    assert_eq!(config["plan_mode"], true);
    assert_eq!(
        config["disabled_tools"],
        json!(["Bash", "WebSearch"]),
        "v2 setSessionDisabledTools collapses duplicates, order preserved"
    );
    assert_eq!(config["profile"], "coder");

    let metadata = server
        .store()
        .get_state("metadata", "sess-opt")
        .unwrap()
        .unwrap();
    assert_eq!(metadata["permission_mode"], "yolo");
    assert_eq!(metadata["title"], "T");

    // An unknown permission mode is rejected before any turn runs.
    let bad = json!({
        "content": [{ "type": "text", "text": "x" }],
        "permission_mode": "nope",
    });
    assert!(apply_prompt_submission_options(&server, "sess-opt", &bad).is_err());
}

#[tokio::test]
async fn prompt_plan_mode_activates_workspace_plan_state() {
    let dir = std::env::temp_dir().join(format!("kimi-plan-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let server = HttpServer::in_memory().unwrap();
    server.store().create_session("sess-plan", None).unwrap();
    let body = json!({
        "content": [{ "type": "text", "text": "plan it" }],
        "plan_mode": true,
        "metadata": { "cwd": dir.to_string_lossy() },
    });
    apply_prompt_submission_options(&server, "sess-plan", &body).unwrap();

    let state = crate::storage::StateStore::for_workspace(&dir).unwrap();
    assert_eq!(
        state
            .read_domain_state("plan")
            .into_result()
            .unwrap()
            .unwrap()["active"],
        true
    );
}

#[tokio::test]
async fn test_http_provider_crud_writes_config() {
    let dir = std::env::temp_dir().join(format!("kimi-provider-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        "default_model = \"\"\n\n[providers.openai]\ntype = \"openai\"\napi_key = \"sk-old\"\n",
    )
    .unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());

    fn request(method: &str, path: &str, body: Option<&Value>) -> HttpRequest {
        HttpRequest {
            method: method.into(),
            path: path.into(),
            query: None,
            headers: HashMap::new(),
            body: body
                .map(|value| serde_json::to_vec(value).unwrap())
                .unwrap_or_default(),
        }
    }

    // 1. Create writes the provider + aliases and seeds the unset default.
    let create_body = json!({
        "id": "kimi-code",
        "type": "openai",
        "api_key": "sk-test",
        "base_url": "https://example.test/v1",
        "default_model": "k3",
        "models": [
            { "model": "k3", "max_context_size": 200000, "display_name": "K3" },
            { "model": "fast", "max_context_size": 128000 }
        ]
    });
    let res = server
        .handle_request(&request("POST", "/api/v1/providers", Some(&create_body)))
        .await;
    assert_eq!(res.status, 201);
    let created: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(created["id"], "kimi-code");
    assert_eq!(created["has_api_key"], true);
    assert_eq!(created["status"], "connected");
    assert_eq!(created["default_model"], "kimi-code/k3");

    // 2. A duplicate id is a PROVIDER_ALREADY_EXISTS conflict.
    let res = server
        .handle_request(&request("POST", "/api/v1/providers", Some(&create_body)))
        .await;
    assert_eq!(res.status, 409);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40921);

    // 3. The single-provider read reveals the stored key.
    let res = server
        .handle_request(&request("GET", "/api/v1/providers/kimi-code", None))
        .await;
    assert_eq!(res.status, 200);
    let fetched: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(fetched["api_key"], "sk-test");

    // 4. The write is format-preserving and the read side sees it.
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.kimi-code]"), "{text}");
    assert!(text.contains("[models.\"kimi-code/k3\"]"), "{text}");
    assert!(text.contains("default_model = \"kimi-code/k3\""), "{text}");
    assert!(text.contains("[providers.openai]"), "{text}");
    let res = server
        .handle_request(&request("GET", "/api/v1/models", None))
        .await;
    let models: Value = serde_json::from_slice(&res.body).unwrap();
    assert!(
        models["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["model"] == "kimi-code/k3")
    );

    // 5. Replace with a rename migrates default_model, drops stale aliases.
    let res = server
        .handle_request(&request(
            "PUT",
            "/api/v1/providers/kimi-code",
            Some(&json!({
                "new_id": "kimi-code-2",
                "type": "anthropic",
                "api_key": "sk-new",
                "base_url": "https://example.test/v2",
                "default_model": "k3",
                "models": [{ "model": "k3", "max_context_size": 200000 }]
            })),
        ))
        .await;
    assert_eq!(res.status, 200);
    let replaced: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(replaced["provider"]["id"], "kimi-code-2");
    assert_eq!(replaced["provider"]["type"], "anthropic");
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.kimi-code-2]"), "{text}");
    assert!(!text.contains("[providers.kimi-code]"), "{text}");
    assert!(
        text.contains("default_model = \"kimi-code-2/k3\""),
        "{text}"
    );
    assert!(!text.contains("kimi-code/fast"), "{text}");

    // 6. OAuth-managed providers refuse writes; delete cleans aliases.
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str(
            "\n[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\noauth = { provider = \"managed:kimi-code\" }\n",
        );
    std::fs::write(&config_path, text).unwrap();
    *server.config_override.lock().await = None;

    let res = server
        .handle_request(&request(
            "DELETE",
            "/api/v1/providers/managed%3Akimi-code",
            None,
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40003);

    let res = server
        .handle_request(&request("DELETE", "/api/v1/providers/kimi-code-2", None))
        .await;
    assert_eq!(res.status, 200);
    let deleted: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(deleted["deleted"], true);
    let text = std::fs::read_to_string(&config_path).unwrap();
    // The provider and its aliases are gone; the (now dangling) default
    // pointer is the user's setting and stays, matching kap-server.
    assert!(!text.contains("[providers.kimi-code-2]"), "{text}");
    assert!(!text.contains("[models.\"kimi-code-2/k3\"]"), "{text}");
    assert!(
        text.contains("default_model = \"kimi-code-2/k3\""),
        "{text}"
    );
    assert!(text.contains("[providers.openai]"), "{text}");

    // 7. Validation failures carry the kap-server validation code.
    let res = server
        .handle_request(&request(
            "POST",
            "/api/v1/providers",
            Some(&json!({ "id": "bad id!", "type": "openai", "models": [] })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40001);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn catalog_routes_fall_back_to_the_builtin_when_the_fetch_fails() {
    let mut server = HttpServer::in_memory().unwrap();
    server.models_dev_cache =
        Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(|| {
            Box::pin(async { Err("upstream down".to_string()) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        })));

    let res = server
        .handle_request(&catalog_request("GET", "/api/v1/catalog/providers", None))
        .await;
    assert_eq!(res.status, 200);
    let list: Value = serde_json::from_slice(&res.body).unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "the built-in snapshot ends the chain");
    assert!(items.iter().any(|item| item["id"] == "moonshot"));

    // The per-id route serves the same entry the list advertises.
    let res = server
        .handle_request(&catalog_request(
            "GET",
            "/api/v1/catalog/providers/anthropic",
            None,
        ))
        .await;
    assert_eq!(res.status, 200);
    let item: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(item["id"], "anthropic");
    assert_eq!(item["wire_type"], "anthropic");
    assert_eq!(item["base_url"], "https://api.anthropic.com");

    // An unknown id is a 404, not a fabricated entry.
    let res = server
        .handle_request(&catalog_request(
            "GET",
            "/api/v1/catalog/providers/nope",
            None,
        ))
        .await;
    assert_eq!(res.status, 404);
}

#[tokio::test]
async fn import_registry_writes_providers_with_their_source() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let registry = json!({
        "acme": {
            "id": "acme",
            "name": "Acme",
            "api": "https://registry.example.test/v1",
            "type": "openai",
            "env": ["ACME_API_KEY"],
            "models": { "big": { "id": "big-1", "limit": { "context": 128000 } } },
        },
        "beta": {
            "id": "beta",
            "name": "Beta",
            "api": "https://beta.example.test/v1",
            "type": "anthropic",
            "models": {
                "sonnet": {
                    "id": "sonnet-1",
                    "limit": { "context": 200000 },
                    "tool_call": true,
                    "reasoning": true,
                },
            },
        },
        "broken": { "id": "broken", "name": "Broken" },
    });
    let upstream = RegistryServer::spawn("200 OK", &registry.to_string()).await;

    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url, "api_key": "sk-reg" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let imported: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(imported["models_imported"], 2);
    let providers = imported["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2, "the invalid entry is skipped");
    assert_eq!(imported["credential_env"]["acme"], "ACME_API_KEY");
    // The Bearer key reached the registry.
    let requests = upstream.requests();
    assert!(
        requests
            .iter()
            .any(|request| request.contains("authorization: Bearer sk-reg")),
        "{requests:?}"
    );

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.acme]"), "{text}");
    assert!(text.contains("[providers.acme.source]"), "{text}");
    assert!(text.contains("kind = \"apiJson\""), "{text}");
    assert!(text.contains("apiKey = \"sk-reg\""), "{text}");
    assert!(text.contains("[models.\"acme/big\"]"), "{text}");
    assert!(text.contains("[models.\"beta/sonnet\"]"), "{text}");
    // The unset default is seeded from the first entry's first model.
    assert!(text.contains("default_model = \"acme/big\""), "{text}");

    // A re-import of the same URL with one provider gone removes the
    // vanished provider and its aliases; the URL is the identity, so the
    // stored key is reused when the request omits it.
    upstream.set_response(
        "200 OK",
        &json!({
            "acme": {
                "id": "acme",
                "name": "Acme",
                "api": "https://registry.example.test/v1",
                "type": "openai",
                "models": { "big": { "id": "big-1", "limit": { "context": 128000 } } },
            },
        })
        .to_string(),
    );
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(!text.contains("[providers.beta]"), "{text}");
    assert!(!text.contains("beta/sonnet"), "{text}");
    assert!(text.contains("[providers.acme]"), "{text}");
    // v2's remove-then-apply clears a default that pointed into the
    // re-imported provider, and its `hadDefault` reads the pre-apply
    // value, so it is not reseeded — the port mirrors that.
    assert!(!text.contains("default_model"), "{text}");
}

#[tokio::test]
async fn import_registry_rejects_the_unimportable() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"acme/big\"\n").unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let upstream = RegistryServer::spawn("200 OK", "{}").await;

    // A missing url is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({})),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40001);

    // An empty registry has no importable providers.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40005);
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("no importable providers")
    );

    // An upstream failure carries the status in the reason.
    upstream.set_response("401 Unauthorized", r#"{"message":"bad key"}"#);
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url, "api_key": "sk-bad" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40005);
    let msg = body["msg"].as_str().unwrap();
    assert!(msg.contains("cannot be imported"), "{msg}");
    assert!(msg.contains("401"), "{msg}");
    assert!(msg.contains("bad key"), "{msg}");

    // An OAuth-managed provider refuses the import.
    upstream.set_response(
        "200 OK",
        &json!({
            "acme": {
                "id": "acme",
                "name": "Acme",
                "api": "https://registry.example.test/v1",
                "type": "openai",
                "models": { "big": { "id": "big-1", "limit": { "context": 128000 } } },
            },
        })
        .to_string(),
    );
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[providers.acme]\ntype = \"openai\"\noauth = { provider = \"acme\" }\n");
    std::fs::write(&config_path, text).unwrap();
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_registry",
            Some(&json!({ "url": upstream.url })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40003);
}

#[tokio::test]
async fn import_catalog_falls_back_to_the_builtin_when_the_fetch_fails() {
    // The offline path a blocked-network user takes: the upstream fetch
    // fails, so the import resolves against the built-in snapshot.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let mut server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    server.models_dev_cache =
        Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(|| {
            Box::pin(async { Err("upstream down".to_string()) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        })));

    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "anthropic", "api_key": "sk-offline" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let imported: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(imported["models_imported"], 1);
    assert_eq!(imported["provider"]["id"], "anthropic");
    assert_eq!(imported["provider"]["type"], "anthropic");
    assert_eq!(imported["provider"]["status"], "connected");

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.anthropic]"), "{text}");
    assert!(
        text.contains("base_url = \"https://api.anthropic.com\""),
        "{text}"
    );
    assert!(
        text.contains("[models.\"anthropic/claude-3-7-sonnet-20250219\"]"),
        "{text}"
    );
    assert!(
        text.contains("default_model = \"anthropic/claude-3-7-sonnet-20250219\""),
        "{text}"
    );
}

#[tokio::test]
async fn import_catalog_writes_the_provider_and_its_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let mut server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let catalog = json!({
        "acme": {
            "id": "acme",
            "name": "Acme",
            "type": "openai",
            "api": "https://api.example.test/v1",
            "env": ["ACME_API_KEY"],
            "models": {
                "big": {
                    "id": "big",
                    "name": "Big",
                    "limit": { "context": 128000, "input": 64000 },
                    "reasoning_options": [{ "type": "effort", "values": ["low", "high"] }],
                },
                "small": { "id": "small", "limit": { "context": 8192 } },
            },
        },
    });
    server.models_dev_cache = Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(
        move || {
            let catalog = catalog.clone();
            Box::pin(async move { Ok(catalog) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        },
    )));

    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme", "api_key": "sk-acme" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let imported: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(imported["models_imported"], 2);
    assert_eq!(imported["provider"]["id"], "acme");
    assert_eq!(imported["provider"]["type"], "openai");
    assert_eq!(imported["provider"]["status"], "connected");

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.acme]"), "{text}");
    assert!(text.contains("type = \"openai\""), "{text}");
    assert!(text.contains("api_key = \"sk-acme\""), "{text}");
    assert!(
        text.contains("base_url = \"https://api.example.test/v1\""),
        "{text}"
    );
    assert!(text.contains("[models.\"acme/big\"]"), "{text}");
    assert!(text.contains("[models.\"acme/small\"]"), "{text}");
    // The unset default is seeded from the first imported model.
    assert!(text.contains("default_model = \"acme/big\""), "{text}");
    // The always-thinking rename and the capped input size ride along.
    assert!(text.contains("always_thinking"), "{text}");
    assert!(text.contains("max_input_size = 64000"), "{text}");

    // A re-import under a new id rewrites that entry; the old provider's
    // aliases survive (v2 filters by the target id) and the credential
    // carries over when the request supplies none.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme", "id": "acme-2" })),
        ))
        .await;
    assert_eq!(res.status, 201);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[providers.acme-2]"), "{text}");
    assert!(text.contains("[models.\"acme-2/big\"]"), "{text}");
    assert!(text.contains("[models.\"acme/big\"]"), "{text}");
    assert!(text.contains("api_key = \"sk-acme\""), "{text}");
    // The seeded default is not rewritten by the re-import.
    assert!(text.contains("default_model = \"acme/big\""), "{text}");
}

#[tokio::test]
async fn import_catalog_rejects_the_unimportable() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"acme/big\"\n").unwrap();
    let mut server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let catalog = json!({
        "acme": {
            "id": "acme",
            "name": "Acme",
            "api": "https://api.example.test/v1",
            "models": { "big": { "id": "big", "limit": { "context": 128000 } } },
        },
        "gateway": { "id": "gateway", "name": "Gateway", "npm": "@acme/gateway" },
        "bedrock": { "id": "bedrock", "name": "Bedrock", "type": "bedrock" },
        "empty": { "id": "empty", "name": "Empty", "api": "https://api.example.test" },
    });
    server.models_dev_cache = Arc::new(models_dev::CatalogCache::new().with_fetcher(Arc::new(
        move || {
            let catalog = catalog.clone();
            Box::pin(async move { Ok(catalog) })
                as crate::rpc::types::BoxFuture<'static, Result<Value, String>>
        },
    )));

    // An unknown catalog entry is CATALOG_ENTRY_NOT_FOUND.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "nope" })),
        ))
        .await;
    assert_eq!(res.status, 404);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40417);

    // A missing catalog_id is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({})),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40001);

    // Every unimportable-entry branch carries CATALOG_IMPORT_INVALID.
    for catalog_id in ["gateway", "bedrock", "empty"] {
        let res = server
            .handle_request(&catalog_request(
                "POST",
                "/api/v1/providers:import_catalog",
                Some(&json!({ "catalog_id": catalog_id })),
            ))
            .await;
        assert_eq!(res.status, 400, "{catalog_id}");
        let body: Value = serde_json::from_slice(&res.body).unwrap();
        assert_eq!(body["code"], 40004, "{catalog_id}");
    }

    // An entry that needs an endpoint is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "gateway" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("requires a base_url")
    );

    // A proprietary-SDK entry is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "bedrock" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);

    // An entry with no importable models is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "empty" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);
    assert!(
        body["msg"]
            .as_str()
            .unwrap()
            .contains("no importable models")
    );

    // An unusable target id is a validation failure.
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme", "id": "bad id!" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40004);

    // An OAuth-managed provider refuses the import.
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.push_str("\n[providers.acme]\ntype = \"openai\"\noauth = { provider = \"acme\" }\n");
    std::fs::write(&config_path, text).unwrap();
    let res = server
        .handle_request(&catalog_request(
            "POST",
            "/api/v1/providers:import_catalog",
            Some(&json!({ "catalog_id": "acme" })),
        ))
        .await;
    assert_eq!(res.status, 400);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["code"], 40003);
}

#[tokio::test]
async fn provider_create_publishes_config_changed_on_the_global_lane() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "default_model = \"\"\n").unwrap();
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config_write_path(config_path.clone());
    let mut sub = server.hub().attach();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/providers".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({
                "id": "kimi-code",
                "type": "openai",
                "api_key": "sk-test",
                "base_url": "https://example.test/v1",
                "default_model": "k3",
                "models": [{ "model": "k3", "max_context_size": 200000 }]
            }))
            .unwrap(),
        })
        .await;
    assert_eq!(res.status, 201);

    let ev = sub.recv().await.unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.config.changed");
    let crate::events::EngineEvent::ConfigChanged {
        changed_fields,
        config,
    } = &ev.event
    else {
        panic!("expected ConfigChanged, got {:?}", ev.event);
    };
    assert_eq!(changed_fields, &vec!["providers".to_string()]);
    // The payload carries the post-write config, so the client can fold
    // it instead of re-fetching.
    assert_eq!(config["providers"]["kimi-code"]["type"], "openai");
    assert_eq!(config["default_model"], "kimi-code/k3");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn model_catalog_changed_publishes_on_the_global_lane() {
    let server = HttpServer::in_memory().unwrap();
    let mut sub = server.hub().attach();
    server.publish_model_catalog_changed(&json!({
            "changed": [{ "provider_id": "kimi-code", "provider_name": "Kimi Code", "added": 2, "removed": 1 }],
            "unchanged": ["other"],
            "failed": [],
        }));

    let ev = sub.recv().await.unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.model_catalog.changed");
    let crate::events::EngineEvent::Custom(value) = &ev.event else {
        panic!("expected Custom event");
    };
    assert_eq!(value["changed"][0]["added"], 2);
    assert_eq!(value["unchanged"][0], "other");
    assert!(value["failed"].as_array().unwrap().is_empty());
}

/// The v3 `config.warning` entity's producer: a `config.toml` with a
/// malformed `[models]` entry reaches the global lane as
/// `event.config.warning`, the event the global translator turns into the
/// entity. The warning is captured at config load, which happens before
/// the server exists, so this exercises the staging seam too.
#[tokio::test]
async fn a_malformed_model_entry_reaches_the_global_lane_as_a_config_warning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        r#"
default_model = "typo"

[models.typo]
provider = "kimi"
max_context_size = 1000
"#,
    )
    .unwrap();
    let config = crate::config::KimiConfig::from_file(&path).unwrap();
    assert_eq!(config.config_warnings.len(), 1);

    let server = HttpServer::in_memory().unwrap();
    let mut sub = server.hub().attach();
    let server = server.with_config(config);
    // The seeded config is what `config()` reads back, so the warning list
    // and the config view come from one load rather than two that could
    // differ.
    assert_eq!(server.config().await.config_warnings.len(), 1);

    let ev = sub.recv().await.unwrap();
    assert_eq!(&*ev.session_id, "global");
    assert_eq!(ev.event.event_type(), "event.config.warning");
    let crate::events::EngineEvent::Custom(value) = &ev.event else {
        panic!("expected Custom event, got {:?}", ev.event);
    };
    // The shape the global translator reads: `warnings[].message`.
    let warnings = value["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1, "{value}");
    assert!(
        warnings[0]["message"]
            .as_str()
            .unwrap()
            .contains("[models] entry 'typo' is missing"),
        "{value}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A config that loaded cleanly must not publish an empty warning entity
/// *at startup*: a fresh daemon has no stale advisory to clear, and the
/// entity would sit in the global lane's replay ring saying nothing.
/// (The reload path is the opposite — see the test below.)
#[tokio::test]
async fn a_clean_config_publishes_no_config_warning_at_startup() {
    let server = HttpServer::in_memory()
        .unwrap()
        .with_config(Default::default());

    // Nothing is on the global lane, so the subscription's replay is empty
    // and `recv` only returns on a publish. Any event would prove the
    // publish happened; a timeout proves none did.
    let mut sub = server.hub().attach();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv())
            .await
            .is_err(),
        "a clean config must not broadcast a config.warning"
    );
}

#[tokio::test]
async fn test_http_provider_refresh_discovers_managed_models() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // A mock `GET /models` whose payload the test can swap between calls.
    let payload = Arc::new(std::sync::Mutex::new(json!({
        "data": [
            { "id": "k3", "limit": { "context": 200000 }, "display_name": "K3", "reasoning": { "type": "both" } },
            { "id": "k3-fast", "limit": { "context": 128000 } }
        ]
    })));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let payload_for_server = payload.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let payload = payload_for_server
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let _ = sock.read(&mut buffer).await;
                let body = payload.to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });

    // Cached OAuth credential the refresh authenticates with.
    let credentials =
        std::env::temp_dir().join(format!("kimi-refresh-creds-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&credentials).unwrap();
    std::fs::write(
        credentials.join("kimi.json"),
        json!({
            "access_token": "token-1",
            "refresh_token": "refresh-1",
            "expires_at": chrono::Utc::now().timestamp() + 3600,
            "scope": "kimi",
            "token_type": "Bearer",
            "expires_in": 3600,
        })
        .to_string(),
    )
    .unwrap();

    let dir = std::env::temp_dir().join(format!("kimi-refresh-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
            &config_path,
            format!(
                "default_model = \"kimi-code/k3\"\n\n[providers.\"managed:kimi-code\"]\ntype = \"kimi\"\nbase_url = \"http://{addr}/v1\"\noauth = {{ provider = \"managed:kimi-code\" }}\n"
            ),
        )
        .unwrap();

    let oauth = crate::server::oauth::OAuthManager::with_hosts(
        format!("http://{addr}"),
        format!("http://{addr}/v1"),
        Some(credentials.clone()),
    );
    let server = HttpServer::in_memory()
        .unwrap()
        .with_oauth_manager(Arc::new(oauth))
        .with_config_write_path(config_path.clone());

    fn request(path: &str) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: path.into(),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    // 1. A targeted refresh discovers both models and writes the aliases.
    let res = server
        .handle_request(&request("/api/v1/providers/managed%3Akimi-code:refresh"))
        .await;
    assert_eq!(res.status, 200);
    let result: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(result["changed"][0]["provider_id"], "managed:kimi-code");
    assert_eq!(result["changed"][0]["provider_name"], "Kimi Code");
    assert_eq!(result["changed"][0]["added"], 2);
    assert_eq!(result["changed"][0]["removed"], 0);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("[models.\"kimi-code/k3\"]"), "{text}");
    assert!(text.contains("[models.\"kimi-code/k3-fast\"]"), "{text}");
    assert!(text.contains("provider = \"managed:kimi-code\""), "{text}");

    // 2. The same payload again is a no-op.
    let res = server
        .handle_request(&request("/api/v1/providers/managed%3Akimi-code:refresh"))
        .await;
    let result: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(result["unchanged"], json!(["managed:kimi-code"]));
    assert!(result["changed"].as_array().unwrap().is_empty());

    // 3. A model the endpoint stops listing is reported and removed.
    *payload
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = json!({
        "data": [{ "id": "k3", "limit": { "context": 200000 } }]
    });
    let res = server
        .handle_request(&request("/api/v1/providers:refresh"))
        .await;
    let result: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(result["changed"][0]["removed"], 1);
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(!text.contains("kimi-code/k3-fast"), "{text}");
    assert!(text.contains("default_model = \"kimi-code/k3\""), "{text}");

    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&credentials).ok();
}
