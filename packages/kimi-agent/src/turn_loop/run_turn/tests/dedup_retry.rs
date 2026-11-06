use super::super::*;
use super::common::*;
use super::*;

// ── Tool-call dedup (v2 `toolDedupeService` mirror, G-6 #2) ─────────

/// A same-step repeat of an identical call never executes: the host
/// runs the original once, and the repeat receives the same result.
#[tokio::test]
async fn test_same_step_duplicate_executes_once() {
    struct DupLlm {
        call: AtomicU32,
    }
    impl LLM for DupLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "dup-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![
                            ToolCall {
                                id: "tc1".into(),
                                name: "read".into(),
                                arguments: serde_json::json!({"path": "/a.txt"}),
                                extras: None,
                            },
                            // Same key, different call id — the repeat.
                            ToolCall {
                                id: "tc2".into(),
                                name: "read".into(),
                                arguments: serde_json::json!({"path": "/a.txt"}),
                                extras: None,
                            },
                        ],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                }
            })
        }
    }
    let llm = DupLlm {
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let executions = Arc::new(AtomicU32::new(0));
    let counter = executions.clone();
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |_params| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "stub".into(),
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
        turn_id: "test-same-step-dedup".into(),
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
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };
    let result = run_turn(input, &callbacks).await.unwrap();

    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the repeat must share the original's execution"
    );
    // Both calls keep their result messages, with identical content.
    let tool_msgs: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 2);
    assert_eq!(tool_msgs[0].tool_call_id.as_deref(), Some("tc1"));
    assert_eq!(tool_msgs[1].tool_call_id.as_deref(), Some("tc2"));
    assert_eq!(tool_msgs[0].content, "stub");
    assert_eq!(tool_msgs[1].content, "stub");
}

/// Calls with non-native names are exempt from engine-side dedup: they
/// run on the host, whose own dedup service stays authoritative.
#[tokio::test]
async fn test_same_step_duplicate_of_host_tool_still_executes_twice() {
    struct HostDupLlm {
        call: AtomicU32,
    }
    impl LLM for HostDupLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "host-dup-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![
                            ToolCall {
                                id: "tc1".into(),
                                name: "echo".into(),
                                arguments: serde_json::json!({"text": "hello"}),
                                extras: None,
                            },
                            ToolCall {
                                id: "tc2".into(),
                                name: "echo".into(),
                                arguments: serde_json::json!({"text": "hello"}),
                                extras: None,
                            },
                        ],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                }
            })
        }
    }
    let llm = HostDupLlm {
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let executions = Arc::new(AtomicU32::new(0));
    let counter = executions.clone();
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |_params| {
        counter.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "stub".into(),
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
        turn_id: "test-host-tool-no-dedup".into(),
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
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };
    run_turn(input, &callbacks).await.unwrap();

    assert_eq!(
        executions.load(Ordering::SeqCst),
        2,
        "non-native names bypass engine dedup (the host dedups them)"
    );
}

/// Cross-step repeats earn the escalating reminders, and the 12th
/// consecutive identical call force-stops the turn as `completed`.
#[tokio::test]
async fn test_repeat_streak_appends_reminders_and_force_stops() {
    struct RepeatLlm {
        call: AtomicU32,
    }
    impl LLM for RepeatLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "repeat-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: format!("tc{call}"),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "/a.txt"}),
                        extras: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }
    let llm = RepeatLlm {
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "stub".into(),
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
        turn_id: "test-repeat-streak".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        // Generous ceiling: the force stop, not max_steps, must end it.
        max_steps: 20,
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

    assert_eq!(
        result.steps, 13,
        "force stop at the 12th repeat + 1 handoff step"
    );
    assert!(matches!(
        result.stop_reason,
        LoopTurnStopReason::RepeatBreaker
    ));
    assert!(result.messages.iter().any(|m| {
        m.content
            .contains("The repeat breaker has stopped the turn")
    }));
    let tool_msgs: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 12);
    // Streak 1 and 2 stay untouched; 3 earns reminder 1.
    assert_eq!(tool_msgs[0].content, "stub");
    assert_eq!(tool_msgs[1].content, "stub");
    assert!(
        tool_msgs[2]
            .content
            .contains("repeated several times in a row")
    );
    // Streak 5 embeds the count (reminder 2).
    assert!(tool_msgs[4].content.contains("issued 5 times in a row"));
    // Streak 8 and 12 carry the final-response reminder; 12 stops the turn.
    assert!(
        tool_msgs[7]
            .content
            .contains("Write your final response now")
    );
    assert!(
        tool_msgs[11]
            .content
            .contains("Write your final response now")
    );
}

// ── Turn telemetry counters ─────────────────────────────────────────

/// The turn result carries the LLM retry count: the transient failure
/// on the first chat attempt counts as one retry.
#[tokio::test]
async fn test_turn_result_llm_retry_counter() {
    struct RetryOnceLlm {
        calls: AtomicU32,
    }
    impl LLM for RetryOnceLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "retry-once"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            true
        }

        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    return Err("transient failure".into());
                }
                Ok(LLMChatResponse {
                    content: "done".into(),
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

    let llm = RetryOnceLlm {
        calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-retry-counter".into(),
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
    assert_eq!(result.llm_retries, 1, "one retryable failure is one retry");
    assert_eq!(
        result.events_emitted, 0,
        "event counting lives at the composition root"
    );
}

/// `RunTurnInput::max_attempts` must cap the retry loop (v2
/// `loopControl.maxAttemptsPerStep`, stepRetryService.ts:138-146): a
/// permanently failing LLM is called exactly `max_attempts` times before
/// the turn errors out. The engine default (no host override) is 10 —
/// `DEFAULT_MAX_RETRY_ATTEMPTS`, pinned by the retry.rs unit test.
#[tokio::test]
async fn test_max_attempts_caps_llm_retries() {
    struct AlwaysFailsLlm {
        calls: AtomicU32,
    }
    impl LLM for AlwaysFailsLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "always-fails"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            true
        }
        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Err("transient failure".into()) })
        }
    }

    let llm = AlwaysFailsLlm {
        calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        max_attempts: Some(2),
        turn_id: "test-max-attempts".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
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
    assert!(result.is_err(), "an always-failing LLM must fail the turn");
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        2,
        "the LLM must be attempted exactly max_attempts times"
    );
}
