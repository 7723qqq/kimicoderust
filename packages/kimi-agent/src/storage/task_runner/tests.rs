use super::*;
use tempfile::TempDir;

/// Captured sink events: `(session lane, payload)` pairs.
type CapturedEvents = Arc<std::sync::Mutex<Vec<(Option<String>, Value)>>>;

fn runner() -> (TempDir, Arc<TaskRunner>) {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    (tmp, Arc::new(TaskRunner::new(Some(store))))
}

fn stored_tasks(runner: &TaskRunner) -> Value {
    runner
        .store
        .as_ref()
        .unwrap()
        .read_domain_state("task")
        .into_result()
        .unwrap()
        .unwrap_or(Value::Array(vec![]))
}

/// A task persisted as still-running by a previous process is marked
/// `lost` on reconcile, and the run produces the previous-session reminder
/// (v2 `markLoadedTasksLost` + `appendPreviousSessionTasksReminder`).
#[test]
fn reconcile_marks_orphaned_tasks_lost_and_reminds_once() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    // A task the previous process left mid-flight.
    store
            .write_domain(
                "task",
                &json!([{ "taskId": "task-1", "description": "run tests", "status": "running", "startedAt": 1u64 }]),
            )
            .unwrap();
    let runner = TaskRunner::new(Some(store));

    let text = runner
        .reconcile_previous_session(&HashSet::new())
        .expect("the orphaned task must be reported");
    assert!(
        text.contains("lost contact"),
        "reminder must announce the lost contact: {text}"
    );
    assert!(
        text.contains("- task-1 \"run tests\""),
        "reminder must list the task: {text}"
    );
    assert!(
        text.contains("Don't assume any of them completed"),
        "reminder must carry the v2 closing line: {text}"
    );

    let stored = stored_tasks(&runner);
    assert_eq!(
        stored[0].get("status").and_then(Value::as_str),
        Some("lost"),
        "the orphaned task is persisted as lost: {stored}"
    );
    assert_eq!(
        stored[0].get("resumeReminded").and_then(Value::as_bool),
        Some(true),
        "the one-shot marker is persisted: {stored}"
    );

    // Idempotent: a second reconcile reports nothing.
    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_none(),
        "the reminder fires once per task"
    );
}

/// A lost subagent names the `Agent(resume=…)` that picks its context back
/// up, rather than the generic kind-only line (v2
/// `previousSessionTaskLine`'s subagent branch).
#[test]
fn reconcile_names_the_resume_hint_for_a_lost_subagent() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    store
        .write_domain(
            "task",
            &json!([{
                "taskId": "task-7",
                "description": "Resume researcher: audit",
                "kind": "subagent",
                "agentId": "agent-42",
                "status": "running",
                "startedAt": 1u64
            }]),
        )
        .unwrap();
    let runner = TaskRunner::new(Some(store));
    let text = runner
        .reconcile_previous_session(&HashSet::new())
        .expect("reported");
    assert!(
        text.contains("(subagent) — resume it with Agent(resume=\"agent-42\""),
        "the lost subagent carries its resume hint: {text}"
    );
}

/// A task *this process* spawned live sits in the domain as `running`
/// (`spawn_task_with_meta` persists it), so reconcile must not treat it
/// as a previous-session loss (v2 keeps live tasks out of `ghosts`;
/// `markLoadedTasksLost` only sees load-time ghosts) — regression test
/// for the live-task misreport.
#[tokio::test]
async fn reconcile_does_not_misreport_a_live_in_process_task() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: None,
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-live".into(),
            "long build".into(),
            std::future::pending::<TaskOutcome>(),
        )
        .unwrap();

    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_none(),
        "a live task spawned by this process must not be reported as lost"
    );
    let stored = stored_tasks(&runner);
    assert_eq!(
        stored[0].get("status").and_then(Value::as_str),
        Some("running"),
        "the live task keeps its running status: {stored}"
    );
}

/// The task lifecycle hook wiring: `TaskStarted` at spawn (matched on the
/// task kind) and `Notification` at settle (matched on the terminal
/// status) — remove either dispatch site and its capture file stays
/// empty.
#[tokio::test]
async fn task_hooks_fire_on_spawn_and_settle() {
    use crate::tools::external_hooks::capture;
    let (_tmp, runner) = runner();
    let Some(dir) = capture::dir() else {
        return;
    };
    let started = dir.path().join("task-started.json");
    let notified = dir.path().join("task-notification.json");
    runner.set_hook_guard(Arc::new(crate::tools::external_hooks::HookGuard::new(
        vec![
            capture::hook("TaskStarted", "subagent", &started),
            capture::hook("Notification", "completed", &notified),
        ],
    )));
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: None,
                kind: "subagent",
                subagent_type: Some("explore"),
                agent_id: None,
                timeout_ms: None,
            },
            "task-hook".into(),
            "explore the tree".into(),
            async { TaskOutcome::Completed("done".into()) },
        )
        .unwrap();

    let started_payload = capture::wait(&started).await;
    assert_eq!(started_payload["hook_event_name"], "TaskStarted");
    assert_eq!(started_payload["task_id"], "task-hook");
    assert_eq!(started_payload["kind"], "subagent");
    assert_eq!(started_payload["description"], "explore the tree");
    assert_eq!(started_payload["status"], "running");

    let notified_payload = capture::wait(&notified).await;
    assert_eq!(notified_payload["hook_event_name"], "Notification");
    assert_eq!(notified_payload["task_id"], "task-hook");
    assert_eq!(notified_payload["status"], "completed");
    assert_eq!(notified_payload["sink"], "context");
}

#[test]
fn reconcile_ignores_tasks_that_already_settled() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    store
            .write_domain(
                "task",
                &json!([{ "taskId": "task-done", "description": "finished", "status": "completed", "startedAt": 1u64 }]),
            )
            .unwrap();
    let runner = TaskRunner::new(Some(store));
    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_none(),
        "a terminal task is not a previous-session loss"
    );
    let stored = stored_tasks(&runner);
    assert_eq!(
        stored[0].get("status").and_then(Value::as_str),
        Some("completed"),
        "a settled task keeps its status: {stored}"
    );
}

/// A task an earlier reminder already named (scanned off the
/// conversation's injection history) is never announced again — but its
/// loss is still persisted with the marker (v2
/// `hasPreviousSessionReminder` gates only the report).
#[test]
fn reconcile_stays_silent_for_a_task_the_history_already_announced() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    store
            .write_domain(
                "task",
                &json!([{ "taskId": "task-2", "description": "index docs", "status": "running", "startedAt": 1u64 }]),
            )
            .unwrap();
    let runner = TaskRunner::new(Some(store));

    assert!(
        runner
            .reconcile_previous_session(&HashSet::from([String::from("task-2")]))
            .is_none(),
        "a task the history already announced is not reported again"
    );
    let stored = stored_tasks(&runner);
    assert_eq!(
        stored[0].get("status").and_then(Value::as_str),
        Some("lost"),
        "the loss itself is still persisted: {stored}"
    );
    assert_eq!(
        stored[0].get("resumeReminded").and_then(Value::as_bool),
        Some(true),
        "the one-shot marker is persisted for the pre-seen task: {stored}"
    );
    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_none(),
        "the marker keeps later reconciles silent"
    );
}

/// The baseline scan reads task ids off previous-session reminders in the
/// history: only `<system-reminder>` messages carrying the reminder
/// header, only their `- <taskId> "` lines — a reminder-shaped message
/// without the header, or a dash line in an ordinary message,
/// contributes nothing.
#[test]
fn scan_previous_session_reminders_reads_ids_off_reminder_messages() {
    let reminder = crate::injection::wrap_system_reminder(&format!(
        "{PREVIOUS_SESSION_REMINDER_HEADER}\n- task-1 \"run tests\" (bash)\n- task-7 \"Resume researcher: audit\" (subagent) — resume it with Agent(resume=\"agent-42\", prompt=\"Pick up where you left off.\")\nDon't assume any of them completed; check current state (they may still be running), then re-run or resume only what you still need."
    ));
    let headerless = crate::injection::wrap_system_reminder("- task-x \"not a reminder\" (bash)");
    let plain = LLMMessage {
        role: "user".into(),
        content: "- task-y \"user pasted a dash line\" (bash)".into(),
        ..Default::default()
    };

    let seen = scan_previous_session_reminders(&[
        crate::injection::injection_message(reminder),
        crate::injection::injection_message(headerless),
        plain,
    ]);
    assert_eq!(
        seen,
        HashSet::from([String::from("task-1"), String::from("task-7")]),
        "only the reminder's own task lines contribute ids"
    );
}

/// Each newly reported loss queues a `Lost` notification for the owning
/// session (v2 `task.notificationDelivery`), and a re-report with the
/// first notification still queued must not queue the same
/// `(task_id, status)` twice.
#[test]
fn reconcile_queues_a_lost_notification_for_a_newly_reported_task() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    store
        .write_domain(
            "task",
            &json!([{
                "taskId": "task-3",
                "description": "nightly build",
                "status": "running",
                "startedAt": 1u64,
                "sessionId": "sess-old",
                "output": "built 42 crates",
            }]),
        )
        .unwrap();
    let runner = TaskRunner::new(Some(store));

    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_some(),
        "the loss is reported"
    );
    assert_eq!(
        runner.pending_notification_count(Some("sess-old")),
        1,
        "the reported loss is queued for its session"
    );

    // Bypass the marker write: the loss re-reports, but the already
    // queued (task_id, status) pair is never queued twice.
    runner
        .store
        .as_ref()
        .unwrap()
        .write_domain(
            "task",
            &json!([{
                "taskId": "task-3",
                "description": "nightly build",
                "status": "running",
                "startedAt": 1u64,
                "sessionId": "sess-old",
                "output": "built 42 crates",
            }]),
        )
        .unwrap();
    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_some(),
        "the bypassed marker re-reports the loss"
    );
    assert_eq!(
        runner.pending_notification_count(Some("sess-old")),
        1,
        "a re-report must not queue the same (task_id, status) twice"
    );

    let queued = runner.take_pending_notifications(Some("sess-old"));
    assert_eq!(queued.len(), 1);
    let notification = &queued[0];
    assert_eq!(notification.task_id, "task-3");
    assert_eq!(notification.status, TaskStatus::Lost);
    assert_eq!(notification.description, "nightly build");
    assert_eq!(notification.session_id.as_deref(), Some("sess-old"));
    assert_eq!(notification.started_at, 1, "startedAt off the entry");
    assert!(
        notification.ended_at >= 1,
        "endedAt was stamped by the reconcile: {}",
        notification.ended_at
    );
    assert_eq!(
        notification.output_preview.as_deref(),
        Some("built 42 crates"),
        "the output rides through the same preview truncation"
    );
}

/// A dead session gets no queued notification — the same liveness gate
/// the settle path uses (nobody would ever drain it) — while the
/// reminder itself still reports the loss to the model.
#[test]
fn reconcile_queues_the_notification_only_while_the_session_is_alive() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    store
        .write_domain(
            "task",
            &json!([{
                "taskId": "task-4",
                "description": "churn the cache",
                "status": "running",
                "startedAt": 1u64,
                "sessionId": "sess-gone",
            }]),
        )
        .unwrap();
    let runner = TaskRunner::new(Some(store));
    runner.set_liveness_check(Arc::new(|session: Option<&str>| {
        session != Some("sess-gone")
    }));

    assert!(
        runner.reconcile_previous_session(&HashSet::new()).is_some(),
        "the reminder still reports the loss"
    );
    assert_eq!(
        runner.pending_notification_count(Some("sess-gone")),
        0,
        "a dead session's queue must not grow"
    );
}

/// The live-task exclusion is per id: the orphan is still reported while
/// the in-process task keeps `running` and is never named.
#[tokio::test]
async fn reconcile_reports_the_orphan_but_never_the_live_task() {
    let (_tmp, runner) = runner();
    runner
            .store
            .as_ref()
            .unwrap()
            .write_domain(
                "task",
                &json!([{ "taskId": "task-orphan", "description": "run tests", "status": "running", "startedAt": 1u64 }]),
            )
            .unwrap();
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: None,
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-live".into(),
            "long build".into(),
            std::future::pending::<TaskOutcome>(),
        )
        .unwrap();

    let text = runner
        .reconcile_previous_session(&HashSet::new())
        .expect("the orphan is reported");
    assert!(
        text.contains("- task-orphan \"run tests\""),
        "the orphan is named: {text}"
    );
    assert!(
        !text.contains("task-live"),
        "the live task is never named: {text}"
    );

    let stored = stored_tasks(&runner);
    let stored_task = |id: &str| -> Value {
        stored
            .as_array()
            .expect("task domain is an array")
            .iter()
            .find(|entry| entry.get("taskId").and_then(Value::as_str) == Some(id))
            .expect("the task is persisted")
            .clone()
    };
    assert_eq!(
        stored_task("task-orphan")
            .get("status")
            .and_then(Value::as_str),
        Some("lost"),
        "the orphan is persisted as lost: {stored}"
    );
    assert_eq!(
        stored_task("task-live")
            .get("status")
            .and_then(Value::as_str),
        Some("running"),
        "the live task keeps its running status: {stored}"
    );
}

#[test]
fn background_limits_from_wire_drops_only_meaningless_zeroes() {
    // Unset wire values keep every built-in.
    assert_eq!(
        BackgroundLimits::from_wire(None, None, None, None, None),
        BackgroundLimits::default()
    );
    // A zero grace period / concurrency cap is not meaningful (JSON has no
    // way to say "unset"), so it falls back to the built-in...
    assert_eq!(
        BackgroundLimits::from_wire(Some(0), Some(0), None, None, None),
        BackgroundLimits::default()
    );
    // ...but `bash_task_timeout_s = 0` is a real value meaning "no
    // timeout", and a concurrency cap wider than `u32` is discarded.
    let limits = BackgroundLimits::from_wire(
        Some(250),
        Some(u64::from(u32::MAX) + 1),
        Some(false),
        Some(0),
        None,
    );
    assert_eq!(
        limits,
        BackgroundLimits {
            kill_grace_period_ms: Some(250),
            max_running_tasks: None,
            bash_auto_background_on_timeout: Some(false),
            bash_task_timeout_s: Some(0),
            keep_alive_on_exit: None,
        }
    );
    // A cap that fits is carried through.
    assert_eq!(
        BackgroundLimits::from_wire(None, Some(4), None, None, None).max_running_tasks,
        Some(4)
    );
}

#[tokio::test]
async fn lifecycle_events_carry_the_session_and_both_vocabularies() {
    let (_tmp, runner) = runner();
    let events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    runner.set_event_sink(Arc::new(move |session, event| {
        sink.lock()
            .unwrap()
            .push((session.map(str::to_string), event));
    }));

    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("sess-9"),
                kind: "subagent",
                subagent_type: Some("research"),
                agent_id: None,
                timeout_ms: None,
            },
            "task-ev".into(),
            "Subagent research: investigate".into(),
            async { TaskOutcome::Completed("done".to_string()) },
        )
        .unwrap();

    // Spawn announces created + started on the task's session lane.
    let (created_session, created) = events.lock().unwrap()[0].clone();
    assert_eq!(created_session.as_deref(), Some("sess-9"));
    assert_eq!(created["type"], "event.task.created");
    assert_eq!(created["task"]["id"], "task-ev");
    assert_eq!(created["task"]["session_id"], "sess-9");
    assert_eq!(created["task"]["kind"], "subagent");
    assert_eq!(created["task"]["status"], "running");
    assert_eq!(created["task"]["subagent_type"], "research");
    assert_eq!(created["task"]["run_in_background"], true);
    let (started_session, started) = events.lock().unwrap()[1].clone();
    assert_eq!(started_session.as_deref(), Some("sess-9"));
    assert_eq!(started["type"], "background.task.started");
    assert_eq!(started["task_id"], "task-ev");

    // Settling announces completed + terminated with the output facts.
    assert!(
        matches!(
            runner.wait("task-ev", 2000).await,
            TaskWaitResult::Completed(_)
        ),
        "the task must settle for the assertion below"
    );
    {
        let lock = events.lock().unwrap();
        let completed = &lock[2].1;
        assert_eq!(completed["type"], "event.task.completed");
        assert_eq!(completed["task_id"], "task-ev");
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["output_preview"], "done");
        assert_eq!(completed["output_bytes"], 4);
        let terminated = &lock[3].1;
        assert_eq!(terminated["type"], "background.task.terminated");
        assert_eq!(terminated["task_id"], "task-ev");
        assert_eq!(terminated["status"], "completed");
    }
}

#[tokio::test]
async fn emit_progress_reports_on_the_running_tasks_lane() {
    let (_tmp, runner) = runner();
    let events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    runner.set_event_sink(Arc::new(move |session, event| {
        sink.lock()
            .unwrap()
            .push((session.map(str::to_string), event));
    }));

    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("sess-prog"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-prog".into(),
            "background job".into(),
            async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                TaskOutcome::Completed("done".to_string())
            },
        )
        .unwrap();

    // An unknown task id is a silent no-op.
    runner.emit_progress("missing", "ignored", "stdout");
    runner.emit_progress("task-prog", "hello ", "stdout");

    assert!(matches!(
        runner.wait("task-prog", 2000).await,
        TaskWaitResult::Completed(_)
    ));

    let lock = events.lock().unwrap();
    let progress: Vec<_> = lock
        .iter()
        .filter(|(_, event)| event["type"] == "event.task.progress")
        .collect();
    assert_eq!(progress.len(), 1, "unknown task must not emit");
    assert_eq!(progress[0].0.as_deref(), Some("sess-prog"));
    assert_eq!(progress[0].1["task_id"], "task-prog");
    assert_eq!(progress[0].1["output_chunk"], "hello ");
    assert_eq!(progress[0].1["stream"], "stdout");
}

#[tokio::test]
async fn background_limits_cap_concurrent_tasks() {
    // `[background].max_running_tasks`: the cap refuses a spawn rather
    // than queueing it, and frees up once a task settles.
    let runner = Arc::new(TaskRunner::new(None).with_background_limits(None, Some(1), None));
    // A oneshot (not a Notify): the release must not race the task's
    // first poll, or the wait never ends.
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    runner
        .spawn_task("t1".into(), "first".into(), async move {
            let _ = held.await;
            "done".to_string()
        })
        .unwrap();

    let err = runner
        .spawn_task("t2".into(), "second".into(), async { "x".to_string() })
        .expect_err("the cap is one running task");
    assert!(err.contains("Too many background tasks"), "{err}");

    let _ = release.send(());
    assert!(matches!(
        runner.wait("t1", 2000).await,
        TaskWaitResult::Completed(_)
    ));
    runner
        .spawn_task("t3".into(), "third".into(), async { "x".to_string() })
        .expect("the slot frees up when the task settles");
}

#[tokio::test]
async fn metaless_spawns_route_to_the_global_lane() {
    let (_tmp, runner) = runner();
    let events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    runner.set_event_sink(Arc::new(move |session, event| {
        sink.lock()
            .unwrap()
            .push((session.map(str::to_string), event));
    }));

    runner
        .spawn_task("task-none".into(), "bash".into(), async { "x".to_string() })
        .unwrap();
    let (session, created) = events.lock().unwrap()[0].clone();
    assert_eq!(session, None, "no meta → no session → global lane");
    assert_eq!(created["task"]["session_id"], Value::Null);
    assert_eq!(created["task"]["kind"], "tool");
}

#[tokio::test]
async fn spawn_completes_and_snapshots_output() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-1".into(), "run tests".into(), async {
            "All tests passed.".to_string()
        })
        .unwrap();
    match runner.wait("task-1", 5000).await {
        TaskWaitResult::Completed(wire) => {
            assert_eq!(wire["taskId"], "task-1");
            assert_eq!(wire["description"], "run tests");
            assert_eq!(wire["status"], "completed");
            assert!(wire["startedAt"].as_u64().unwrap() > 0);
            assert!(wire["endedAt"].as_u64().unwrap() >= wire["startedAt"].as_u64().unwrap());
            assert_eq!(wire["output"], "All tests passed.");
        }
        other => panic!("expected completed, got {other:?}"),
    }
    assert_eq!(
        runner.get_output("task-1").as_deref(),
        Some("All tests passed.")
    );
    // The task domain state reflects the terminal status.
    let tasks = stored_tasks(&runner);
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["taskId"], "task-1");
    assert_eq!(tasks[0]["status"], "completed");
    assert!(tasks[0].get("endedAt").is_some());
}

#[tokio::test]
async fn output_is_none_while_running() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    assert_eq!(runner.get_output("task-1"), None);
    // The task domain state shows the running entry.
    let tasks = stored_tasks(&runner);
    assert_eq!(tasks[0]["taskId"], "task-1");
    assert_eq!(tasks[0]["status"], "running");
    // Clean up the pending task so the test runtime can shut down.
    let wire = runner.stop("task-1", None).await.unwrap().wire;
    assert_eq!(wire["status"], "killed");
}

#[tokio::test]
async fn stop_kills_running_task() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    let wire = runner.stop("task-1", None).await.unwrap().wire;
    assert_eq!(wire["status"], "killed");
    assert_eq!(wire["stopReason"], "Stopped by TaskStop");
    assert!(wire["endedAt"].as_u64().unwrap() >= wire["startedAt"].as_u64().unwrap());
    // A stopped task has no output snapshot.
    assert_eq!(runner.get_output("task-1"), None);
    // Waiting on the killed task reports the terminal state.
    match runner.wait("task-1", 5000).await {
        TaskWaitResult::Completed(wire) => assert_eq!(wire["status"], "killed"),
        other => panic!("expected completed, got {other:?}"),
    }
    // The task domain state reflects the kill.
    let tasks = stored_tasks(&runner);
    assert_eq!(tasks[0]["status"], "killed");
    assert_eq!(tasks[0]["stopReason"], "Stopped by TaskStop");
}

#[tokio::test]
async fn wait_times_out_then_completes() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-1".into(), "slow".into(), async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            "done".to_string()
        })
        .unwrap();
    match runner.wait("task-1", 50).await {
        TaskWaitResult::TimedOut(wire) => assert_eq!(wire["status"], "running"),
        other => panic!("expected timed out, got {other:?}"),
    }
    match runner.wait("task-1", 5000).await {
        TaskWaitResult::Completed(wire) => {
            assert_eq!(wire["status"], "completed");
            assert_eq!(wire["output"], "done");
        }
        other => panic!("expected completed, got {other:?}"),
    }
}

#[tokio::test]
async fn wait_zero_timeout_returns_immediately() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    match runner.wait("task-1", 0).await {
        TaskWaitResult::TimedOut(wire) => assert_eq!(wire["status"], "running"),
        other => panic!("expected timed out, got {other:?}"),
    }
    runner.stop("task-1", None).await.unwrap();
}

/// The primitive behind v2's `stopAllOnExit('Session closed')`: it stops
/// the named session's tasks and nobody else's (ROADMAP §15 D2).
#[tokio::test]
async fn stop_session_flags_only_its_own_tasks() {
    let (_tmp, runner) = runner();
    for (id, session) in [("a-1", "sess-a"), ("a-2", "sess-a"), ("b-1", "sess-b")] {
        runner
            .spawn_task_with_meta(
                TaskSpawnMeta {
                    session_id: Some(session),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                    timeout_ms: None,
                },
                id.into(),
                format!("task {id}"),
                std::future::pending::<TaskOutcome>(),
            )
            .unwrap();
    }

    let mut stopped = runner.stop_session("sess-a", None).await.flagged;
    stopped.sort();
    assert_eq!(stopped, vec!["a-1".to_string(), "a-2".to_string()]);

    // Its own tasks carry the close reason; the other session's task was
    // never touched — an unscoped stop would be a cross-session kill.
    assert_eq!(runner.entry("a-1").unwrap()["stopReason"], "Session closed");
    assert_eq!(runner.entry("a-2").unwrap()["stopReason"], "Session closed");
    assert!(runner.entry("b-1").unwrap()["stopReason"].is_null());
    assert_eq!(runner.entry("b-1").unwrap()["status"], "running");
}

#[tokio::test]
async fn stop_session_is_empty_for_a_session_that_owns_nothing() {
    let (_tmp, runner) = runner();
    assert!(runner.stop_session("nobody", None).await.flagged.is_empty());

    // A task with no session is nobody's to stop by naming a session.
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: None,
                kind: "tool",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "unattributed".into(),
            "no session".into(),
            std::future::pending::<TaskOutcome>(),
        )
        .unwrap();
    assert!(runner.stop_session("nobody", None).await.flagged.is_empty());
    assert!(runner.entry("unattributed").unwrap()["stopReason"].is_null());
    runner.stop("unattributed", None).await.unwrap();
}

#[tokio::test]
async fn stop_session_does_not_overwrite_a_reason_already_set() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("sess-r"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-r".into(),
            "reasoned".into(),
            std::future::pending::<TaskOutcome>(),
        )
        .unwrap();
    runner.stop("task-r", Some("user cancelled")).await.unwrap();
    runner.stop_session("sess-r", None).await;
    assert_eq!(
        runner.entry("task-r").unwrap()["stopReason"],
        "user cancelled"
    );
}

#[tokio::test]
async fn unknown_task_is_not_found() {
    let (_tmp, runner) = runner();
    assert!(matches!(
        runner.wait("nope", 100).await,
        TaskWaitResult::NotFound
    ));
    let err = runner.stop("nope", None).await.unwrap_err();
    assert!(err.contains("Task not found: nope"));
    assert_eq!(runner.get_output("nope"), None);
    assert_eq!(runner.entry("nope"), None);
}

#[tokio::test]
async fn wait_interruptible_ends_early_and_leaves_the_task_running() {
    let (_tmp, runner) = runner();
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    runner
        .spawn_task("task-1".into(), "long".into(), async move {
            let _ = held.await;
            "done".to_string()
        })
        .unwrap();

    let signal = crate::subagent::types::ParentCancel::new();
    let trigger = signal.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.trigger();
    });

    let started = std::time::Instant::now();
    assert!(matches!(
        runner
            .wait_interruptible("task-1", 30_000, Some(&signal))
            .await,
        TaskWaitResult::Interrupted
    ));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the signal must end the wait, not the timeout"
    );
    assert_eq!(
        runner.entry("task-1").unwrap()["status"],
        "running",
        "an interrupted wait must not settle the task"
    );
    let _ = release.send(());
}

#[tokio::test]
async fn wait_interruptible_returns_at_once_for_an_already_fired_signal() {
    // The trigger can land before the wait parks; the pre-check is what
    // keeps that from turning into a full-timeout wait.
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    let signal = crate::subagent::types::ParentCancel::new();
    signal.trigger();
    let started = std::time::Instant::now();
    assert!(matches!(
        runner
            .wait_interruptible("task-1", 30_000, Some(&signal))
            .await,
        TaskWaitResult::Interrupted
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    runner.stop("task-1", None).await.unwrap();
}

#[tokio::test]
async fn wait_interruptible_takes_the_terminal_state_over_a_racing_signal() {
    // A task that settled and a steer that fired are not distinguishable by
    // the caller's intent, so the settled task's result must win: the
    // completed report carries output a bare "interrupted" would drop.
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-1".into(), "quick".into(), async {
            "done".to_string()
        })
        .unwrap();
    // Let it settle first, so the status check — not the signal — decides.
    match runner.wait("task-1", 5000).await {
        TaskWaitResult::Completed(wire) => assert_eq!(wire["status"], "completed"),
        other => panic!("expected completed, got {other:?}"),
    }
    let signal = crate::subagent::types::ParentCancel::new();
    signal.trigger();
    match runner
        .wait_interruptible("task-1", 30_000, Some(&signal))
        .await
    {
        TaskWaitResult::Completed(wire) => assert_eq!(wire["status"], "completed"),
        other => panic!("expected completed, got {other:?}"),
    }
}

#[tokio::test]
async fn wait_any_returns_the_first_task_to_settle() {
    let (_tmp, runner) = runner();
    let (release_slow, held_slow) = tokio::sync::oneshot::channel::<()>();
    runner
        .spawn_task("slow".into(), "slow".into(), async move {
            let _ = held_slow.await;
            "slow output".to_string()
        })
        .unwrap();
    let (release_fast, held_fast) = tokio::sync::oneshot::channel::<()>();
    runner
        .spawn_task("fast".into(), "fast".into(), async move {
            let _ = held_fast.await;
            "fast output".to_string()
        })
        .unwrap();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = release_fast.send(());
    });
    match runner
        .wait_any(&["slow".to_string(), "fast".to_string()], 30_000, None)
        .await
    {
        TaskWaitResult::Completed(wire) => {
            assert_eq!(wire["taskId"], "fast");
            assert_eq!(wire["output"], "fast output");
        }
        other => panic!("expected completed, got {other:?}"),
    }
    assert_eq!(runner.entry("slow").unwrap()["status"], "running");
    let _ = release_slow.send(());
}

#[tokio::test]
async fn wait_any_times_out_and_reports_not_found_for_no_ids() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    match runner.wait_any(&["task-1".to_string()], 50, None).await {
        TaskWaitResult::TimedOut(wire) => assert_eq!(wire["status"], "running"),
        other => panic!("expected timed out, got {other:?}"),
    }
    assert!(matches!(
        runner.wait_any(&[], 50, None).await,
        TaskWaitResult::NotFound
    ));
    runner.stop("task-1", None).await.unwrap();
}

#[tokio::test]
async fn wait_any_honors_the_interrupt_signal() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    let signal = crate::subagent::types::ParentCancel::new();
    signal.trigger();
    assert!(matches!(
        runner
            .wait_any(&["task-1".to_string()], 30_000, Some(&signal))
            .await,
        TaskWaitResult::Interrupted
    ));
    assert_eq!(runner.entry("task-1").unwrap()["status"], "running");
    runner.stop("task-1", None).await.unwrap();
}

#[tokio::test]
async fn duplicate_spawn_is_rejected() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-1".into(), "a".into(), async { "x".to_string() })
        .unwrap();
    let err = runner
        .spawn_task("task-1".into(), "b".into(), async { "y".to_string() })
        .unwrap_err();
    assert!(err.contains("task-1"));
}

#[tokio::test]
async fn list_returns_entries_without_output() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-1".into(), "a".into(), async { "x".to_string() })
        .unwrap();
    runner
        .spawn_task(
            "task-2".into(),
            "b".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    runner.wait("task-1", 5000).await;
    let entries = runner.list();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["taskId"], "task-1");
    assert_eq!(entries[0]["status"], "completed");
    assert!(entries[0].get("output").is_none());
    assert_eq!(entries[1]["taskId"], "task-2");
    assert_eq!(entries[1]["status"], "running");
    runner.stop("task-2", None).await.unwrap();
}

#[tokio::test]
async fn stop_records_custom_reason() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task(
            "task-1".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    let wire = runner
        .stop("task-1", Some("User initiated stop"))
        .await
        .unwrap()
        .wire;
    assert_eq!(wire["status"], "killed");
    assert_eq!(wire["stopReason"], "User initiated stop");
    // A blank reason falls back to the default.
    runner
        .spawn_task(
            "task-2".into(),
            "long".into(),
            std::future::pending::<String>(),
        )
        .unwrap();
    let wire = runner.stop("task-2", Some("   ")).await.unwrap().wire;
    assert_eq!(wire["stopReason"], "Stopped by TaskStop");
}

#[tokio::test]
async fn stop_on_terminal_task_returns_current_status() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-1".into(), "a".into(), async { "x".to_string() })
        .unwrap();
    runner.wait("task-1", 5000).await;
    let wire = runner
        .stop("task-1", Some("late reason"))
        .await
        .unwrap()
        .wire;
    assert_eq!(wire["status"], "completed");
    // Terminal entries keep their state: no reason is attached.
    assert!(wire.get("stopReason").is_none());
}

#[tokio::test]
async fn runner_works_without_state_store() {
    let runner = Arc::new(TaskRunner::new(None));
    runner
        .spawn_task("task-1".into(), "a".into(), async { "x".to_string() })
        .unwrap();
    match runner.wait("task-1", 5000).await {
        TaskWaitResult::Completed(wire) => assert_eq!(wire["output"], "x"),
        other => panic!("expected completed, got {other:?}"),
    }
}

/// The settle path records the task's session on the notification, and the
/// drain hands a session only its own completions: the runner is shared
/// across sessions, so an unscoped drain would let one session's print turn
/// consume another session's task.
#[tokio::test]
async fn notifications_are_scoped_to_the_settling_tasks_session() {
    let (_tmp, runner) = runner();
    for (id, session) in [("task-a", "sess-a"), ("task-b", "sess-b")] {
        runner
            .spawn_task_with_meta(
                TaskSpawnMeta {
                    session_id: Some(session),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                    timeout_ms: None,
                },
                id.into(),
                format!("job {id}"),
                async { TaskOutcome::Completed("done".to_string()) },
            )
            .unwrap();
    }
    assert!(matches!(
        runner.wait("task-a", 2000).await,
        TaskWaitResult::Completed(_)
    ));
    assert!(matches!(
        runner.wait("task-b", 2000).await,
        TaskWaitResult::Completed(_)
    ));

    assert_eq!(runner.pending_notification_count(Some("sess-a")), 1);
    assert_eq!(runner.pending_notification_count(Some("sess-b")), 1);

    let drained = runner.take_pending_notifications(Some("sess-a"));
    assert_eq!(drained.len(), 1, "only sess-a's completion may be drained");
    assert_eq!(drained[0].task_id, "task-a");
    assert_eq!(drained[0].session_id.as_deref(), Some("sess-a"));
    assert_eq!(runner.pending_notification_count(Some("sess-a")), 0);
    assert_eq!(
        runner.pending_notification_count(Some("sess-b")),
        1,
        "sess-b's completion must survive sess-a's drain"
    );

    let drained = runner.take_pending_notifications(Some("sess-b"));
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].task_id, "task-b");
    assert_eq!(drained[0].session_id.as_deref(), Some("sess-b"));
}

/// A task spawned without a session id is server-level: no session-scoped
/// drain may take it, and an unattributed caller takes nothing either.
#[tokio::test]
async fn unattributed_notifications_are_never_drained_by_a_session() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task("task-none".into(), "bash".into(), async { "x".to_string() })
        .unwrap();
    assert!(matches!(
        runner.wait("task-none", 2000).await,
        TaskWaitResult::Completed(_)
    ));

    assert_eq!(runner.pending_notification_count(Some("sess-a")), 0);
    assert!(runner.take_pending_notifications(Some("sess-a")).is_empty());
    assert!(runner.take_pending_notifications(None).is_empty());
    assert_eq!(
        runner.pending_notification_count(None),
        0,
        "a caller with no session id counts nothing"
    );
    // The notification is still queued, not dropped by a foreign drain.
    assert_eq!(runner.pending_notifications.lock().unwrap().len(), 1);
}

/// Late-settle silence (#3717 / v2 `taskService.lifecycleActive`): once
/// the host reports the session gone, the settle path must announce
/// nothing and queue nothing — the pump that would drain the notification
/// is gone, so a queued completion just leaks. The entry itself still
/// settles and stays queryable: v2's shape where the task terminates but
/// its notification is dropped.
#[tokio::test]
async fn settle_stays_silent_when_the_host_reports_the_session_dead() {
    let (_tmp, runner) = runner();
    let events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    runner.set_event_sink(Arc::new(move |session, event| {
        sink.lock()
            .unwrap()
            .push((session.map(str::to_string), event));
    }));
    runner.set_liveness_check(Arc::new(|session: Option<&str>| match session {
        // Server-level tasks have no session lifecycle to outlive.
        None => true,
        Some(s) => s != "sess-gone",
    }));

    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("sess-gone"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-gone".into(),
            "job".into(),
            async { TaskOutcome::Completed("done".to_string()) },
        )
        .unwrap();
    assert!(matches!(
        runner.wait("task-gone", 2000).await,
        TaskWaitResult::Completed(_)
    ));

    // The entry still settled (the state bridge mirror is unaffected).
    assert_eq!(runner.entry("task-gone").unwrap()["status"], "completed");
    // ...but neither terminal vocabulary fired, and no completion was
    // queued for a drain that will never come.
    let settled = |ty: &str| events.lock().unwrap().iter().any(|(_, e)| e["type"] == ty);
    assert!(!settled("event.task.completed"));
    assert!(!settled("background.task.terminated"));
    assert!(
        runner
            .take_pending_notifications(Some("sess-gone"))
            .is_empty()
    );
    assert!(runner.take_pending_notifications(None).is_empty());
}

/// The gate is a per-session rule, not a global kill switch: a live
/// session's settle still fires both vocabularies and queues its
/// notification, while a dead session's settle goes silent. Spawn-time
/// facts are not gated — the task did start; only its terminal effects
/// depend on whether anyone is left to receive them.
#[tokio::test]
async fn liveness_gate_is_per_session() {
    let (_tmp, runner) = runner();
    let events: CapturedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    runner.set_event_sink(Arc::new(move |session, event| {
        sink.lock()
            .unwrap()
            .push((session.map(str::to_string), event));
    }));
    runner.set_liveness_check(Arc::new(|session: Option<&str>| match session {
        None => true,
        Some(s) => s != "sess-dead",
    }));

    for (id, session) in [("task-alive", "sess-alive"), ("task-dead", "sess-dead")] {
        runner
            .spawn_task_with_meta(
                TaskSpawnMeta {
                    session_id: Some(session),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                    timeout_ms: None,
                },
                id.into(),
                format!("job {id}"),
                async { TaskOutcome::Completed("done".to_string()) },
            )
            .unwrap();
    }
    assert!(matches!(
        runner.wait("task-alive", 2000).await,
        TaskWaitResult::Completed(_)
    ));
    assert!(matches!(
        runner.wait("task-dead", 2000).await,
        TaskWaitResult::Completed(_)
    ));

    let settled = |lane: &str, ty: &str| {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|(s, e)| s.as_deref() == Some(lane) && e["type"] == ty)
    };
    assert!(settled("sess-alive", "event.task.completed"));
    assert!(settled("sess-alive", "background.task.terminated"));
    assert!(!settled("sess-dead", "event.task.completed"));
    assert!(!settled("sess-dead", "background.task.terminated"));
    assert!(
        settled("sess-dead", "event.task.created"),
        "spawn facts are not gated — only the settle path is"
    );
    assert_eq!(runner.pending_notification_count(Some("sess-alive")), 1);
    assert_eq!(runner.pending_notification_count(Some("sess-dead")), 0);
}

/// No predicate installed = the pre-gate behavior: hosts that never opted
/// in keep receiving terminal events and notifications, so the existing
/// wiring (repl, tests, in-process runners) is unaffected.
#[tokio::test]
async fn without_a_liveness_predicate_the_settle_path_keeps_firing() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("sess-legacy"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-legacy".into(),
            "job".into(),
            async { TaskOutcome::Completed("done".to_string()) },
        )
        .unwrap();
    assert!(matches!(
        runner.wait("task-legacy", 2000).await,
        TaskWaitResult::Completed(_)
    ));
    assert_eq!(runner.pending_notification_count(Some("sess-legacy")), 1);
}

#[tokio::test]
async fn tower_wake_coalesces_into_one_notification_per_batch() {
    let (_tmp, runner) = runner();
    runner.enqueue_wake(
        "sess-main",
        "tower-inbox-wake",
        "Workers sent new message(s) to the tower.".into(),
    );
    runner.enqueue_wake(
        "sess-main",
        "tower-inbox-wake",
        "Workers sent 2 new message(s) to the tower.".into(),
    );
    // The second enqueue replaced the first: one pending wake, refreshed.
    assert_eq!(runner.pending_notification_count(Some("sess-main")), 1);
    let drained = runner.take_pending_notifications(Some("sess-main"));
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].task_id, "tower-inbox-wake");
    assert_eq!(
        drained[0].description,
        "Workers sent 2 new message(s) to the tower."
    );
}

#[tokio::test]
async fn tower_wake_is_scoped_and_cancelable() {
    let (_tmp, runner) = runner();
    runner.enqueue_wake(
        "sess-main",
        "tower-inbox-wake",
        "Workers sent new message(s) to the tower.".into(),
    );
    // Another session's drain takes nothing (task notifications are
    // per-session and the wake is no exception).
    assert_eq!(runner.pending_notification_count(Some("sess-other")), 0);
    // Exiting tower mode drops the queued wake.
    runner.cancel_wake("sess-main", "tower-inbox-wake");
    assert_eq!(runner.pending_notification_count(Some("sess-main")), 0);
}

// ── task settlement verdicts (v2 agent/task/types.ts) ─────────────────

/// The wire vocabulary, and the one place a status can still be spelled
/// wrong: the TypeScript counterpart is `TaskLifecycleStatus`
/// (`packages/node-sdk/src/types.ts:69-78`), so these strings are a
/// contract with the host, not a display detail.
#[test]
fn task_status_as_str_matches_the_host_contract() {
    assert_eq!(TaskStatus::Running.as_str(), "running");
    assert_eq!(TaskStatus::Completed.as_str(), "completed");
    assert_eq!(TaskStatus::Failed.as_str(), "failed");
    assert_eq!(TaskStatus::Killed.as_str(), "killed");
    assert_eq!(TaskStatus::Lost.as_str(), "lost");
}

/// `From<String>` is what keeps `spawn_task`'s infallible callers working;
/// it must land on `Completed`, never on a guess.
#[test]
fn a_bare_string_outcome_settles_as_completed() {
    let (status, output) = TaskOutcome::from("output".to_string()).settle();
    assert_eq!(status, TaskStatus::Completed);
    assert_eq!(output.as_deref(), Some("output"));
}

/// The bug this whole change exists for: a task whose work failed used to
/// settle `completed`, so the parent session and anyone listing the tasks
/// were told a failed subagent had succeeded.
#[tokio::test]
async fn a_failed_outcome_settles_as_failed_and_keeps_its_reason() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("sess-x"),
                kind: "subagent",
                subagent_type: Some("research"),
                agent_id: None,
                timeout_ms: None,
            },
            "t-failed".into(),
            "Subagent research: investigate".into(),
            async { TaskOutcome::Failed("Error: provider refused the request".into()) },
        )
        .unwrap();

    assert!(
        matches!(
            runner.wait("t-failed", 2000).await,
            TaskWaitResult::Completed(_)
        ),
        "a failed task still settles, so wait() returns rather than hanging"
    );

    let entry = runner.entry("t-failed").expect("the entry survives");
    assert_eq!(
        entry["status"], "failed",
        "a failed task must not read as completed: {entry}"
    );
    // The reason is the task's output, so a reader sees *why* it failed.
    assert_eq!(entry["output"], "Error: provider refused the request");
}

/// The three settlement arms stay distinguishable end to end: a success, a
/// failure, and a `stop()` are three different stories, and collapsing any
/// two of them is the defect.
#[tokio::test]
async fn success_failure_and_kill_settle_three_different_statuses() {
    let (_tmp, runner) = runner();
    // The infallible entry point still means success — that is its whole
    // contract, and it is why its callers were left untouched.
    runner
        .spawn_task("t-ok".into(), "fine".into(), async {
            "all good".to_string()
        })
        .unwrap();
    // The fallible one carries its verdict.
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: None,
                kind: "subagent",
                subagent_type: Some("research"),
                agent_id: None,
                timeout_ms: None,
            },
            "t-bad".into(),
            "broken".into(),
            async { TaskOutcome::Failed("Error: boom".to_string()) },
        )
        .unwrap();
    runner
        .spawn_task("t-held".into(), "long".into(), async {
            let _ = std::future::pending::<()>().await;
            unreachable!("the cancel arm wins before this resolves")
        })
        .unwrap();

    // Let the two immediate tasks settle, then cut the third short the way
    // `stop()` does — the only producer of a kill.
    let _ = runner.wait("t-ok", 2000).await;
    let _ = runner.wait("t-bad", 2000).await;
    runner
        .stop("t-held", Some("the test cut it short"))
        .await
        .expect("the held task is running");

    assert_eq!(runner.entry("t-ok").unwrap()["status"], "completed");
    assert_eq!(runner.entry("t-bad").unwrap()["status"], "failed");
    assert_eq!(runner.entry("t-held").unwrap()["status"], "killed");
}

/// v2 `sessionActivityService`: a running background task contributes to its
/// session's `busy` fold, and the query flips as soon as the task settles.
#[tokio::test]
async fn running_task_keeps_its_session_busy() {
    let (_tmp, runner) = runner();
    runner
        .spawn_task_with_meta(
            TaskSpawnMeta {
                session_id: Some("session-a"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
                timeout_ms: None,
            },
            "task-busy".into(),
            "sleep".into(),
            async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                TaskOutcome::Completed("done".to_string())
            },
        )
        .unwrap();

    assert!(runner.has_running_task_for_session("session-a"));
    assert!(
        !runner.has_running_task_for_session("session-b"),
        "another session's task must not count"
    );

    // Wait for the task to settle, then the same session reports idle.
    let _ = runner.wait("task-busy", 2_000).await;
    assert!(!runner.has_running_task_for_session("session-a"));
}

/// A panic while a critical section holds the registry must not turn every
/// later call into a panic: the locks recover the poisoned guard instead of
/// unwrapping it, which is what `StateStore` already does.
#[test]
fn a_poisoned_registry_lock_is_recovered_not_propagated() {
    let (_tmp, runner) = runner();
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = runner.tasks.lock().unwrap_or_else(|e| e.into_inner());
        panic!("poison the task registry");
    }));
    assert!(poisoned.is_err(), "the guard is poisoned by the panic");

    // Every read path takes the same lock; none of them may panic now.
    assert!(runner.list().is_empty());
    assert!(runner.entry("task-1").is_none());
    assert!(runner.reconcile_previous_session(&HashSet::new()).is_none());
}

/// A `task.json` that parses but is not a list must not be replaced by a
/// settle's mirror: the file may hold entries this process does not know
/// about, so the mirror is skipped instead of rewritten from an empty list.
#[tokio::test]
async fn a_malformed_task_domain_is_not_overwritten_by_a_settle() {
    let tmp = TempDir::new().unwrap();
    let store = StateStore::for_dir(tmp.path().join("state")).unwrap();
    let path = tmp.path().join("state").join("task.json");
    let original = "{ \"entries\": [] }";
    std::fs::write(&path, original).unwrap();

    let runner = Arc::new(TaskRunner::new(Some(store)));
    runner
        .spawn_task("task-1".into(), "work".into(), async { "ok".to_string() })
        .unwrap();
    let _ = runner.wait("task-1", 2_000).await;

    assert_eq!(
        runner.entry("task-1").expect("the entry is in memory")["status"],
        "completed",
        "the in-memory entry still settles"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "the mirror must not replace a malformed task domain"
    );
}
