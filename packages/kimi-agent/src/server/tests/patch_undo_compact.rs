use super::super::*;
use super::common::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_session_patch_and_undo() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

    // 1. Create a session
    let create_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: "/api/v1/sessions".into(),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "title": "Initial Title" })).unwrap(),
        })
        .await;
    assert_eq!(create_res.status, 200);
    let create_val: Value = serde_json::from_slice(&create_res.body).unwrap();
    let sid = create_val["id"].as_str().unwrap();

    // 2. Patch session title and custom metadata using RFC 6902 JSON patch
    let patch_ops = json!([
        { "op": "replace", "path": "/title", "value": "Updated Title" },
        { "op": "add", "path": "/metadata/custom_tag", "value": "production" }
    ]);

    let patch_res = server
        .handle_request(&HttpRequest {
            method: "PATCH".into(),
            path: format!("/api/v1/sessions/{sid}"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&patch_ops).unwrap(),
        })
        .await;
    assert_eq!(patch_res.status, 200);
    let patch_val: Value = serde_json::from_slice(&patch_res.body).unwrap();
    assert_eq!(patch_val["session"]["title"], "Updated Title");
    assert_eq!(patch_val["session"]["metadata"]["custom_tag"], "production");
    assert!(patch_val["applied_patch"].is_object() || patch_val["applied_patch"].is_array());
    assert!(patch_val["undo_patch"].is_object() || patch_val["undo_patch"].is_array());

    // 3. Partial object patch via POST :patch
    let partial_update = json!({
        "title": "Object Patched Title",
        "metadata": { "client_version": "1.2.3" }
    });
    let post_patch_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:patch"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&partial_update).unwrap(),
        })
        .await;
    assert_eq!(post_patch_res.status, 200);
    let post_patch_val: Value = serde_json::from_slice(&post_patch_res.body).unwrap();
    assert_eq!(post_patch_val["session"]["title"], "Object Patched Title");
    assert_eq!(
        post_patch_val["session"]["metadata"]["client_version"],
        "1.2.3"
    );
    assert_eq!(
        post_patch_val["session"]["metadata"]["custom_tag"],
        "production"
    );

    // 4. Undo the last patch
    let undo_res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo_patch"),
            query: None,
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(undo_res.status, 200);
    let undo_val: Value = serde_json::from_slice(&undo_res.body).unwrap();
    assert_eq!(undo_val["session"]["title"], "Updated Title");
}

/// `POST :undo` with `revert_files` reverts the workspace files of the
/// turns it actually removed —the file history is keyed by turn *number*,
/// so passing the undo *count* used to restore the wrong turn.
#[tokio::test]
async fn test_http_undo_reverts_files_of_the_undone_turn() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-undo-files";
    store.create_session(sid, None).unwrap();
    store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": temp.path().display().to_string() }),
        )
        .unwrap();

    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store.save_turn(sid, "t1", 1, &msgs, None, None).unwrap();
    store.save_turn(sid, "t2", 2, &msgs, None, None).unwrap();
    std::fs::write(temp.path().join("a.txt"), "after-1").unwrap();
    std::fs::write(temp.path().join("b.txt"), "after-2").unwrap();
    store
        .record_file_change(sid, 1, "a.txt", Some("before-1"), Some("after-1"))
        .unwrap();
    store
        .record_file_change(sid, 2, "b.txt", Some("before-2"), Some("after-2"))
        .unwrap();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1, "revert_files": true })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["undone"], 1);

    // Only the removed turn's file is restored.
    assert_eq!(
        std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "before-2"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "after-1"
    );
}

/// `/undo` used to delete transcript rows while leaving `todo` / `plan` on
/// disk describing work the model could no longer see. These cover the
/// checkpoint stack being unwound LIFO, and — just as important — an *empty*
/// stack staying a non-error so sessions that never checkpointed are
/// unaffected.
#[tokio::test]
async fn undo_restores_state_domains_from_the_checkpoint_stack() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-undo-state";
    store.create_session(sid, None).unwrap();
    store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": temp.path().display().to_string() }),
        )
        .unwrap();

    // Turn 1 pushes the anchor, then mutates the domain. Undo state is
    // session-scoped now (v2 agent scope), so the checkpoints live in the
    // session store.
    let state = crate::storage::StateStore::for_session(temp.path(), sid).unwrap();
    state
        .write_domain("todo", &json!([{ "id": "T1" }]))
        .unwrap();
    state.checkpoint().unwrap();
    state
        .write_domain("todo", &json!([{ "id": "T1" }, { "id": "T2" }]))
        .unwrap();

    // Turn 2 pushes a second anchor and mutates again.
    state.checkpoint().unwrap();
    state
        .write_domain(
            "todo",
            &json!([{ "id": "T1" }, { "id": "T2" }, { "id": "T3" }]),
        )
        .unwrap();
    assert_eq!(state.checkpoint_depth(), 2);

    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store.save_turn(sid, "t1", 1, &msgs, None, None).unwrap();
    store.save_turn(sid, "t2", 2, &msgs, None, None).unwrap();

    // Undoing both turns must unwind both snapshots and land on the
    // pre-turn-1 value, not merely one level up.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 2 })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["undone"], 2);

    assert_eq!(state.checkpoint_depth(), 0, "both snapshots are consumed");
    assert_eq!(
        state.read_domain("todo"),
        Some(json!([{ "id": "T1" }])),
        "LIFO unwind must reach the earliest anchor, not stop one level short"
    );
}

/// An empty checkpoint stack is normal, not a failure: sessions that never
/// went through `StateStoreCallbacks::checkpoint` must still undo cleanly.
/// Pinned because the wired-up route now touches the state store at all.
#[tokio::test]
async fn undo_succeeds_when_no_checkpoint_was_ever_taken() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-undo-no-ckpt";
    store.create_session(sid, None).unwrap();
    store
        .put_state(
            "metadata",
            sid,
            &json!({ "cwd": temp.path().display().to_string() }),
        )
        .unwrap();

    let msgs = vec![crate::turn_loop::types::LLMMessage::user("hi")];
    store.save_turn(sid, "t1", 1, &msgs, None, None).unwrap();

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1 })).unwrap(),
        })
        .await;
    assert_eq!(
        res.status, 200,
        "an undo inside the available anchors must not require a checkpoint"
    );

    // Asking for more anchors than exist is unavailable, independent of
    // the checkpoint stack.
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 3 })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 400, "insufficient undo count must be refused");
}

#[tokio::test]
async fn test_http_compact_without_summarizer_preserves_history() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let server = HttpServer::with_hub(store.clone(), hub.clone());
    let sid = "sess-compact-http";
    store.create_session(sid, None).unwrap();
    for i in 1..=12 {
        let filler = "x".repeat(400);
        store
            .save_turn(
                sid,
                &format!("t{i}"),
                i,
                &[
                    crate::turn_loop::types::LLMMessage::user(format!("u{i} {filler}")),
                    crate::turn_loop::types::LLMMessage::assistant(format!("a{i} {filler}")),
                ],
                None,
                None,
            )
            .unwrap();
    }

    let original_history = serde_json::to_value(store.load_session_history(sid).unwrap()).unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let collector = seen.clone();
    hub.bus_for(sid).subscribe(move |event| {
        if let crate::events::types::EngineEvent::Custom(value) = event {
            collector
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(value.clone());
        }
    });

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:compact"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "instruction": "keep decisions" })).unwrap(),
        })
        .await;
    // No engine is bound, so the mandatory-summary prerequisites fail
    // before any history is touched: a 503 naming the configuration
    // problem, not a mid-request 500. History is untouched and no
    // completion event is published.
    assert_eq!(res.status, 503);
    assert!(
        serde_json::from_slice::<Value>(&res.body).unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("no engine"),
    );
    assert_eq!(
        serde_json::to_value(store.load_session_history(sid).unwrap()).unwrap(),
        original_history,
    );
    let events = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "compaction.completed")
    );
}

/// `POST :compact` folds the omitted prefix into the session model's
/// written summary. The endpoint used to write the fixed placeholder even
/// with an engine attached, so the same action produced different history
/// depending on whether the client went through REST or NAPI.
#[tokio::test]
async fn the_compact_route_writes_the_llm_summary_not_the_placeholder() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let sid = "sess-compact-summary";
    store.create_session(sid, None).unwrap();
    // Give the session a workspace and seed its todo domain: the route
    // appends the rendered list to the summary (v2 `postProcessSummary`).
    let ws = tempfile::tempdir().unwrap();
    let workspace = store
        .create_workspace(&ws.path().to_string_lossy(), None)
        .unwrap();
    store
        .create_session_with_workspace(sid, None, Some(&workspace.id))
        .unwrap();
    let state = crate::storage::StateStore::for_workspace(ws.path()).unwrap();
    state
        .write_domain(
            "todo",
            &json!([{ "id": "t1", "title": "ship the fix", "status": "in_progress" }]),
        )
        .unwrap();
    for i in 1..=12 {
        let filler = "x".repeat(400);
        store
            .save_turn(
                sid,
                &format!("t{i}"),
                i,
                &[
                    crate::turn_loop::types::LLMMessage::user(format!("u{i} {filler}")),
                    crate::turn_loop::types::LLMMessage::assistant(format!("a{i} {filler}")),
                ],
                None,
                None,
            )
            .unwrap();
    }
    // A trailing user message is never a safe split point
    // (`can_split_after` refuses user messages), so the fold stops one
    // short of the end — exactly the distinction between `compactedCount`
    // (messages folded) and the pre-compact length.
    store
        .save_turn(
            sid,
            "t-tail",
            13,
            &[crate::turn_loop::types::LLMMessage::user(
                "a trailing question".to_string(),
            )],
            None,
            None,
        )
        .unwrap();
    let pre_len = store.load_session_history(sid).unwrap().len();

    // A provider-backed spec: `build_llm_for_spec` resolves it to a
    // host-proxy LLM, which is the seam the factory below answers.
    let engine = ServerEngine::new(
        crate::pipeline::PipelineSpec {
            system_prompt: "sys".into(),
            model_name: "test-model".into(),
            providers: vec![crate::pipeline::PipelineProvider {
                name: "test".into(),
                system_prompt: "sys".into(),
                model: "test-model".into(),
                native: None,
            }],
            native_llm: None,
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
    engine.set_host_factory(Arc::new(|_session_id| {
        Arc::new(SummarizingHost {
            summary: "The user sent twelve filler messages.".to_string(),
        })
    }));
    let server = HttpServer::with_hub(store.clone(), hub).with_engine(engine);

    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:compact"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({})).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);
    let body: Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["compacted"], true);
    assert_eq!(
        body["compactedCount"].as_u64().unwrap() as usize,
        pre_len - 1,
        "compactedCount is the folded range (first kept index), not the pre-compact length"
    );

    let history = store.load_session_history(sid).unwrap();
    let summary_message = history
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the endpoint must store the model's summary, not the placeholder");
    let summary_pos = summary_message
        .content
        .find("The user sent twelve filler messages.")
        .expect("the model's summary must be stored verbatim");
    let todos_pos = summary_message
        .content
        .find("## TODO List")
        .expect("the todo list must ride after the summary");
    assert!(
        summary_pos < todos_pos,
        "v2 postProcessSummary appends the todos after the summary"
    );
    assert!(
        summary_message.content.contains("ship the fix"),
        "the appended list is the session's own todo state"
    );
}
