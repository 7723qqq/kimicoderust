use super::super::*;
use super::common::*;
use super::*;

/// An LLM that stalls briefly before answering, so a `timeout_ms` of
/// `0` misread as "expire immediately" would fail the turn.
struct SleepyLlm;
impl LLM for SleepyLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "sleepy-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<TurnChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            Ok(TurnChatResponse {
                content: "finished after the stall".into(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

#[tokio::test]
async fn zero_timeout_means_no_timeout() {
    // v2 `taskService` arms the timeout only when `timeoutMs > 0`:
    // an explicit `0` disables it (it must neither expire immediately
    // nor fold into the 2h default's timeout error path).
    let manager = manager_with(Arc::new(SleepyLlm)).await;
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "research".into(),
            description: "d".into(),
            system_prompt: "You research.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "research", "prompt": "go" }),
        Some(0),
        None,
        None,
        None,
    )
    .await
    .expect("the run completes");
    assert!(
        !result.is_error,
        "timeout 0 must not kill the run: {}",
        result.content
    );
    assert!(result.content.contains("finished after the stall"));
}

#[tokio::test]
async fn parent_cancellation_interrupts_the_subagent() {
    let manager = manager_with(Arc::new(ToolForeverLlm)).await;
    register_looper(&manager).await;
    let parent_cancel = ParentCancel::new();
    let signal = parent_cancel.clone();
    let abort_task = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        signal.trigger();
    });
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "looper", "prompt": "loop until aborted" }),
        Some(60_000),
        Some(&parent_cancel),
        None,
        None,
    )
    .await
    .expect("abort is a native outcome, not a fallback");
    abort_task.await.unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains(USER_INTERRUPTED_SUBAGENT_MESSAGE),
        "parent abort reports the user-interrupted message: {}",
        result.content
    );
}

/// LLM whose first *working* answer carries partial text plus a tool call
/// and whose later answers wait on `release` — a kill landing mid-turn
/// leaves an aborted turn whose messages hold the partial output.
/// `seed_first` prepends one plain answer, so the resume test can complete
/// an initial run before the stop-driven one.
struct PartialThenBlockLlm {
    calls: std::sync::atomic::AtomicU32,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    seed_first: bool,
}

impl PartialThenBlockLlm {
    fn new(
        seed_first: bool,
    ) -> (
        Arc<Self>,
        Arc<tokio::sync::Notify>,
        Arc<tokio::sync::Notify>,
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let llm = Arc::new(Self {
            calls: std::sync::atomic::AtomicU32::new(0),
            entered: entered.clone(),
            release: release.clone(),
            seed_first,
        });
        (llm, entered, release)
    }
}

impl LLM for PartialThenBlockLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "partial-then-block-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<TurnChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        let seed_first = self.seed_first;
        Box::pin(async move {
            let echo_call = |id: &str| ToolCall {
                id: id.into(),
                name: "echo".into(),
                arguments: serde_json::json!({}),
                extras: None,
                args_parse_error: None,
            };
            // Seeding mode: the first call answers plainly and ends the
            // turn, so the initial run completes with a resumable history.
            if seed_first && call == 0 {
                return Ok(TurnChatResponse {
                    content: "seed answer".into(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                });
            }
            let first_working = (!seed_first && call == 0) || (seed_first && call == 1);
            if first_working {
                return Ok(TurnChatResponse {
                    content: "partial findings so far".into(),
                    thinking: vec![],
                    tool_calls: vec![echo_call("tc-partial")],
                    finish_reason: Some("tool_calls".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                });
            }
            entered.notify_one();
            release.notified().await;
            Ok(TurnChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls: vec![echo_call(&format!("tc-{}", fastrand::u64(..)))],
                finish_reason: Some("tool_calls".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

/// Kill the one running instance once the LLM is mid-turn, then let the
/// in-flight call finish so the loop reaches its cancellation checkpoint.
async fn kill_the_running_instance(
    manager: &Arc<SubagentManager>,
    entered: &tokio::sync::Notify,
    release: &tokio::sync::Notify,
) {
    entered.notified().await;
    let list = manager.list().await;
    assert_eq!(list.len(), 1, "exactly one subagent is running");
    assert!(manager.kill(&list[0].id).await.unwrap());
    release.notify_one();
}

fn assert_stopped_with_partial_output(content: &str) {
    assert!(content.contains("status: failed"), "got: {content}");
    assert!(
        content.contains(SUBAGENT_STOPPED_MESSAGE),
        "the v2 stopped message leads: {content}"
    );
    assert!(
        content.contains("[partial output]"),
        "the partial output is marked: {content}"
    );
    assert!(
        content.contains("partial findings so far"),
        "the partial output rides below: {content}"
    );
}

#[tokio::test]
async fn stopped_subagent_returns_partial_output() {
    let (llm, entered, release) = PartialThenBlockLlm::new(false);
    let manager = manager_with(llm).await;
    register_looper(&manager).await;
    let mgr = manager.clone();
    let run = tokio::spawn(async move {
        execute_agent(
            &mgr,
            None,
            &serde_json::json!({ "subagent_type": "looper", "prompt": "dig" }),
            Some(60_000),
            None,
            None,
            None,
        )
        .await
    });
    kill_the_running_instance(&manager, &entered, &release).await;
    let result = run
        .await
        .unwrap()
        .expect("a stop is a native outcome, not a fallback");
    assert!(result.is_error);
    assert_stopped_with_partial_output(&result.content);
}

#[tokio::test]
async fn stopped_resume_returns_partial_output() {
    let (llm, entered, release) = PartialThenBlockLlm::new(true);
    let manager = manager_with(llm).await;
    register_looper(&manager).await;
    // Owned by main: v2's resume guard refuses another parent's agent.
    let agent_id = as_main_agent(manager.spawn("looper", "resumable"))
        .await
        .unwrap();
    // Seed the conversation the resume continues (the first LLM call's
    // partial text becomes the history's last assistant message).
    let seeded = manager
        .run_foreground_turn(&agent_id, "start", None)
        .await
        .unwrap();
    assert!(matches!(seeded, ForegroundTurnOutcome::Completed(_)));
    let mgr = manager.clone();
    let resume_id = agent_id.clone();
    let run = tokio::spawn(async move {
        as_main_agent(execute_agent(
            &mgr,
            None,
            &serde_json::json!({ "resume": resume_id, "prompt": "continue" }),
            Some(60_000),
            None,
            None,
            None,
        ))
        .await
    });
    kill_the_running_instance(&manager, &entered, &release).await;
    let result = run
        .await
        .unwrap()
        .expect("a stopped resume is a native outcome, not a fallback");
    assert!(
        result.is_error,
        "a stopped resume must not report completed: {}",
        result.content
    );
    assert_stopped_with_partial_output(&result.content);
}

/// v2 `formatForegroundAgentSuccess` / `formatForegroundAgentFailure`: the
/// handoff the parent reads names the stop reason, carries the resume hint, and
/// — for a repeat-breaker handoff — the notice that the summary is not a
/// finished result. A failure names its reason too, prints the partial-output
/// placeholder when there is nothing to show, and skips the resume hint on a
/// cancelled turn. The fork's report had none of those lines.
#[test]
fn foreground_reports_match_the_v2_shape() {
    use super::super::format::{
        NO_PARTIAL_OUTPUT, REPEAT_BREAKER_NOTICE, format_failure, format_success,
    };

    let success = format_success("agent-child", "coder", "child result", "completed");
    assert!(success.contains("status: completed"), "{success}");
    assert!(success.contains("stop_reason: completed"), "{success}");
    assert!(success.contains("[summary]\nchild result"), "{success}");
    assert!(
        success
            .contains("resume_hint: Continue with Agent(resume=\"agent-child\", prompt=\"...\")"),
        "{success}"
    );
    assert!(!success.contains("notice:"), "{success}");
    assert!(!success.contains("next_step:"), "{success}");

    let breaker = format_success("agent-child", "coder", "handoff", "repeat_breaker");
    assert!(breaker.contains("stop_reason: repeat_breaker"), "{breaker}");
    assert!(breaker.contains(REPEAT_BREAKER_NOTICE), "{breaker}");
    assert!(
        breaker.contains("next_step: The subagent was stuck on one tool call."),
        "{breaker}"
    );

    let failure = format_failure("agent-child", "coder", "boom", "no_final_message", None);
    assert!(failure.contains("status: failed"), "{failure}");
    assert!(
        failure.contains("stop_reason: no_final_message"),
        "{failure}"
    );
    assert!(failure.contains(NO_PARTIAL_OUTPUT), "{failure}");
    assert!(
        failure.contains(
            "resume_hint: Continue with Agent(resume=\"agent-child\", prompt=\"continue\")"
        ),
        "{failure}"
    );
    assert!(
        failure.contains("next_step: Resume to continue where it stopped"),
        "{failure}"
    );

    let cancelled = format_failure(
        "agent-child",
        "coder",
        "stopped",
        "cancelled",
        Some("[partial output] half a thought"),
    );
    assert!(
        cancelled.contains("[partial output] half a thought"),
        "{cancelled}"
    );
    assert!(
        !cancelled.contains("resume_hint:"),
        "a cancelled turn is not resumable next: {cancelled}"
    );
    assert!(
        cancelled.contains("next_step: The user stopped this subagent."),
        "{cancelled}"
    );
}

/// v2 throws a typed error per failure, and the failure shape prints *that*
/// error's message: the step cap names the budget it spent (and carries no
/// config advice, unlike the main-turn message), a filtered response keeps the
/// provider sentence, a token cap names its reason, and anything longer than
/// 2,000 UTF-16 code units is cut with v2's `… [truncated]` marker.
#[test]
fn failure_messages_carry_the_v2_reason_text() {
    use super::super::format::{failure_message, truncate_reason};
    use crate::turn_loop::types::LoopTurnStopReason as Stop;

    let steps = failure_message(&Stop::MaxSteps, 5, false);
    assert!(steps.contains("maxSteps=5"), "{steps}");
    assert!(
        !steps.contains("config.toml"),
        "the subagent step-cap message carries no config advice: {steps}"
    );

    let filtered = failure_message(&Stop::Filtered, 1, false);
    assert_eq!(filtered, "Provider safety policy blocked the response.");

    let tokens = failure_message(&Stop::MaxTokens, 1, false);
    assert!(tokens.contains("reason=max_tokens"), "{tokens}");

    let interrupted = failure_message(&Stop::Aborted, 1, true);
    assert_eq!(
        interrupted,
        "The subagent was stopped before it finished by user."
    );

    let long = "x".repeat(5000);
    let truncated = truncate_reason(&long);
    assert!(truncated.ends_with("… [truncated]"), "{truncated}");
    assert_eq!(
        truncated.encode_utf16().count(),
        2000 + "… [truncated]".encode_utf16().count(),
        "the cut is 2,000 UTF-16 units plus the marker"
    );
    assert_eq!(truncate_reason("short"), "short");
}
