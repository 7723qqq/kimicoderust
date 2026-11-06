use super::super::*;
use super::common::*;
use super::*;

/// Callbacks wrapper recording `set_turn_goal` bindings.
struct GoalBindingCallbacks {
    inner: Arc<dyn HostCallbacks>,
    #[allow(clippy::type_complexity)]
    bound: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

impl HostCallbacks for GoalBindingCallbacks {
    fn llm_chat(
        &self,
        request: crate::rpc::types::LlmChatRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: crate::rpc::types::ToolExecuteRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::ToolExecuteResponse, String>>
    {
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        self.inner.check_permission(request)
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.bound
            .lock()
            .unwrap()
            .push((turn_id.to_string(), goal_id.map(str::to_string)));
    }
}

#[tokio::test]
async fn test_turn_binds_start_goal_for_stale_check() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let base = rpc_callbacks(server.clone());
    let bound = Arc::new(Mutex::new(Vec::new()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(GoalBindingCallbacks {
        inner: base.clone(),
        bound: bound.clone(),
    });

    let goal = crate::turn_loop::types::GoalContext {
        goal_id: "g-1".into(),
        objective: "objective".into(),
        status: crate::turn_loop::types::GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        wall_clock_budget_ms: None,
        tokens_used: 0,
        turns_used: 0,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-goal".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: Some(goal),
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(result.is_ok());
    assert_eq!(
        bound.lock().unwrap().as_slice(),
        &[("turn-goal".to_string(), Some("g-1".to_string()))],
        "run_turn must bind the turn-start goal for the stale gate"
    );
}

#[tokio::test]
async fn test_turn_binds_no_goal_when_goalless() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let base = rpc_callbacks(server.clone());
    let bound = Arc::new(Mutex::new(Vec::new()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(GoalBindingCallbacks {
        inner: base.clone(),
        bound: bound.clone(),
    });
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-goal-free".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(result.is_ok());
    assert_eq!(
        bound.lock().unwrap().as_slice(),
        &[("turn-goal-free".to_string(), None)],
        "a goal-less turn binds None so the stale gate skips it"
    );
}

// ── empty provider responses, end to end (v2 empty_response) ───────────

/// A native-transport LLM that answers the first `empty_answers` calls
/// with a 200 carrying no content, then a real answer.
///
/// `transport()` is `native-http` on purpose. The empty-response guard arms
/// only for the transport that actually assembles the assistant message
/// (see `turn_step::empty_response_retryable`) — the host-proxy leg owns its
/// own transcript, where an empty body is a legitimate answer — so a stub
/// left on the default `"custom"` would bypass the very thing under test and
/// make this test pass for the wrong reason.
struct EmptyThenAnswerLlm {
    empty_answers: u32,
    calls: AtomicU32,
    answer: String,
}

impl LLM for EmptyThenAnswerLlm {
    fn system_prompt(&self) -> &str {
        "You are helpful."
    }
    fn model_name(&self) -> &str {
        "empty-then"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn transport(&self) -> &'static str {
        "native-http"
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let empty_answers = self.empty_answers;
        let answer = self.answer.clone();
        Box::pin(async move {
            let content = if call < empty_answers {
                String::new()
            } else {
                answer
            };
            Ok(LLMChatResponse {
                content,
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
                timing: None,
            })
        })
    }
}

/// A provider that behaves the way a real one does about a duplicated
/// `tool_call_id`: it reads the request, and rejects its *shape* with a 400
/// naming the problem. Only a request whose duplicates are gone is
/// accepted. This is the only way to prove the strict retry is reachable
/// from the real loop rather than merely callable.
struct RejectsDuplicateToolCallsLlm {
    calls: AtomicU32,
    /// Tool-call ids seen per request, recorded so the test can assert the
    /// second request was actually clean rather than accidentally accepted.
    seen_per_call: std::sync::Mutex<Vec<Vec<String>>>,
}

impl RejectsDuplicateToolCallsLlm {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            seen_per_call: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl LLM for RejectsDuplicateToolCallsLlm {
    fn system_prompt(&self) -> &str {
        "You are helpful."
    }
    fn model_name(&self) -> &str {
        "strict-shape"
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
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut seen: Vec<String> = Vec::new();
        let mut duplicate: Option<String> = None;
        for message in params.messages.iter() {
            for tool_call in &message.tool_calls {
                if seen.contains(&tool_call.id) {
                    duplicate = Some(tool_call.id.clone());
                }
                seen.push(tool_call.id.clone());
            }
        }
        self.seen_per_call.lock().unwrap().push(seen);
        Box::pin(async move {
            // The verdict is a function of the request alone. A mock that
            // also failed "the first time" would pass this test whether or
            // not the retry actually removed the duplicate.
            if let Some(id) = duplicate {
                return Err(Box::new(std::io::Error::other(format!(
                    "llm http status 400: messages: tool_use ids must be unique (duplicate {id})"
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(LLMChatResponse {
                content: "recovered".to_string(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
                timing: None,
            })
        })
    }
}

/// End-to-end through the real loop: a history carrying a duplicated
/// `tool_call_id` is rejected by the provider, and the turn recovers on the
/// strict resend instead of failing the user's request. Asserts the second
/// request really was clean — otherwise this would pass on a mock that
/// simply stopped complaining.
#[tokio::test]
async fn a_turn_resends_with_the_strict_projection_after_a_shape_rejection() {
    let llm = RejectsDuplicateToolCallsLlm::new();
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-strict".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "Run it twice.".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                }],
                ..Default::default()
            },
            LLMMessage {
                role: "tool".into(),
                content: "done".into(),
                tool_call_id: Some("call_1".into()),
                ..Default::default()
            },
            // The replayed step: the same call declared a second time.
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                }],
                ..Default::default()
            },
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks)
        .await
        .expect("the strict resend must recover the turn");

    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        2,
        "one rejection, one resend"
    );
    let seen = llm.seen_per_call.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].iter().filter(|id| *id == "call_1").count(),
        2,
        "the first request carried the duplicate"
    );
    assert_eq!(
        seen[1].iter().filter(|id| *id == "call_1").count(),
        1,
        "the resend carried exactly one declaration"
    );
    assert!(
        result
            .messages
            .iter()
            .rev()
            .any(|m| m.role == "assistant" && m.content == "recovered"),
        "the turn ends on the recovered answer"
    );
}

/// rather than the retry helper in isolation: a provider that answers 200
/// with nothing used to end the turn as a success, so the model stopped
/// early and nothing — transcript, telemetry, UI — could tell it from a
/// real reply.
#[tokio::test]
async fn a_turn_retries_through_empty_provider_responses_and_answers() {
    let llm = EmptyThenAnswerLlm {
        empty_answers: 2,
        calls: AtomicU32::new(0),
        answer: "the real answer".into(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-empty".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks)
        .await
        .expect("the turn recovers from the empty responses");

    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        3,
        "two empty answers were retried, the third carried content"
    );
    assert_eq!(
        result.llm_retries, 2,
        "the turn must record the retries, not silently absorb them"
    );
    assert_eq!(result.steps, 1, "recovery costs attempts, not steps");
    let final_assistant = result
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .expect("the turn ends on an assistant message");
    assert_eq!(
        final_assistant.content, "the real answer",
        "the recovered answer is what the model reads"
    );
}

/// The other half: a provider that never answers must **fail** rather than
/// book an empty turn as a success. Without this, the retry could mask a
/// hard failure as a very slow success.
#[tokio::test]
async fn a_turn_fails_when_every_response_is_empty() {
    let llm = EmptyThenAnswerLlm {
        empty_answers: u32::MAX,
        calls: AtomicU32::new(0),
        answer: "never used".into(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-empty-forever".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: Some(2),
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let err = run_turn(input, &callbacks)
        .await
        .expect_err("a provider that never answers must not read as success");
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        2,
        "the attempt budget is spent, and no more"
    );
    assert!(
        err.to_string().contains("empty response"),
        "the failure must name the cause: {err}"
    );
}
