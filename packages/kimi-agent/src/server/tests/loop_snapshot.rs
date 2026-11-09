use super::super::*;

fn queued_prompt(prompt_id: &str) -> Value {
    json!({
        "prompt_id": prompt_id,
        "status": "submitted",
        "kind": "user",
        "content": [],
    })
}

/// A13: the snapshot folds the prompt queue (tracked prompts vs notification
/// wakes) into one read; nothing here needs a running turn.
#[test]
fn loop_snapshot_folds_queue_and_separates_notifications() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let server = HttpServer::new(store);

    let idle = server.loop_snapshot("sess-loop");
    assert_eq!(idle.state, "idle");
    assert!(idle.active_turn_id.is_none());
    assert!(idle.active_prompt_id.is_none());
    assert!(idle.queued_prompt_ids.is_empty());
    assert_eq!(idle.notification_count, 0);
    assert!(!idle.paused);
    assert!(!idle.has_pending_requests);
    assert!(idle.turn.is_none());

    let (_, run) = server.prompt_queue.admit(
        "sess-loop",
        queued_prompt("p-active"),
        "first".into(),
        Vec::new(),
        None,
    );
    assert!(run.is_some(), "the first prompt starts immediately");
    let _ = server.prompt_queue.admit(
        "sess-loop",
        queued_prompt("p-queued"),
        "second".into(),
        Vec::new(),
        None,
    );
    server
        .prompt_queue
        .admit_wake("sess-loop", "task done".into(), json!({ "kind": "task" }));

    let snapshot = server.loop_snapshot("sess-loop");
    assert_eq!(snapshot.state, "idle", "no engine turn is running");
    assert_eq!(snapshot.active_prompt_id.as_deref(), Some("p-active"));
    assert_eq!(snapshot.queued_prompt_ids, vec!["p-queued".to_string()]);
    assert_eq!(
        snapshot.notification_count, 1,
        "an untracked wake is a notification, not a queued prompt id"
    );
    // camelCase on the wire, matching v2's snapshot shape.
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(wire["queuedPromptIds"][0], "p-queued");
    assert_eq!(wire["notificationCount"], 1);
    assert!(wire.get("turn").is_none());
}

/// A13: the machine's three-value phase plus retry/tool detail.
#[test]
fn loop_turn_snapshot_maps_activity_phases() {
    let retrying = activity::AgentPhase::Retrying {
        turn_id: 7,
        step: 2,
        step_id: "step-2".into(),
        failed_attempt: 1,
        next_attempt: 2,
        max_attempts: 3,
        delay_ms: 500,
        error_name: Some("overloaded".into()),
        status_code: Some(529),
        since: 10,
    };
    let turn = LoopTurnSnapshot::from_phase(Some(7), &retrying, 2, true, Some(5)).unwrap();
    assert_eq!(turn.turn_id, 7);
    assert_eq!(turn.phase, "retrying");
    assert!(turn.ending);
    assert_eq!(turn.ending_reason, Some("aborted"));
    assert_eq!(turn.since, Some(5));
    let retry = turn.retry.expect("retrying carries the retry snapshot");
    assert_eq!(retry.failed_attempt, 1);
    assert_eq!(retry.next_attempt, 2);
    assert_eq!(retry.status_code, Some(529));

    let tool_call = activity::AgentPhase::ToolCall {
        turn_id: 7,
        step: 3,
        step_id: "step-3".into(),
        tool_call_id: "call-1".into(),
        name: "Bash".into(),
        since: 11,
    };
    let turn = LoopTurnSnapshot::from_phase(Some(7), &tool_call, 3, false, Some(5)).unwrap();
    assert_eq!(turn.phase, "tool_call");
    assert_eq!(turn.active_tool_calls.len(), 1);
    assert_eq!(turn.active_tool_calls[0].name, "Bash");
    assert!(turn.retry.is_none());
    assert!(!turn.ending);

    // `streaming` and `awaiting_approval` sit inside the machine's running.
    let streaming = activity::AgentPhase::Streaming {
        turn_id: 7,
        step: 3,
        step_id: "step-3".into(),
        stream: activity::StreamKind::Assistant,
        since: 12,
    };
    assert_eq!(
        LoopTurnSnapshot::from_phase(Some(7), &streaming, 3, false, Some(5))
            .unwrap()
            .phase,
        "running"
    );

    // Without a turn id there is no turn to report.
    assert!(
        LoopTurnSnapshot::from_phase(None, &activity::AgentPhase::Idle, 0, false, None).is_none()
    );
}
