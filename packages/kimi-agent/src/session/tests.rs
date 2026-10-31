use std::sync::Arc;

use super::*;
use crate::callbacks::RpcHostCallbacks;
use crate::rpc::server::RpcServer;
use crate::rpc::types::TokenUsage;
use crate::turn_loop::types::{LLMChatParams, LLMChatResponse};

struct ScriptedLlm {
    requests: Arc<std::sync::Mutex<Vec<Vec<LLMMessage>>>>,
    /// One entry per call: `None` = no gate, `Some(rx)` = await before
    /// returning. `simple(...)` leaves the vec empty; `with_gate(...)`
    /// fills it with one Some per response.
    gates: Arc<std::sync::Mutex<Vec<Option<tokio::sync::oneshot::Receiver<()>>>>>,
    responses: Arc<std::sync::Mutex<Vec<LLMChatResponse>>>,
}

impl ScriptedLlm {
    fn simple(responses: Vec<LLMChatResponse>) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            gates: Arc::new(std::sync::Mutex::new(Vec::new())),
            responses: Arc::new(std::sync::Mutex::new(responses)),
        }
    }

    fn with_gate(responses: Vec<LLMChatResponse>) -> (Self, Vec<tokio::sync::oneshot::Sender<()>>) {
        let mut senders = Vec::new();
        let gates: Vec<Option<tokio::sync::oneshot::Receiver<()>>> = (0..responses.len())
            .map(|_| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                senders.push(tx);
                Some(rx)
            })
            .collect();
        (
            Self {
                requests: Arc::new(std::sync::Mutex::new(Vec::new())),
                gates: Arc::new(std::sync::Mutex::new(gates)),
                responses: Arc::new(std::sync::Mutex::new(responses)),
            },
            senders,
        )
    }
}

impl LLM for ScriptedLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "scripted-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn transport(&self) -> &'static str {
        "native-http"
    }
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let resp = self
            .responses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(0);
        let gate = self.gates.lock().unwrap_or_else(|e| e.into_inner()).pop();
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(params.messages.to_vec());
        let cancel = params.cancel.clone();
        Box::pin(async move {
            if let Some(rx) = gate.and_then(|g| g) {
                // Real transports select on `params.cancel` while the
                // request is in flight (types.rs `LLMChatParams::cancel`);
                // mirror that so a turn cancel cuts a gated response short
                // instead of racing it, and the call fails with an error
                // the retry layer never retries.
                let cancelled = async {
                    match cancel.as_ref() {
                        Some(token) => token.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = rx => {}
                    _ = cancelled => {
                        return Err("llm cancelled by script".into());
                    }
                }
            }
            Ok(resp)
        })
    }
}

fn text_response(text: &str) -> LLMChatResponse {
    LLMChatResponse {
        content: text.into(),
        thinking: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: Some("stop".into()),
        usage: TokenUsage::default(),
        timing: None,
    }
}

fn rpc_callbacks(server: Arc<RpcServer>) -> Arc<dyn HostCallbacks> {
    // The engine pulls the tool table before every LLM call (M1d) and the
    // goal/plan injection snapshot reads the state bridge at each step
    // head (M4). With no local handler the server falls back to a stdio
    // round-trip that stalls for the full timeout, so the session tests
    // answer both here.
    RpcServer::register_arc(
        &server,
        crate::rpc::types::methods::HOST_LIST_TOOLS,
        |_params| {
            Box::pin(async move {
                let resp = crate::rpc::types::ListToolsResponse { tools: vec![] };
                serde_json::to_value(&resp)
                    .map_err(|e| crate::rpc::types::JsonRpcError::internal_error(e.to_string()))
            })
        },
    );
    RpcServer::register_arc(
        &server,
        crate::rpc::types::methods::HOST_STATE_READ,
        |_params| {
            Box::pin(async move {
                let resp = crate::rpc::types::StateReadResponse {
                    value: serde_json::Value::Null,
                };
                serde_json::to_value(&resp)
                    .map_err(|e| crate::rpc::types::JsonRpcError::internal_error(e.to_string()))
            })
        },
    );
    Arc::new(RpcHostCallbacks { server })
}

fn msg(role: &str, content: &str) -> LLMMessage {
    LLMMessage {
        role: role.into(),
        content: content.into(),
        ..Default::default()
    }
}

async fn make_session(llm: Arc<dyn LLM>, callbacks: Arc<dyn HostCallbacks>) -> EngineSession {
    make_session_with_steer(llm, callbacks, None).await
}

/// [`make_session`] with a steer slot (#3697), so a test can observe the
/// per-turn signal the pump publishes and steer firings refresh.
async fn make_session_with_steer(
    llm: Arc<dyn LLM>,
    callbacks: Arc<dyn HostCallbacks>,
    steer_slot: Option<Arc<Mutex<Option<crate::subagent::types::ParentCancel>>>>,
) -> EngineSession {
    let config = SessionConfig {
        llm,
        callbacks,
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot,
        hook_guard: None,
        print_background: None,
        session_id: None,
        task_runner: None,
        toolset: None,
        telemetry: None,
    };
    EngineSession::new(config).await
}

async fn wait_until<F: FnMut() -> bool>(mut pred: F) {
    for _ in 0..1000 {
        if pred() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("condition not met after yield loop");
}

/// A disposed session must not keep its pump — and through it the whole
/// conversation — alive for the life of the process.
#[tokio::test]
async fn shutdown_releases_the_pump_and_the_conversation() {
    let server = Arc::new(RpcServer::new());
    let session = make_session(
        Arc::new(ScriptedLlm::simple(Vec::new())),
        rpc_callbacks(server),
    )
    .await;
    session.set_history(vec![msg("user", "hello")]);
    assert!(
        Arc::strong_count(&session.core) >= 2,
        "the pump holds the core while the session is live"
    );

    session.shutdown();
    wait_until(|| Arc::strong_count(&session.core) == 1).await;
    assert_eq!(
        session.history_len(),
        1,
        "history stays readable until drop"
    );
}

/// `released()` — the fact `dispose()` promises: it must not resolve
/// while the pump still holds the conversation, and must resolve once the
/// pump has exited (ROADMAP §15 D1 / §15.8 S1).
#[tokio::test]
async fn released_resolves_only_after_the_pump_exits() {
    let server = Arc::new(RpcServer::new());
    let session = make_session(
        Arc::new(ScriptedLlm::simple(Vec::new())),
        rpc_callbacks(server),
    )
    .await;
    assert!(
        !session.is_released(),
        "a live session is not released before anything closed it"
    );

    let waiter = session.clone();
    let waiting = tokio::spawn(async move { waiter.released().await });
    // Park the waiter on the oneshot: it is registered but must still be
    // pending — resolving here would mean the future short-circuited.
    tokio::task::yield_now().await;
    assert!(
        !waiting.is_finished(),
        "released() resolved before the pump exited"
    );

    session.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .expect("released() never resolved after shutdown")
        .expect("the waiting task panicked");
    assert!(session.is_released());
    assert_eq!(
        Arc::strong_count(&session.core),
        1,
        "the pump's reference is gone once released"
    );
}

/// `released()` is not a one-shot notification: a late waiter (a second
/// `dispose()`) must still resolve, and must resolve promptly.
#[tokio::test]
async fn released_resolves_immediately_once_already_released() {
    let server = Arc::new(RpcServer::new());
    let session = make_session(
        Arc::new(ScriptedLlm::simple(Vec::new())),
        rpc_callbacks(server),
    )
    .await;

    session.shutdown();
    tokio::time::timeout(std::time::Duration::from_secs(5), session.released())
        .await
        .expect("the first released() never resolved");
    tokio::time::timeout(std::time::Duration::from_secs(5), session.released())
        .await
        .expect("a later released() never resolved");
}

/// Every waiter registered before the pump exits is woken by the guard —
/// two concurrent `dispose()` calls must not strand one of them.
#[tokio::test]
async fn released_wakes_every_registered_waiter() {
    let server = Arc::new(RpcServer::new());
    let session = make_session(
        Arc::new(ScriptedLlm::simple(Vec::new())),
        rpc_callbacks(server),
    )
    .await;

    let first = tokio::spawn({
        let s = session.clone();
        async move { s.released().await }
    });
    let second = tokio::spawn({
        let s = session.clone();
        async move { s.released().await }
    });
    tokio::task::yield_now().await;

    session.shutdown();
    for waiter in [first, second] {
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("a waiter was left behind after the pump exited")
            .expect("a waiting task panicked");
    }
}

/// The pump's teardown also stops the session's background work: v2 does
/// it as an explicit close step (`stopAllOnExit('Session closed')`), and
/// the runner is process-wide, so nothing else can reach those tasks
/// (ROADMAP §15 D2). Removing the `stop_session` call in `pump` makes this
/// red, which is the point: the primitive has its own tests, but a tested
/// primitive with an unwired caller is exactly the "logic tested, wiring
/// not" gap §10.44 records.
#[tokio::test]
async fn shutdown_stops_the_sessions_background_tasks() {
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    runner
        .spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: Some("sess-pump"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
            },
            "task-pump".into(),
            "background".into(),
            std::future::pending::<crate::storage::TaskOutcome>(),
        )
        .unwrap();
    assert!(runner.entry("task-pump").unwrap()["stopReason"].is_null());

    let server = Arc::new(RpcServer::new());
    let config = SessionConfig {
        llm: Arc::new(ScriptedLlm::simple(Vec::new())),
        callbacks: rpc_callbacks(server),
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: None,
        hook_guard: None,
        print_background: None,
        session_id: Some("sess-pump".into()),
        task_runner: Some(runner.clone()),
        toolset: None,
        telemetry: None,
    };
    let session = EngineSession::new(config).await;

    session.shutdown();
    wait_until(|| !runner.entry("task-pump").unwrap()["stopReason"].is_null()).await;
    assert_eq!(
        runner.entry("task-pump").unwrap()["stopReason"],
        "Session closed"
    );
}

/// `[background].print_background_mode` wire mapping: only `drain` /
/// `steer` hold a turn receipt, and an unrecognised mode must resolve to
/// `exit` so a typo cannot stall a run behind the ceiling.
#[test]
fn test_print_background_mode_from_wire() {
    assert_eq!(
        PrintBackgroundMode::from_wire("drain"),
        PrintBackgroundMode::Drain
    );
    assert_eq!(
        PrintBackgroundMode::from_wire("steer"),
        PrintBackgroundMode::Steer
    );
    assert_eq!(
        PrintBackgroundMode::from_wire("exit"),
        PrintBackgroundMode::Exit
    );
    assert_eq!(
        PrintBackgroundMode::from_wire("wat"),
        PrintBackgroundMode::Exit
    );
    assert!(PrintBackgroundMode::Drain.waits());
    assert!(PrintBackgroundMode::Steer.waits());
    assert!(!PrintBackgroundMode::Exit.waits());
}

/// The steer prompt the engine hands the model: one block per completed
/// task, with the output preview attached when the runner kept one.
#[test]
fn test_render_task_notifications() {
    use crate::storage::{TaskNotification, TaskStatus};
    let rendered = render_task_notifications(&[
        TaskNotification {
            task_id: 't'.to_string(),
            description: "run the test suite".into(),
            status: TaskStatus::Completed,
            output_preview: Some("42 passing".into()),
            started_at: 0,
            ended_at: 100,
            session_id: Some("sess-1".into()),
        },
        TaskNotification {
            task_id: "t2".into(),
            description: "stop me".into(),
            status: TaskStatus::Killed,
            output_preview: None,
            started_at: 150,
            ended_at: 200,
            session_id: Some("sess-1".into()),
        },
    ]);
    assert_eq!(
        rendered,
        "Wall time: 0.100 seconds\n\
             Background task t (completed) finished: run the test suite\n\
             42 passing\n\n\
             Wall time: 0.050 seconds\n\
             Background task t2 (killed) finished: stop me"
    );
}

/// `steer`: a completed background task is fed back to the model as a
/// follow-up turn, and the original turn's receipt stays pending until that
/// follow-up has run — the host's `prompt()` covers the whole run.
#[tokio::test]
async fn test_print_steer_feeds_task_notifications_back() {
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    runner
        .spawn_task_with_meta(
            crate::storage::TaskSpawnMeta {
                session_id: Some("sess-steer"),
                kind: "bash",
                subagent_type: None,
                agent_id: None,
            },
            "t1".into(),
            "quick job".into(),
            async { crate::storage::TaskOutcome::Completed("done output".to_string()) },
        )
        .unwrap();
    wait_until(|| runner.pending_notification_count(Some("sess-steer")) == 1).await;

    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("main-response"),
        text_response("steer-response"),
    ]));
    let requests = llm.requests.clone();
    let config = SessionConfig {
        llm,
        callbacks: rpc_callbacks(server),
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: None,
        hook_guard: None,
        print_background: Some(PrintBackgroundPolicy {
            mode: PrintBackgroundMode::Steer,
            ceiling_s: 30,
            max_turns: 5,
        }),
        session_id: Some("sess-steer".into()),
        task_runner: Some(runner),
        toolset: None,
        telemetry: None,
    };
    let session = EngineSession::new(config).await;

    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "hello"), Admission::NewTurn))
        .unwrap();
    let outcome = receipt.outcome().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Ran(_)));

    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2, "steer must run one follow-up turn");
    let steer_call = &calls[1];
    assert!(
        steer_call.iter().any(|m| {
            m.role == "user"
                && m.content
                    .contains("Background task t1 (completed) finished: quick job")
                && m.content.contains("done output")
        }),
        "steer turn missing the task notification: {steer_call:?}"
    );
    assert!(
        steer_call
            .iter()
            .any(|m| m.role == "assistant" && m.content == "main-response"),
        "steer turn history missing the main turn's answer"
    );
}

/// The runner is shared across sessions, so a print turn must not fold
/// another session's completed task into its own follow-up: the settle
/// drains only the notifications of the session it belongs to. Both
/// sessions have a completion pending, so the drain itself is exercised —
/// the steer turn carries this session's task and only that one.
#[tokio::test]
async fn test_print_steer_ignores_another_sessions_notifications() {
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    for (id, session, description) in [
        ("t-mine", "sess-mine", "my job"),
        ("t-other", "sess-other", "someone else's job"),
    ] {
        runner
            .spawn_task_with_meta(
                crate::storage::TaskSpawnMeta {
                    session_id: Some(session),
                    kind: "bash",
                    subagent_type: None,
                    agent_id: None,
                },
                id.into(),
                description.into(),
                async { crate::storage::TaskOutcome::Completed("done output".to_string()) },
            )
            .unwrap();
    }
    wait_until(|| runner.pending_notification_count(Some("sess-mine")) == 1).await;
    wait_until(|| runner.pending_notification_count(Some("sess-other")) == 1).await;

    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("main-response"),
        text_response("steer-response"),
    ]));
    let requests = llm.requests.clone();
    let config = SessionConfig {
        llm,
        callbacks: rpc_callbacks(server),
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: None,
        hook_guard: None,
        print_background: Some(PrintBackgroundPolicy {
            mode: PrintBackgroundMode::Steer,
            ceiling_s: 30,
            max_turns: 5,
        }),
        session_id: Some("sess-mine".into()),
        task_runner: Some(runner.clone()),
        toolset: None,
        telemetry: None,
    };
    let session = EngineSession::new(config).await;

    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "hello"), Admission::NewTurn))
        .unwrap();
    let outcome = receipt.outcome().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Ran(_)));

    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2, "this session's completion steers one turn");
    let steer_call = &calls[1];
    assert!(
        steer_call
            .iter()
            .any(|m| m.role == "user" && m.content.contains("Background task t-mine")),
        "steer turn missing this session's completion: {steer_call:?}"
    );
    assert!(
        !steer_call.iter().any(|m| m.content.contains("t-other")),
        "another session's completion must not reach this turn: {steer_call:?}"
    );
    assert_eq!(
        runner.pending_notification_count(Some("sess-other")),
        1,
        "the other session's notification stays queued for its own settle"
    );
}

/// A print run with an active goal continues the goal itself: the turn
/// loop renders the follow-up prompt (v2's host loop re-prompted, and no
/// native host consumes `goal.continuation`), and the receipt rides along
/// until the continuation turn has run. `drain` on purpose — the goal
/// continuation is mode-independent, the way v2's goal wait preceded the
/// mode check. `max_turns: 1` also proves the shared budget stops the
/// loop: the provider here never completes its goal, so an unbounded run
/// would spin forever.
#[tokio::test]
async fn test_print_run_continues_active_goal() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("main-response"),
        text_response("goal-response"),
    ]));
    let requests = llm.requests.clone();
    let goal: GoalProvider = Arc::new(|| {
        Box::pin(async {
            Some(crate::turn_loop::types::GoalContext {
                goal_id: "g1".into(),
                objective: "ship the thing".into(),
                status: crate::turn_loop::types::GoalStatus::Active,
                token_budget: None,
                turn_budget: None,
                wall_clock_budget_ms: None,
                wall_clock_ms: 0,
                tokens_used: 100,
                turns_used: 1,
            })
        })
    });
    let config = SessionConfig {
        llm,
        callbacks: rpc_callbacks(server),
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: Some(goal),
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: None,
        hook_guard: None,
        print_background: Some(PrintBackgroundPolicy {
            mode: PrintBackgroundMode::Drain,
            ceiling_s: 30,
            max_turns: 1,
        }),
        session_id: Some("sess-goal".into()),
        task_runner: Some(Arc::new(crate::storage::TaskRunner::new(None))),
        toolset: None,
        telemetry: None,
    };
    let session = EngineSession::new(config).await;

    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "hello"), Admission::NewTurn))
        .unwrap();
    let outcome = receipt.outcome().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Ran(_)));

    let calls = requests.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "one goal continuation, then the shared budget stops the loop"
    );
    let continuation = &calls[1];
    assert!(
        continuation
            .iter()
            .any(|m| m.role == "user" && m.content.contains("ship the thing")),
        "continuation turn missing the goal objective: {continuation:?}"
    );
    assert!(
        continuation
            .iter()
            .any(|m| m.role == "assistant" && m.content == "main-response"),
        "continuation turn history missing the main turn's answer"
    );
}

/// The cron fire a print run injects: the documented `<cron-fire>`
/// envelope (renderers strip it before display) with the `CronJobOrigin`
/// the transcript folds into a cron card.
#[test]
fn test_render_cron_fire_matches_documented_envelope() {
    let entry = crate::cron::scheduler::CronEntry {
        id: "a3f9c2".into(),
        cron: "*/5 * * * *".into(),
        prompt: "Check the deploy status".into(),
        recurring: true,
        session_id: None,
        created_at: None,
    };
    assert_eq!(
        render_cron_fire(&entry, 1, false),
        "<cron-fire jobId=\"a3f9c2\" cron=\"*/5 * * * *\" recurring=\"true\" coalescedCount=\"1\" stale=\"false\">\n<prompt>\nCheck the deploy status\n</prompt>\n</cron-fire>"
    );
    assert_eq!(
        cron_fire_origin(&entry, 1, false),
        serde_json::json!({
            "kind": "cron_job",
            "jobId": "a3f9c2",
            "cron": "*/5 * * * *",
            "recurring": true,
            "coalescedCount": 1,
            "stale": false,
        })
    );
}

/// A late delivery reports the real coalesced count and staleness instead
/// of the hardcoded `coalescedCount="1" stale="false"`.
#[test]
fn test_render_cron_fire_reports_coalesced_and_stale() {
    let entry = crate::cron::scheduler::CronEntry {
        id: "old".into(),
        cron: "*/5 * * * *".into(),
        prompt: "Check the deploy status".into(),
        recurring: true,
        session_id: None,
        created_at: None,
    };
    assert_eq!(
        render_cron_fire(&entry, 11, true),
        "<cron-fire jobId=\"old\" cron=\"*/5 * * * *\" recurring=\"true\" coalescedCount=\"11\" stale=\"true\">\n<prompt>\nCheck the deploy status\n</prompt>\n</cron-fire>"
    );
    assert_eq!(
        cron_fire_origin(&entry, 11, true)["stale"],
        serde_json::json!(true)
    );
    assert_eq!(
        cron_fire_origin(&entry, 11, true)["coalescedCount"],
        serde_json::json!(11)
    );
}

#[tokio::test]
async fn test_history_accumulates_across_turns() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("first-response"),
        text_response("second-response"),
    ]));
    let requests = llm.requests.clone();
    let session = make_session(llm, rpc_callbacks(server)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "hello"), Admission::NewTurn))
        .unwrap();
    let mut r2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "world"), Admission::NewTurn))
        .unwrap();
    let o1 = r1.outcome().await.unwrap();
    let o2 = r2.outcome().await.unwrap();
    assert!(matches!(o1, TurnOutcome::Ran(_)));
    assert!(matches!(o2, TurnOutcome::Ran(_)));

    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let turn2 = &calls[1];
    assert!(
        turn2
            .iter()
            .any(|m| m.role == "assistant" && m.content == "first-response"),
        "turn 2 history missing turn 1 assistant: {turn2:?}"
    );
    assert!(
        turn2
            .iter()
            .any(|m| m.role == "user" && m.content == "world")
    );
}

/// The fold must not re-include the history a turn was handed: from the
/// third turn on, a re-folding fold would duplicate every earlier message
/// in the model's context.
#[tokio::test]
async fn test_history_does_not_duplicate_across_three_turns() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("first-response"),
        text_response("second-response"),
        text_response("third-response"),
    ]));
    let requests = llm.requests.clone();
    let session = make_session(llm, rpc_callbacks(server)).await;

    for prompt in ["hello", "world", "again"] {
        let mut receipt = session
            .enqueue_turn(TurnRequest::user(msg("user", prompt), Admission::NewTurn))
            .unwrap();
        assert!(matches!(
            receipt.outcome().await.unwrap(),
            TurnOutcome::Ran(_)
        ));
    }

    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 3);
    let turn3 = &calls[2];
    let hello_count = turn3
        .iter()
        .filter(|m| m.role == "user" && m.content == "hello")
        .count();
    assert_eq!(
        hello_count, 1,
        "turn 3 context must carry each earlier message exactly once: {turn3:?}"
    );
    assert!(
        turn3
            .iter()
            .any(|m| m.role == "assistant" && m.content == "second-response"),
        "turn 3 history missing turn 2 assistant: {turn3:?}"
    );
    assert!(
        turn3
            .iter()
            .any(|m| m.role == "user" && m.content == "again")
    );
}

#[tokio::test]
async fn test_pending_turns_run_in_fifo_order() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("1"),
        text_response("2"),
        text_response("3"),
    ]));
    let session = make_session(llm, rpc_callbacks(server)).await;
    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "a"), Admission::NewTurn))
        .unwrap();
    let mut r2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "b"), Admission::NewTurn))
        .unwrap();
    let mut r3 = session
        .enqueue_turn(TurnRequest::user(msg("user", "c"), Admission::NewTurn))
        .unwrap();
    let o1 = r1.outcome().await.unwrap();
    let o2 = r2.outcome().await.unwrap();
    let o3 = r3.outcome().await.unwrap();
    assert!(matches!(o1, TurnOutcome::Ran(_)));
    assert!(matches!(o2, TurnOutcome::Ran(_)));
    assert!(matches!(o3, TurnOutcome::Ran(_)));
    let status = session.status();
    assert!(status.active_turn_id.is_none());
    assert!(status.pending_turn_ids.is_empty());
}

#[tokio::test]
async fn test_cancel_queued_turn_resolves_immediately() {
    let (llm, gates) = ScriptedLlm::with_gate(vec![text_response("first")]);
    let llm = Arc::new(llm);
    let requests = llm.requests.clone();
    let server = Arc::new(RpcServer::new());
    let session = make_session(llm, rpc_callbacks(server)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "first"), Admission::NewTurn))
        .unwrap();
    wait_until(|| !requests.lock().unwrap().is_empty()).await;
    let mut r2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "second"), Admission::NewTurn))
        .unwrap();
    assert!(session.cancel_turn(Some(r2.turn_id)));
    let o2 = r2.outcome().await.unwrap();
    assert!(matches!(o2, TurnOutcome::CancelledBeforeStart));
    gates.into_iter().next().unwrap().send(()).unwrap();
    let o1 = r1.outcome().await.unwrap();
    assert!(matches!(o1, TurnOutcome::Ran(_)));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// A queued turn cancelled before it starts must announce the cancel and
/// **never** a `turn.ended` — the v2 `cancelWaiter` contract
/// (`loopService.ts:735-750`: `cancelQueueItem` → `publishPromptAborted`,
/// with no `TurnEnded` anywhere on that path). A client that only watches
/// `turn.ended` therefore stays busy forever after such a cancel, which is
/// exactly why `turn.cancel` has to be a wire event.
#[tokio::test]
async fn test_queued_cancel_emits_turn_cancel_and_no_turn_ended() {
    let (llm, gates) = ScriptedLlm::with_gate(vec![text_response("first")]);
    let llm = Arc::new(llm);
    let requests = llm.requests.clone();
    let (callbacks, events) = TurnRecordingCallbacks::new(serde_json::json!({}));
    let session = make_session(llm, Arc::new(callbacks)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "first"), Admission::NewTurn))
        .unwrap();
    wait_until(|| !requests.lock().unwrap().is_empty()).await;
    let mut r2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "second"), Admission::NewTurn))
        .unwrap();
    assert!(session.cancel_turn(Some(r2.turn_id)));
    assert!(matches!(
        r2.outcome().await.unwrap(),
        TurnOutcome::CancelledBeforeStart
    ));

    // The cancel is announced…
    let seen = recorded(&events);
    assert!(
        seen.iter().any(|event| matches!(
            event,
            TurnEvent::Cancel {
                target: Some(TurnCancelTarget::Queued),
                ..
            }
        )),
        "the queued cancel must reach the host: {seen:?}"
    );
    // …and the turn that never started reports no end at all.
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, TurnEvent::Ended { .. })),
        "a turn that never started must not report turn.ended: {seen:?}"
    );

    gates.into_iter().next().unwrap().send(()).unwrap();
    assert!(matches!(r1.outcome().await.unwrap(), TurnOutcome::Ran(_)));
    // Only the turn that actually ran closes with `turn.ended`.
    let seen = recorded(&events);
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, TurnEvent::Ended { .. }))
            .count(),
        1,
        "only the turn that ran reports ended: {seen:?}"
    );
}

#[tokio::test]
async fn test_active_or_new_steers_when_active() {
    let (llm, gates) = ScriptedLlm::with_gate(vec![text_response("active-response")]);
    let llm = Arc::new(llm);
    let server = Arc::new(RpcServer::new());
    let session = make_session(llm, rpc_callbacks(server)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "first"), Admission::NewTurn))
        .unwrap();
    wait_until(|| session.status().active_turn_id == Some(r1.turn_id)).await;
    let mut r2 = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "steer-me"),
            Admission::ActiveOrNewTurn,
        ))
        .unwrap();
    assert_eq!(r2.turn_id, r1.turn_id);
    gates.into_iter().next().unwrap().send(()).unwrap();
    let o1 = r1.outcome().await.unwrap();
    let o2 = r2.outcome().await.unwrap();
    assert!(matches!(o1, TurnOutcome::Ran(_)));
    assert!(matches!(o2, TurnOutcome::Ran(_)));
}

#[tokio::test]
async fn test_steer_fires_the_turn_signal_and_draining_refreshes_it() {
    // #3697: a steering message fires the running turn's signal so a
    // blocking `WaitFor` can end early; once the turn has drained the
    // message the signal is refreshed, so the next wait is not aborted by
    // the steer that already ended the previous one.
    let server = Arc::new(RpcServer::new());
    let queue: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let slot: Arc<Mutex<Option<crate::subagent::types::ParentCancel>>> = Arc::new(Mutex::new(
        Some(crate::subagent::types::ParentCancel::new()),
    ));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(
        SteerQueueCallbacks::new(rpc_callbacks(server), queue.clone())
            .with_steer_slot(Some(slot.clone())),
    );
    let current = || {
        slot.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .unwrap()
            .clone()
    };
    assert!(!current().triggered(), "a turn starts unfired");

    // A steer joins the active turn: queued, and the signal fires.
    queue
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(msg("user", "steer-me"));
    current().trigger();
    assert!(current().triggered());

    // Draining it hands the message to the turn and refreshes the signal.
    let drained = callbacks.drain_steers().await.unwrap();
    assert_eq!(drained.len(), 1);
    assert!(
        !current().triggered(),
        "draining must refresh the signal for the next wait"
    );

    // An empty drain must not refresh: nothing was consumed, so a wait that
    // the steer already ended keeps observing the fire.
    current().trigger();
    let drained = callbacks.drain_steers().await.unwrap();
    assert!(drained.is_empty());
    assert!(
        current().triggered(),
        "an empty drain must leave the signal alone"
    );
}

#[tokio::test]
async fn test_steering_into_the_active_turn_fires_the_signal() {
    let (llm, gates) = ScriptedLlm::with_gate(vec![text_response("steered")]);
    let server = Arc::new(RpcServer::new());
    let slot: Arc<Mutex<Option<crate::subagent::types::ParentCancel>>> = Arc::new(Mutex::new(None));
    let session =
        make_session_with_steer(Arc::new(llm), rpc_callbacks(server), Some(slot.clone())).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "first"), Admission::NewTurn))
        .unwrap();
    wait_until(|| session.status().active_turn_id == Some(r1.turn_id)).await;
    wait_until(|| {
        slot.lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some()
    })
    .await;
    let signal = slot
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .unwrap()
        .clone();
    assert!(
        !signal.triggered(),
        "the turn starts with an unfired signal"
    );

    let mut r2 = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "steer-me"),
            Admission::ActiveOrNewTurn,
        ))
        .unwrap();
    assert_eq!(r2.turn_id, r1.turn_id);
    assert!(
        signal.triggered(),
        "steering into the active turn must fire its signal"
    );

    gates.into_iter().next().unwrap().send(()).unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), r1.outcome())
        .await
        .expect("turn 1 never settled");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), r2.outcome())
        .await
        .expect("the steer receipt never settled");
}

#[tokio::test]
async fn test_active_turn_only_without_active_errors() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![]));
    let session = make_session(llm, rpc_callbacks(server)).await;
    let result = session.enqueue_turn(TurnRequest::user(
        msg("user", "x"),
        Admission::ActiveTurnOnly,
    ));
    assert!(result.is_err());
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected ActiveTurnOnly without an active turn to error"),
    };
    assert!(err.contains("requires an active turn"));
}

#[tokio::test]
async fn test_pump_wakes_after_idle_gap() {
    // Regression: the handle must share the pump's wakeup `Notify`. With a
    // second one, an enqueue arriving after the pump parked for an idle gap
    // never woke it — the REPL hung on its second prompt.
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("one"),
        text_response("two"),
    ]));
    let session = make_session(llm, rpc_callbacks(server)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "a"), Admission::NewTurn))
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), r1.outcome())
            .await
            .expect("turn 1 never ran"),
        Ok(TurnOutcome::Ran(_))
    ));

    // Let the pump reach its idle await before the next enqueue.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut r2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "b"), Admission::NewTurn))
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(5), r2.outcome())
            .await
            .expect("turn 2 never ran — pump stayed parked after the idle gap"),
        Ok(TurnOutcome::Ran(_))
    ));
}

/// Records every turn event and answers the `turn` state domain from a
/// fixed value, so clock hydration and event dispatch share one harness.
struct TurnRecordingCallbacks {
    events: Arc<Mutex<Vec<TurnEvent>>>,
    emitted: Arc<Mutex<Vec<serde_json::Value>>>,
    turn_state: serde_json::Value,
}

type RecordedEvents = Arc<Mutex<Vec<TurnEvent>>>;
type EmittedEvents = Arc<Mutex<Vec<serde_json::Value>>>;

impl TurnRecordingCallbacks {
    fn new(turn_state: serde_json::Value) -> (Self, RecordedEvents) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                emitted: Arc::new(Mutex::new(Vec::new())),
                turn_state,
            },
            events,
        )
    }

    fn new_with_emitted(turn_state: serde_json::Value) -> (Self, RecordedEvents, EmittedEvents) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let emitted = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                events: events.clone(),
                emitted: emitted.clone(),
                turn_state,
            },
            events,
            emitted,
        )
    }
}

impl HostCallbacks for TurnRecordingCallbacks {
    fn llm_chat(
        &self,
        _: LlmChatRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn check_permission(
        &self,
        _: PermissionCheckRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }
    fn state_read(
        &self,
        request: StateReadRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<StateReadResponse, String>> {
        let value = if request.domain == "turn" {
            self.turn_state.clone()
        } else {
            serde_json::Value::Null
        };
        Box::pin(async move { Ok(StateReadResponse { value }) })
    }
    fn turn_event(&self, event: TurnEvent) {
        self.events.lock().unwrap().push(event);
    }
    fn emit_event(&self, event: serde_json::Value) {
        self.emitted.lock().unwrap().push(event);
    }
}

fn recorded(events: &Arc<Mutex<Vec<TurnEvent>>>) -> Vec<TurnEvent> {
    events.lock().unwrap().clone()
}

#[tokio::test]
async fn test_turn_events_dispatch_in_order() {
    let (callbacks, events) = TurnRecordingCallbacks::new(serde_json::json!({}));
    let llm = Arc::new(ScriptedLlm::simple(vec![text_response("answer")]));
    let session = make_session(llm, Arc::new(callbacks)).await;

    let mut receipt = session
        .enqueue_turn(TurnRequest {
            prompt: msg("user", "hello"),
            admission: Admission::NewTurn,
            input: serde_json::json!([{"type": "text", "text": "hello"}]),
            origin: serde_json::json!({"kind": "user"}),
        })
        .unwrap();
    receipt.outcome().await.unwrap();

    let seen = recorded(&events);
    assert_eq!(seen.len(), 3, "prompt + started + ended: {seen:?}");
    assert_eq!(
        seen[0],
        TurnEvent::Prompt {
            turn_id: 0,
            input: serde_json::json!([{"type": "text", "text": "hello"}]),
            origin: serde_json::json!({"kind": "user"}),
        },
        "the host's own payload must come back unchanged"
    );
    assert_eq!(
        seen[1],
        TurnEvent::Started {
            turn_id: 0,
            origin: serde_json::json!({"kind": "user"}),
        }
    );
    match &seen[2] {
        TurnEvent::Ended {
            turn_id,
            reason,
            error,
            duration_ms,
        } => {
            assert_eq!(*turn_id, 0);
            assert_eq!(*reason, TurnEndReason::Completed);
            assert!(error.is_none());
            assert!(duration_ms.is_some());
        }
        other => panic!("expected turn.ended, got {other:?}"),
    }
}

#[tokio::test]
async fn test_cancel_dispatches_turn_cancel() {
    let (llm, gates) = ScriptedLlm::with_gate(vec![text_response("first")]);
    let llm = Arc::new(llm);
    let (callbacks, events) = TurnRecordingCallbacks::new(serde_json::json!({}));
    let session = make_session(llm, Arc::new(callbacks)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "first"), Admission::NewTurn))
        .unwrap();
    wait_until(|| session.status().active_turn_id == Some(r1.turn_id)).await;
    let r2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "second"), Admission::NewTurn))
        .unwrap();

    assert!(session.cancel_turn(Some(r2.turn_id)));
    assert!(recorded(&events).contains(&TurnEvent::Cancel {
        turn_id: Some(r2.turn_id),
        target: Some(TurnCancelTarget::Queued),
        reason: Some(TurnCancelReason::UserCancelled),
    }));

    assert!(session.cancel_turn(Some(r1.turn_id)));
    assert!(recorded(&events).contains(&TurnEvent::Cancel {
        turn_id: Some(r1.turn_id),
        target: Some(TurnCancelTarget::Active),
        reason: Some(TurnCancelReason::UserCancelled),
    }));

    gates.into_iter().next().unwrap().send(()).unwrap();
    r1.outcome().await.unwrap();
    assert!(!session.cancel_turn(Some(9999)));
    assert_eq!(
        recorded(&events)
            .iter()
            .filter(|e| matches!(e, TurnEvent::Cancel { .. }))
            .count(),
        2,
        "an id matching nothing must not report a cancellation"
    );
}

/// The turn-lifecycle hook wiring: `TurnStarted` when a turn starts,
/// `UserPromptQueued` when a prompt waits behind an active turn, and
/// `Interrupt` when the active turn is cancelled — each reaching the
/// session's hook guard (remove the wiring at any site and its capture
/// file stays empty).
#[tokio::test]
async fn turn_lifecycle_hooks_fire_from_the_session_wiring() {
    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let started = dir.path().join("turn-started.json");
    let queued = dir.path().join("prompt-queued.json");
    let interrupt = dir.path().join("interrupt.json");
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        capture::hook("TurnStarted", "user", &started),
        capture::hook("UserPromptQueued", "second", &queued),
        capture::hook("Interrupt", "", &interrupt),
    ]));

    let (llm, gates) =
        ScriptedLlm::with_gate(vec![text_response("first"), text_response("second")]);
    let server = Arc::new(RpcServer::new());
    let session = EngineSession::new(SessionConfig {
        llm: Arc::new(llm),
        callbacks: rpc_callbacks(server),
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: None,
        hook_guard: Some(guard),
        print_background: None,
        session_id: None,
        task_runner: None,
        toolset: None,
        telemetry: None,
    })
    .await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "first prompt"),
            Admission::NewTurn,
        ))
        .unwrap();
    wait_until(|| session.status().active_turn_id == Some(r1.turn_id)).await;
    let payload = capture::wait(&started).await;
    assert_eq!(payload["hook_event_name"], "TurnStarted");
    assert_eq!(payload["turn_id"], r1.turn_id);
    assert_eq!(payload["origin_kind"], "user");
    assert_eq!(payload["prompt"], "first prompt");

    // A prompt enqueued while a turn is active fires UserPromptQueued
    // (the matcher runs against the prompt text).
    let mut r2 = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "the second prompt"),
            Admission::NewTurn,
        ))
        .unwrap();
    let queued_payload = capture::wait(&queued).await;
    assert_eq!(queued_payload["hook_event_name"], "UserPromptQueued");
    assert_eq!(queued_payload["prompt_id"], r2.turn_id);
    assert_eq!(queued_payload["prompt"], "the second prompt");
    assert!(queued_payload["queue_length"].as_u64().unwrap() >= 1);

    // Cancelling the active turn ends it with the Aborted stop reason ⇒
    // Interrupt, and releases the gate so the queued turn can run. The
    // sleep gives the cancel flag's 25 ms poll time to cancel the token
    // while the gated LLM call is still in flight.
    assert!(session.cancel_turn(Some(r1.turn_id)));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    for gate in gates {
        let _ = gate.send(());
    }
    r1.outcome().await.unwrap();
    let interrupt_payload = capture::wait(&interrupt).await;
    assert_eq!(interrupt_payload["hook_event_name"], "Interrupt");
    assert_eq!(interrupt_payload["turn_id"], r1.turn_id);

    r2.outcome().await.unwrap();
}

/// A turn that ends `Failed` (a filtered response) reaches `StopFailure`
/// with the turn's error code and message — remove the arm at the turn-end
/// dispatch and the capture file stays empty.
#[tokio::test]
async fn stop_failure_hook_fires_for_a_failed_turn() {
    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let failed = dir.path().join("stop-failure.json");
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        capture::hook("StopFailure", "provider.filtered", &failed),
    ]));

    let llm = Arc::new(ScriptedLlm::simple(vec![LLMChatResponse {
        content: "blocked".into(),
        thinking: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: Some("content_filter".into()),
        usage: TokenUsage::default(),
        timing: None,
    }]));
    let server = Arc::new(RpcServer::new());
    let session = EngineSession::new(SessionConfig {
        llm,
        callbacks: rpc_callbacks(server),
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        tool_defs: Arc::new(|| Box::pin(async { Vec::new() })),
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: None,
        hook_guard: Some(guard),
        print_background: None,
        session_id: None,
        task_runner: None,
        toolset: None,
        telemetry: None,
    })
    .await;

    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "hi"), Admission::NewTurn))
        .unwrap();
    let outcome = receipt.outcome().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Ran(_)));

    let payload = capture::wait(&failed).await;
    assert_eq!(payload["hook_event_name"], "StopFailure");
    assert_eq!(payload["error_type"], "provider.filtered");
    assert!(
        payload["error_message"]
            .as_str()
            .is_some_and(|message| message.contains("safety")),
        "the turn's error message rides the payload: {payload}"
    );
}

/// `SessionHeartbeat` fires only when a listener is configured: no guard
/// and a guard without the event both stay silent, and the payload
/// carries the uptime with the (engine-untracked) empty session title.
#[tokio::test]
async fn heartbeat_tick_fires_only_for_a_listener() {
    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let beat = dir.path().join("heartbeat.json");
    let other = dir.path().join("other-event.json");

    heartbeat_tick(None, 4_242).await;
    let wrong_event = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        capture::hook("SessionStart", "", &other),
    ]));
    heartbeat_tick(Some(&wrong_event), 4_242).await;
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!other.exists(), "a non-heartbeat listener must not fire");

    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        capture::hook("SessionHeartbeat", "", &beat),
    ]));
    heartbeat_tick(Some(&guard), 4_242).await;
    let payload = capture::wait(&beat).await;
    assert_eq!(payload["hook_event_name"], "SessionHeartbeat");
    assert_eq!(payload["uptime_ms"], 4_242);
    assert_eq!(payload["session_title"], "");
}

#[tokio::test]
async fn test_clock_continues_host_turn_sequence() {
    // A resumed session keeps counting where the host's fold left off
    // (v2 `turnKey.nextTurnId` is 0-based), rather than restarting at zero.
    let (callbacks, events) = TurnRecordingCallbacks::new(serde_json::json!({ "nextTurnId": 41 }));
    let llm = Arc::new(ScriptedLlm::simple(vec![text_response("ok")]));
    let session = make_session(llm, Arc::new(callbacks)).await;

    let mut receipt = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "after resume"),
            Admission::NewTurn,
        ))
        .unwrap();
    assert_eq!(receipt.turn_id, 41);
    receipt.outcome().await.unwrap();
    assert!(matches!(
        recorded(&events)[0],
        TurnEvent::Prompt { turn_id: 41, .. }
    ));
}

#[tokio::test]
async fn test_host_without_turn_domain_starts_at_zero() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![text_response("ok")]));
    // RpcHostCallbacks has no state bridge wired, so the read fails.
    let session = make_session(llm, rpc_callbacks(server)).await;
    let receipt = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "first-ever"),
            Admission::NewTurn,
        ))
        .unwrap();
    assert_eq!(receipt.turn_id, 0);
}

#[test]
fn turn_end_reasons_map_onto_v2s_four_values() {
    use crate::turn_loop::types::LoopTurnStopReason as Stop;
    let cases = [
        (Stop::EndTurn, TurnEndReason::Completed),
        (Stop::MaxTokens, TurnEndReason::Completed),
        (Stop::Paused, TurnEndReason::Blocked),
        (Stop::BudgetLimited, TurnEndReason::Blocked),
        (Stop::Aborted, TurnEndReason::Cancelled),
        // v2 fails the turn on a filtered response (loopService.ts:877-882)
        // and on step-budget exhaustion (loopService.ts:956-962).
        (Stop::Filtered, TurnEndReason::Failed),
        (Stop::MaxSteps, TurnEndReason::Failed),
        (Stop::Unknown, TurnEndReason::Failed),
    ];
    for (stop, expected) in cases {
        assert_eq!(end_reason_of(&stop), expected, "{stop:?}");
    }
    assert_eq!(
        turn_end_error_payload(&Stop::MaxSteps, 7),
        Some(serde_json::json!({
            "type": "error",
            "code": "loop.max_steps_exceeded",
            "message": "Turn exceeded maxSteps=7. If max_steps_per_turn is too small, raise it in config.toml (loop_control.max_steps_per_turn), or run \"/update-config\" to update it, then \"/reload\".",
            "retryable": false,
        })),
        "the max_steps payload mirrors createMaxStepsExceededError (loop.ts:20-27)"
    );
    assert_eq!(
        turn_end_error_payload(&Stop::Filtered, 3),
        Some(serde_json::json!({
            "type": "error",
            "code": "provider.filtered",
            "message": "Provider safety policy blocked the response.",
            "retryable": false,
        }))
    );
    assert_eq!(turn_end_error_payload(&Stop::EndTurn, 3), None);
}

#[test]
fn test_turn_failure_payload_classifies_the_llm_status() {
    // The LLM layer's transport failures carry the HTTP status in the
    // message (llm/http.rs:287); everything else falls back to `internal`.
    let cases = [
        (
            "llm http status 401 Unauthorized: bad key",
            "provider.auth_error",
        ),
        (
            "llm http status 403 Forbidden: no access",
            "provider.auth_error",
        ),
        (
            "llm http status 404 Not Found: no such model",
            "provider.not_found",
        ),
        (
            "llm http status 429 Too Many Requests: slow down",
            "provider.rate_limit",
        ),
        (
            "llm http status 503 Service Unavailable: busy",
            "provider.overloaded",
        ),
        (
            "llm http status 400 Bad Request: invalid schema",
            "provider.api_error",
        ),
        ("connection reset by peer", "provider.connection_error"),
        ("request timed out after 60s", "provider.connection_error"),
        ("tool registry is empty", "internal"),
        // The shapes the native transport actually produces, wrapped by
        // the retry layer the way a real turn sees them.
        (
            "LLM call failed after 1 attempts: llm transport error connect: \
                 error sending request for url (http://127.0.0.1:1/v1/chat/completions)",
            "provider.connection_error",
        ),
        (
            "LLM call failed after 3 attempts: llm transport error timeout: operation timed out",
            "provider.connection_error",
        ),
        (
            "LLM call failed after 1 attempts: llm transport error decode: invalid json body",
            "provider.api_error",
        ),
        (
            "LLM call failed after 1 attempts: llm transport error invalid_request: bad url",
            "provider.api_error",
        ),
    ];
    for (message, expected) in cases {
        assert_eq!(turn_failure_code(message), expected, "{message}");
    }
    assert_eq!(
        turn_failure_payload("llm http status 400 Bad Request: invalid schema"),
        serde_json::json!({
            "type": "error",
            "code": "provider.api_error",
            "message": "llm http status 400 Bad Request: invalid schema",
            "retryable": false,
        })
    );
}

struct FailingLlm;

impl LLM for FailingLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "failing-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn transport(&self) -> &'static str {
        "native-http"
    }
    fn chat(
        &self,
        _: LLMChatParams,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async { Err("provider is offline".into()) })
    }
}

#[tokio::test]
async fn test_failed_turn_reports_ended_with_error() {
    let (callbacks, events, emitted) =
        TurnRecordingCallbacks::new_with_emitted(serde_json::json!({}));
    let session = make_session(Arc::new(FailingLlm), Arc::new(callbacks)).await;
    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "boom"), Admission::NewTurn))
        .unwrap();
    assert!(receipt.outcome().await.is_err());
    let seen = recorded(&events);
    assert!(
        matches!(
            seen.last(),
            Some(TurnEvent::Ended {
                reason: TurnEndReason::Failed,
                error: Some(_),
                ..
            })
        ),
        "a turn that never produced a stop reason must still close: {seen:?}"
    );
    // The payload is a protocol `KimiErrorPayload`, not a bare string: the
    // host's terminal-state display reads `error.code` off it.
    let Some(TurnEvent::Ended {
        error: Some(payload),
        ..
    }) = seen.last()
    else {
        panic!("expected a failed turn.ended: {seen:?}");
    };
    assert_eq!(payload["code"], "internal");
    assert_eq!(payload["message"], "provider is offline");
    assert_eq!(payload["retryable"], false);
    // v2 dispatches a separate error event alongside the failed turn
    // (loopService.ts:1533-1537); without it a turn that dies before any
    // output leaves the user with no explanation at all.
    let emitted = emitted.lock().unwrap().clone();
    assert_eq!(
        emitted,
        vec![
            // The loop opens the step before the request goes out, so a
            // provider refusal leaves the boundary standing ahead of the
            // error. v2 would close it with `turn.step.interrupted`, which
            // this engine does not emit.
            serde_json::json!({
                "type": "llm.step.begin",
                "agent_id": "main",
                "turn_id": "turn-0",
                "step": 1,
            }),
            serde_json::json!({
                "type": "error",
                "code": "internal",
                "message": "provider is offline",
                "retryable": false,
            }),
        ],
        "the failure must reach the host's error channel"
    );
}

#[tokio::test]
async fn test_cancel_without_id_reports_the_active_turn_id() {
    let (llm, gates) = ScriptedLlm::with_gate(vec![text_response("first")]);
    let (callbacks, events) = TurnRecordingCallbacks::new(serde_json::json!({}));
    let session = make_session(Arc::new(llm), Arc::new(callbacks)).await;

    let mut r1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "a"), Admission::NewTurn))
        .unwrap();
    wait_until(|| session.status().active_turn_id == Some(r1.turn_id)).await;

    assert!(session.cancel_turn(None));
    assert!(recorded(&events).contains(&TurnEvent::Cancel {
        turn_id: Some(r1.turn_id),
        target: Some(TurnCancelTarget::Active),
        reason: Some(TurnCancelReason::UserCancelled),
    }));

    gates.into_iter().next().unwrap().send(()).unwrap();
    let _ = r1.outcome().await;
}

// ── M1c: quiescence / backpressure / settled ────────────────────────────

#[tokio::test]
async fn test_settled_resolves_immediately_when_idle() {
    let server = Arc::new(RpcServer::new());
    let session = make_session(
        Arc::new(ScriptedLlm::simple(vec![text_response("unused")])),
        rpc_callbacks(server),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_millis(100), session.settled())
        .await
        .expect("settled must resolve immediately on an idle session");
}

#[tokio::test]
async fn test_settled_waits_for_active_turn() {
    let (llm, mut gates) = ScriptedLlm::with_gate(vec![text_response("gated")]);
    let server = Arc::new(RpcServer::new());
    let session = make_session(Arc::new(llm), rpc_callbacks(server)).await;
    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "a"), Admission::NewTurn))
        .unwrap();
    wait_until(|| session.status().active_turn_id.is_some()).await;

    let settled = session.settled();
    tokio::pin!(settled);
    // The turn is gated open — settled must not resolve while it runs.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut settled)
            .await
            .is_err(),
        "settled resolved while a turn was active"
    );

    gates.remove(0).send(()).unwrap();
    let _ = receipt.outcome().await;
    tokio::time::timeout(std::time::Duration::from_millis(1000), settled)
        .await
        .expect("settled must resolve once the turn finished");
}

#[tokio::test]
async fn test_quiescence_holds_turns_and_release_replays_in_order() {
    let llm = Arc::new(ScriptedLlm::simple(vec![
        text_response("first"),
        text_response("second"),
    ]));
    let requests = llm.requests.clone();
    let server = Arc::new(RpcServer::new());
    let session = make_session(llm, rpc_callbacks(server)).await;

    let guard = session
        .try_acquire_quiescence()
        .expect("an idle session must grant quiescence");
    let mut held1 = session
        .enqueue_turn(TurnRequest::user(msg("user", "held-a"), Admission::NewTurn))
        .unwrap();
    let mut held2 = session
        .enqueue_turn(TurnRequest::user(msg("user", "held-b"), Admission::NewTurn))
        .unwrap();
    assert!(
        requests.lock().unwrap().is_empty(),
        "no turn may start while quiescence is held"
    );

    drop(guard);

    let o1 = held1.outcome().await.unwrap();
    let o2 = held2.outcome().await.unwrap();
    assert!(matches!(o1, TurnOutcome::Ran(_)));
    assert!(matches!(o2, TurnOutcome::Ran(_)));
    let calls = requests.lock().unwrap();
    assert_eq!(calls.len(), 2, "both held turns must run after release");
    assert!(calls[0].iter().any(|m| m.content == "held-a"));
    assert!(calls[1].iter().any(|m| m.content == "held-b"));
}

#[tokio::test]
async fn test_try_acquire_quiescence_fails_while_turns_outstanding() {
    let (llm, mut gates) = ScriptedLlm::with_gate(vec![text_response("b"), text_response("a")]);
    let server = Arc::new(RpcServer::new());
    let session = make_session(Arc::new(llm), rpc_callbacks(server)).await;
    let mut receipt_a = session
        .enqueue_turn(TurnRequest::user(msg("user", "a"), Admission::NewTurn))
        .unwrap();
    wait_until(|| session.status().active_turn_id == Some(receipt_a.turn_id)).await;
    // chat pops gates LIFO, so turn a is parked on gates[1].
    assert!(
        session.try_acquire_quiescence().is_none(),
        "an active turn must deny quiescence"
    );
    // A pending turn denies too.
    let mut receipt_b = session
        .enqueue_turn(TurnRequest::user(
            msg("user", "b"),
            Admission::ActiveOrNextTurn,
        ))
        .unwrap();
    assert!(
        session.try_acquire_quiescence().is_none(),
        "a pending turn must deny quiescence"
    );
    // Finish turn a; turn b starts and parks on the remaining gate.
    gates.swap_remove(1).send(()).unwrap();
    let _ = receipt_a.outcome().await;
    wait_until(|| session.status().active_turn_id == Some(receipt_b.turn_id)).await;
    assert!(session.try_acquire_quiescence().is_none());
    gates.swap_remove(0).send(()).unwrap();
    let _ = receipt_b.outcome().await;
    wait_until(|| session.is_settled()).await;
    assert!(session.try_acquire_quiescence().is_some());
}

#[tokio::test]
async fn test_cancel_held_turn_resolves_and_settles() {
    let llm = Arc::new(ScriptedLlm::simple(vec![text_response("unused")]));
    let (callbacks, events) = TurnRecordingCallbacks::new(serde_json::json!({}));
    let session = make_session(llm, Arc::new(callbacks)).await;

    let guard = session.try_acquire_quiescence().unwrap();
    let mut held = session
        .enqueue_turn(TurnRequest::user(msg("user", "held"), Admission::NewTurn))
        .unwrap();
    assert!(session.cancel_turn(Some(held.turn_id)));
    drop(guard);

    let outcome = held.outcome().await.unwrap();
    assert!(
        matches!(outcome, TurnOutcome::CancelledBeforeStart),
        "a cancelled held turn must never run"
    );
    assert!(recorded(&events).contains(&TurnEvent::Cancel {
        turn_id: Some(held.turn_id),
        target: Some(TurnCancelTarget::Queued),
        reason: Some(TurnCancelReason::UserCancelled),
    }));
    tokio::time::timeout(std::time::Duration::from_millis(1000), session.settled())
        .await
        .expect("cancelling the only held turn must settle the session");
}

/// A finished turn folds its usage into the model bucket the pump recorded
/// (`EngineSession::status().usage`), so the host can read it off the live
/// session shape without re-deriving anything in TS.
#[tokio::test]
async fn usage_is_folded_per_model_and_reported_by_status() {
    let server = Arc::new(RpcServer::new());
    let llm = Arc::new(ScriptedLlm::simple(vec![LLMChatResponse {
        content: "done".into(),
        thinking: Vec::new(),
        tool_calls: Vec::new(),
        finish_reason: Some("stop".into()),
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            input_cache_read: 3,
            input_cache_creation: 2,
        },
        timing: None,
    }]));
    let session = make_session(llm, rpc_callbacks(server)).await;

    let mut receipt = session
        .enqueue_turn(TurnRequest::user(msg("user", "hello"), Admission::NewTurn))
        .unwrap();
    assert!(matches!(
        receipt.outcome().await.unwrap(),
        TurnOutcome::Ran(_)
    ));

    let status = session.status();
    let bucket = status
        .usage
        .by_model
        .get("scripted-llm")
        .expect("the turn's model bucket");
    assert_eq!(bucket.input_other, 10);
    assert_eq!(bucket.output, 5);
    assert_eq!(bucket.input_cache_read, 3);
    assert_eq!(bucket.input_cache_creation, 2);
    assert_eq!(status.usage.total.output, 5);
    assert_eq!(status.usage.current_turn.map(|turn| turn.output), Some(5));
}

/// v2 undo anchors: a user turn snapshots durable state, a continuation
/// (goal follow-up / cron fire / task wake) does not. The host's undo can
/// only cut user turns, so a continuation checkpoint would shift the stack
/// against the history and restore the wrong state.
#[tokio::test]
async fn user_turns_checkpoint_state_and_continuations_do_not() {
    struct CountingCallbacks {
        inner: Arc<dyn HostCallbacks>,
        checkpoints: Arc<std::sync::atomic::AtomicU32>,
    }

    impl HostCallbacks for CountingCallbacks {
        fn llm_chat(
            &self,
            request: LlmChatRequest,
        ) -> futures_util::future::BoxFuture<'static, Result<LlmChatResponse, String>> {
            self.inner.llm_chat(request)
        }

        fn execute_tool(
            &self,
            request: ToolExecuteRequest,
        ) -> futures_util::future::BoxFuture<'static, Result<ToolExecuteResponse, String>> {
            self.inner.execute_tool(request)
        }

        fn check_permission(
            &self,
            request: PermissionCheckRequest,
        ) -> futures_util::future::BoxFuture<'static, Result<PermissionDecision, String>> {
            self.inner.check_permission(request)
        }

        fn checkpoint_state(&self) -> futures_util::future::BoxFuture<'static, Result<(), String>> {
            self.checkpoints
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    let server = Arc::new(RpcServer::new());
    let checkpoints = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(CountingCallbacks {
        inner: rpc_callbacks(server),
        checkpoints: checkpoints.clone(),
    });
    let session = make_session(
        Arc::new(ScriptedLlm::simple(vec![
            text_response("one"),
            text_response("two"),
        ])),
        callbacks,
    )
    .await;

    let mut user = session
        .enqueue_turn(TurnRequest::user(msg("user", "hello"), Admission::NewTurn))
        .unwrap();
    assert!(matches!(user.outcome().await.unwrap(), TurnOutcome::Ran(_)));
    assert_eq!(
        checkpoints.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a user turn is an undo anchor"
    );

    let mut continuation = session
        .enqueue_turn(TurnRequest {
            prompt: msg("user", "keep going"),
            input: serde_json::json!([]),
            origin: serde_json::json!({ "kind": "system_trigger", "name": "goal_continuation" }),
            admission: Admission::NewTurn,
        })
        .unwrap();
    assert!(matches!(
        continuation.outcome().await.unwrap(),
        TurnOutcome::Ran(_)
    ));
    assert_eq!(
        checkpoints.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a continuation is not an undo anchor, so it must not push a checkpoint"
    );
}

/// The undo-anchor rule mirrors the host's `isUndoAnchorPrompt`
/// (`sdk-client/helpers.ts`): plain user prompts and user-slash skill /
/// plugin commands anchor, everything else — including the user prompt a
/// steer folds into a running turn — does not.
#[test]
fn undo_anchor_origins_match_the_host_predicate() {
    for anchor in [
        serde_json::json!({ "kind": "user" }),
        serde_json::json!({ "kind": "skill_activation", "trigger": "user-slash" }),
        serde_json::json!({ "kind": "plugin_command", "trigger": "user-slash" }),
        serde_json::json!({}),
    ] {
        assert!(origin_is_undo_anchor(&anchor), "should anchor: {anchor}");
    }
    for non_anchor in [
        serde_json::json!({ "kind": "user", "inTurn": true }),
        serde_json::json!({ "kind": "system_trigger", "name": "goal_continuation" }),
        serde_json::json!({ "kind": "cron_fire" }),
        serde_json::json!({ "kind": "skill_activation", "trigger": "model" }),
        serde_json::json!({ "kind": "plugin_command", "trigger": "model" }),
    ] {
        assert!(
            !origin_is_undo_anchor(&non_anchor),
            "must not anchor: {non_anchor}"
        );
    }
}
