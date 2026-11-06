use super::super::*;
use super::common::*;
use super::*;

// ── Goal steering text injection test ────────────────────────────────

/// Verify that when a goal is active, the system prompt is enriched
/// with steering text containing the objective and budget info.
#[tokio::test]
async fn test_goal_steering_injected_into_system_prompt() {
    use std::sync::Mutex;

    // LLM that captures the messages it receives so we can inspect
    // the system prompt.
    struct CaptureLlm {
        captured_system: Arc<Mutex<Option<String>>>,
        call: AtomicU32,
    }
    impl LLM for CaptureLlm {
        fn system_prompt(&self) -> &str {
            "base prompt"
        }
        fn model_name(&self) -> &str {
            "capture"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let captured = self.captured_system.clone();
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 && !params.messages.is_empty() {
                    *captured.lock().unwrap() = Some(params.messages[0].content.clone());
                }
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 3,
                        total_tokens: 8,
                        ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let llm = CaptureLlm {
        captured_system: captured.clone(),
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let goal = GoalContext {
        goal_id: "g-steer".into(),
        objective: "Write a hello world program".into(),
        status: GoalStatus::Active,
        token_budget: Some(1000),
        turn_budget: Some(10),
        tokens_used: 100,
        turns_used: 1,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-steering".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("system prompt was captured");
    assert!(
        captured.contains("base prompt"),
        "should contain base system prompt"
    );
    assert!(
        captured.contains("Write a hello world program"),
        "should contain objective"
    );
    assert!(captured.contains("Goal"), "should contain Goal header");
    assert!(captured.contains("Budgets:"), "should contain budget info");
    assert!(captured.contains("1000"), "should mention token budget");
    assert!(captured.contains("10"), "should mention turn budget");
}

// ── Max steps enforcement test ──────────────────────────────────────

/// When the LLM always returns tool calls, the loop must stop at
/// max_steps with EndTurn (the loop exits after the max_steps
/// iterations).
#[tokio::test]
async fn test_max_steps_enforcement() {
    // LLM that always returns a tool call — never stops on its own.
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "loop".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "/x"}),
            extras: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "ok".into(),
                is_error: false,
                note: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-max-steps".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "loop".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 3,
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

    let result = run_turn(input, &callbacks).await.unwrap();
    // The loop runs max_steps iterations; steps = max_steps.
    assert_eq!(result.steps, 3, "should stop at max_steps");
}

// ── render_goal_steering tests ──────────────────────────────────────

#[test]
fn test_render_goal_steering_basic() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Write tests".into(),
        status: GoalStatus::Active,
        token_budget: Some(1000),
        turn_budget: Some(10),
        tokens_used: 100,
        turns_used: 1,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };
    let text = render_goal_steering(&goal, 50, 1, 0);
    assert!(text.contains("Write tests"), "should contain objective");
    assert!(text.contains("Goal"), "should contain Goal header");
    assert!(text.contains("Budgets:"), "should contain budget section");
    assert!(text.contains("1000"), "should mention token budget");
    assert!(
        text.contains("within budget"),
        "should say within budget when low"
    );
}

#[test]
fn test_render_goal_steering_wall_clock_budget() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Finish".into(),
        status: GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        wall_clock_budget_ms: Some(10_000),
        wall_clock_ms: 5000,
        tokens_used: 0,
        turns_used: 0,
        updated_at: 0,
        terminal_reason: None,
    };
    let text = render_goal_steering(&goal, 0, 0, 500);
    assert!(
        text.contains("time 5s/10s (remaining 4s)"),
        "should show wall-clock budget"
    );
    assert!(
        text.contains("elapsed"),
        "should mention elapsed time in progress"
    );
}

#[test]
fn test_format_elapsed() {
    assert_eq!(format_elapsed(500), "0s");
    assert_eq!(format_elapsed(59_000), "59s");
    assert_eq!(format_elapsed(65_000), "1m05s");
    assert_eq!(format_elapsed(3_600_000), "1h0m");
    assert_eq!(format_elapsed(7_200_000), "2h0m");
}

#[test]
fn test_render_goal_steering_near_limit() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Finish".into(),
        status: GoalStatus::Active,
        token_budget: Some(100),
        turn_budget: None,
        tokens_used: 80,
        turns_used: 0,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };
    // 80 + 10 = 90 / 100 = 0.9 >= 0.75 → should say "nearing"
    let text = render_goal_steering(&goal, 10, 0, 0);
    assert!(
        text.contains("nearing a budget"),
        "should warn about nearing budget"
    );
}

#[test]
fn test_render_goal_steering_no_budgets() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Do thing".into(),
        status: GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        tokens_used: 0,
        turns_used: 0,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };
    let text = render_goal_steering(&goal, 0, 0, 0);
    assert!(text.contains("Do thing"), "should contain objective");
    assert!(
        !text.contains("Budgets:"),
        "should not contain budget section when no budgets"
    );
}

// ── Context compaction wiring tests ────────────────────────────────

/// When the message history crosses the context trigger threshold, the
/// loop compacts it before the LLM call: the system prompt and the
/// most recent messages survive, the middle is replaced by a summary
/// placeholder.
#[tokio::test]
async fn test_context_compaction_before_llm_call() {
    use std::sync::Mutex;

    struct CaptureLlm {
        captured: Arc<Mutex<Vec<LLMMessage>>>,
    }
    impl LLM for CaptureLlm {
        fn system_prompt(&self) -> &str {
            "base prompt"
        }
        fn model_name(&self) -> &str {
            "capture"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let captured = self.captured.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = params.messages.to_vec();
                Ok(LLMChatResponse {
                    content: if params.messages[0]
                        .content
                        .contains("conversation summarizer")
                    {
                        "Earlier user and assistant discussed the task.".into()
                    } else {
                        String::new()
                    },
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 2,
                        ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    // Default trigger threshold is 128k - 50k reserved = 81_072 tokens;
    // at 4 chars/token that is ~324k chars. Three 120k-char messages
    // (90k estimated tokens) cross it.
    let big = "x".repeat(120_000);
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));

    let captured = captured.lock().unwrap();
    // v2 compaction shape: system, the compacted range's head user input
    // (verbatim), the elision note, the prefixed summary, the continuation
    // note, the preserved recent user input, then the turn injections.
    assert_eq!(captured[0].role, "system");
    let summary_message = captured
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the prefixed summary must be present");
    assert!(
        summary_message
            .content
            .ends_with("Earlier user and assistant discussed the task."),
        "the generated summary must ride the compaction prefix"
    );
    assert!(
        captured.iter().any(|m| {
            m.content.contains("roughly") && m.content.contains("tokens in between were dropped")
        }),
        "the elision note must mark the omitted middle"
    );
    let continuation_count = captured
        .iter()
        .filter(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_CONTINUATION_TEXT)
        })
        .count();
    assert_eq!(
        continuation_count, 1,
        "exactly one continuation note, carried by the compacted shape itself"
    );
    assert!(
        captured.iter().any(|m| m.content == big),
        "most recent message preserved"
    );
    assert!(
        captured
            .iter()
            .any(|m| m.content.starts_with("<system-reminder>\n")
                && !m.content.contains("roughly")
                && !m.content.contains("compaction")),
        "injection appended after compaction"
    );
}

/// v2 `postProcessSummary` (fullCompactionService.ts:844): a compaction
/// in a session with todos appends the rendered list to the written
/// summary, so the todo state survives the fold that dropped the older
/// history. The default `host/state_read` answers Null for every domain;
/// the todo domain gets a real list here.
#[tokio::test]
async fn test_auto_compaction_summary_carries_the_todo_list() {
    use std::sync::Mutex;

    struct CaptureLlm {
        captured: Arc<Mutex<Vec<LLMMessage>>>,
    }
    impl LLM for CaptureLlm {
        fn system_prompt(&self) -> &str {
            "base prompt"
        }
        fn model_name(&self) -> &str {
            "capture"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let captured = self.captured.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = params.messages.to_vec();
                Ok(LLMChatResponse {
                    content: if params.messages[0]
                        .content
                        .contains("conversation summarizer")
                    {
                        "Earlier user and assistant discussed the task.".into()
                    } else {
                        String::new()
                    },
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        total_tokens: 2,
                        ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let big = "x".repeat(120_000);
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    // Overwrite the no-op state_read (a HashMap insert, so this wins) and
    // answer only the todo domain — goal/plan keep the Null the step-head
    // refresh expects.
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, |params| {
        Box::pin(async move {
            let value = if params.get("domain").and_then(|v| v.as_str()) == Some("todo") {
                serde_json::json!([
                    { "id": "t1", "title": "ship the fix", "status": "in_progress" }
                ])
            } else {
                serde_json::Value::Null
            };
            let resp = types::StateReadResponse { value };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-todos".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));

    let captured = captured.lock().unwrap();
    let summary_message = captured
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the prefixed summary must be present");
    assert!(
        summary_message
            .content
            .contains("Earlier user and assistant discussed the task."),
        "the generated summary rides the compaction prefix"
    );
    assert!(
        summary_message.content.contains("## TODO List")
            && summary_message.content.contains("ship the fix"),
        "the summary carries the todo list (v2 postProcessSummary)"
    );
}

// ── Injection wiring tests ─────────────────────────────────────────
