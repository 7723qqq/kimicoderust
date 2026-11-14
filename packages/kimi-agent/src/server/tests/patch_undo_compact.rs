use super::super::*;
use super::common::*;
use std::collections::HashMap;

#[tokio::test]
async fn test_http_session_patch_and_undo() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());

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

    assert_eq!(
        std::fs::read_to_string(temp.path().join("b.txt")).unwrap(),
        "before-2"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("a.txt")).unwrap(),
        "after-1"
    );
}

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

    let state = crate::storage::StateStore::for_session(temp.path(), sid).unwrap();
    state
        .write_domain("todo", &json!([{ "id": "T1" }]))
        .unwrap();
    state.checkpoint().unwrap();
    state
        .write_domain("todo", &json!([{ "id": "T1" }, { "id": "T2" }]))
        .unwrap();

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
        state.read_domain_state("todo").into_result().unwrap(),
        Some(json!([{ "id": "T1" }])),
        "LIFO unwind must reach the earliest anchor, not stop one level short"
    );
}

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
async fn test_http_undo_appends_the_v2_switch_triple_to_the_journal() {
    use crate::events::EngineEvent;
    use crate::session::wire_journal::WireJournal;
    use crate::turn_loop::types::LLMMessage;

    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteSessionStore::open(temp.path().join("sessions.db")).unwrap());
    let workspace = store.create_workspace("/work/undo-journal", None).unwrap();
    let sid = "sess-undo-journal";
    store
        .create_session_with_workspace(sid, None, Some(&workspace.id))
        .unwrap();
    let journal = WireJournal::new(temp.path(), store.clone());
    for (turn, prompt, answer) in [(1i64, "first", "answer one"), (2, "second", "answer two")] {
        store
            .save_turn(
                sid,
                &format!("turn-{turn}"),
                turn as u32,
                &[LLMMessage::user(prompt), LLMMessage::assistant(answer)],
                None,
                Some(&json!({ "kind": "user" })),
            )
            .unwrap();
        for event in [
            json!({
                "type": "turn.prompt",
                "turnId": turn,
                "input": [{ "type": "text", "text": prompt }],
                "origin": { "kind": "user" },
            }),
            json!({
                "type": "llm.step.begin",
                "agent_id": "main",
                "turn_id": format!("turn-{turn}"),
                "step": 1,
                "step_id": format!("turn-{turn}-step-1"),
            }),
            json!({
                "type": "llm.delta",
                "agent_id": "main",
                "part": { "type": "text", "text": answer },
            }),
            json!({
                "type": "llm.step.end",
                "agent_id": "main",
                "turn_id": format!("turn-{turn}"),
                "step": 1,
                "finish_reason": "stop",
            }),
        ] {
            journal.append(sid, &EngineEvent::Custom(event), turn * 100);
        }
    }
    let before = store.load_session_history(sid).unwrap();
    assert_eq!(before.len(), 4, "{before:?}");

    let server = HttpServer::new(store.clone());
    let res = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1 })).unwrap(),
        })
        .await;
    assert_eq!(res.status, 200);

    let after = store.load_session_history(sid).unwrap();
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(after[0].content, "first");
    assert_eq!(after[1].content, "answer one");
    assert_eq!(
        store.load_session_messages(sid).unwrap().len(),
        4,
        "the undo must keep the sqlite projection rows"
    );
    assert_eq!(
        store.list_turns(sid).unwrap().len(),
        2,
        "the undo must keep the turn rows"
    );
    assert_eq!(
        store.next_turn_number(sid).unwrap(),
        3,
        "turn ids stay monotonic after an append-only undo"
    );
    assert_eq!(
        store
            .refresh_last_prompt_after_undo(sid)
            .unwrap()
            .as_deref(),
        Some("first")
    );

    store
        .fork_session(sid, "sess-undo-fork", None, None)
        .unwrap();
    let forked = store.load_session_history("sess-undo-fork").unwrap();
    assert_eq!(
        forked.len(),
        2,
        "the fork must inherit only the active turns: {forked:?}"
    );

    let path = store.wire_journal_path(sid, "main").unwrap();
    let records: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let non_metadata: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] != "metadata")
        .collect();
    let types: Vec<&str> = non_metadata
        .iter()
        .map(|record| record["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        &types[types.len() - 3..],
        ["agent.switched", "context.undo", "context.undone"]
    );
    let switched = non_metadata[non_metadata.len() - 3];
    assert_eq!(switched["branch"], "b1");
    assert_eq!(switched["turns"], 1);
    assert_eq!(switched["legacyUndoLine"], json!(non_metadata.len() as i64));
    assert_eq!(switched["base"]["branch"], "main");
    assert_eq!(switched["base"]["line"], 7);

    let second = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1 })).unwrap(),
        })
        .await;
    assert_eq!(second.status, 200, "{:?}", second.body);
    let records: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let non_metadata: Vec<&Value> = records
        .iter()
        .filter(|record| record["type"] != "metadata")
        .collect();
    let switched = non_metadata[non_metadata.len() - 3];
    assert_eq!(switched["branch"], "b2");
    assert_eq!(switched["base"]["branch"], "main");
    assert_eq!(switched["base"]["line"], 2);
    assert_eq!(switched["legacyUndoLine"], json!(non_metadata.len() as i64));
}

#[tokio::test]
async fn operation_log_drives_file_history_and_undo_routes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@e.test"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);

    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store.clone());
    let sid = "sess-operation-log";
    store.create_session(sid, None).unwrap();
    store
        .set_session_environment(
            sid,
            Some(crate::environment::LOCAL_ENVIRONMENT_ID),
            Some(root.to_string_lossy().as_ref()),
        )
        .unwrap();
    store
        .put_state("metadata", sid, &json!({ "cwd": root.to_string_lossy() }))
        .unwrap();
    for (id, turn, text) in [("t1", 1, "first"), ("t2", 2, "second")] {
        store
            .save_turn(
                sid,
                id,
                turn,
                &[
                    crate::turn_loop::types::LLMMessage::user(text),
                    crate::turn_loop::types::LLMMessage::assistant("done"),
                ],
                None,
                None,
            )
            .unwrap();
    }

    let operations =
        crate::features::operationLog::operationService::AgentOperationService::new(
            root.to_string_lossy(),
            sid,
            "main",
        )
        .with_flag_enabled(true);
    operations
        .capture(
            &crate::features::operationLog::operationService::CaptureContext {
                description: "Writing a.txt".to_string(),
                origin: crate::features::operationLog::operationService::CaptureOrigin {
                    session_id: sid.to_string(),
                    agent_id: "main".to_string(),
                    turn_id: Some(2),
                    tool_call_id: Some("call-2".to_string()),
                    tool_name: "Write".to_string(),
                },
                repo_mutation: None,
            },
            || async {
                std::fs::write(root.join("a.txt"), "two\n").unwrap();
                Ok(())
            },
        )
        .await
        .unwrap();
    assert_eq!(operations.list_operations(None).await.unwrap().len(), 1);
    assert_eq!(
        fs_routes::resolve_session_workdir(&store, sid).as_deref(),
        Some(root)
    );

    let _env_lock = crate::env::ENV_TEST_LOCK.lock().await;
    let previous_flag = std::env::var_os("KIMI_CODE_EXPERIMENTAL_OPERATION_LOG");
    unsafe { std::env::set_var("KIMI_CODE_EXPERIMENTAL_OPERATION_LOG", "1") };
    assert!(crate::features::operationLog::operationService::operation_log_enabled());
    let changes_response = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/file-history/changes"),
            query: Some("turn_id=2".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(changes_response.status, 200, "{:?}", changes_response.body);
    let changes: Value = serde_json::from_slice(&changes_response.body).unwrap();
    assert_eq!(changes["recorded"], true, "{changes}");
    assert_eq!(changes["changes"][0]["path"], "a.txt");
    assert_eq!(changes["changes"][0]["status"], "modified");

    let content_response = server
        .handle_request(&HttpRequest {
            method: "GET".into(),
            path: format!("/api/v1/sessions/{sid}/file-history/content"),
            query: Some("turn_id=2&path=a.txt".into()),
            headers: HashMap::new(),
            body: Vec::new(),
        })
        .await;
    assert_eq!(content_response.status, 200, "{:?}", content_response.body);
    let content: Value = serde_json::from_slice(&content_response.body).unwrap();
    assert_eq!(
        content["content"]
            .as_str()
            .map(|text| text.replace("\r\n", "\n")),
        Some("two\n".to_string())
    );

    let undo_response = server
        .handle_request(&HttpRequest {
            method: "POST".into(),
            path: format!("/api/v1/sessions/{sid}:undo"),
            query: None,
            headers: HashMap::new(),
            body: serde_json::to_vec(&json!({ "count": 1, "revert_files": true })).unwrap(),
        })
        .await;
    match previous_flag {
        Some(value) => unsafe { std::env::set_var("KIMI_CODE_EXPERIMENTAL_OPERATION_LOG", value) },
        None => unsafe { std::env::remove_var("KIMI_CODE_EXPERIMENTAL_OPERATION_LOG") },
    }
    drop(_env_lock);
    assert_eq!(undo_response.status, 200, "{:?}", undo_response.body);
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt"))
            .unwrap()
            .replace("\r\n", "\n"),
        "one\n"
    );
    assert_eq!(store.load_session_history(sid).unwrap().len(), 2);
    operations.dispose().await;
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

#[tokio::test]
async fn the_compact_route_writes_the_llm_summary_not_the_placeholder() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let hub = Arc::new(EventHub::new());
    let sid = "sess-compact-summary";
    store.create_session(sid, None).unwrap();

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
            llm_request_byte_budget: None,
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
