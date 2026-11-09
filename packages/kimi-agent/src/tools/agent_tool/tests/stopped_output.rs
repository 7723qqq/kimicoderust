use super::super::*;
use super::common::*;
use super::*;

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

async fn kill_the_running_instance(
    manager: &Arc<SubagentManager>,
    entered: &tokio::sync::Notify,
    release: &tokio::sync::Notify,
) {
    entered.notified().await;
    let list = manager.list().await;
    assert_eq!(list.len(), 1, "exactly one subagent is running");

    assert!(manager.kill_now(&list[0].id).await.unwrap());
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

    let agent_id = as_main_agent(manager.spawn("looper", "resumable"))
        .await
        .unwrap();

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
