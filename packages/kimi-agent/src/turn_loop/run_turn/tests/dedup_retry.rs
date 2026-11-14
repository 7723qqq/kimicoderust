use super::super::*;
use super::common::*;
use super::*;

struct ArmedRepeatBreaker {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    previous: Option<String>,
}

impl ArmedRepeatBreaker {
    async fn new() -> Self {
        use crate::tools::tool_dedupe::REPEAT_BREAKER_ENV;
        let lock = crate::env::ENV_TEST_LOCK.lock().await;
        let previous = std::env::var(REPEAT_BREAKER_ENV).ok();
        unsafe { std::env::set_var(REPEAT_BREAKER_ENV, "1") };
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for ArmedRepeatBreaker {
    fn drop(&mut self) {
        use crate::tools::tool_dedupe::REPEAT_BREAKER_ENV;
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(REPEAT_BREAKER_ENV, value) },
            None => unsafe { std::env::remove_var(REPEAT_BREAKER_ENV) },
        }
    }
}

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
                                args_parse_error: None,
                            },
                            ToolCall {
                                id: "tc2".into(),
                                name: "read".into(),
                                arguments: serde_json::json!({"path": "/a.txt"}),
                                extras: None,
                                args_parse_error: None,
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
    let (capturing, events) = EventCapturingCallbacks::new(callbacks);
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);
    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
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
    let events = events.lock().unwrap();
    let duplicate_native = events
        .iter()
        .position(|event| event["type"] == "tool.native" && event["tool_call_id"] == "tc2")
        .expect("the deduplicated call must still report its result");
    let duplicate_call = events
        .iter()
        .position(|event| event["type"] == "tool.call.started" && event["tool_call_id"] == "tc2")
        .expect("the deduplicated call must be announced before its result");
    assert!(
        duplicate_call < duplicate_native,
        "tool.call must precede tool.native for the deduplicated call"
    );
    assert_eq!(events[duplicate_call]["args"]["path"], "/a.txt");
}

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
                                args_parse_error: None,
                            },
                            ToolCall {
                                id: "tc2".into(),
                                name: "echo".into(),
                                arguments: serde_json::json!({"text": "hello"}),
                                extras: None,
                                args_parse_error: None,
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
        llm_request_byte_budget: u64::MAX,
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
                        args_parse_error: None,
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
    let (capturing, events) = EventCapturingCallbacks::new(callbacks);
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);
    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
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

    let _breaker = ArmedRepeatBreaker::new().await;
    let result = run_turn(input, &callbacks).await.unwrap();

    assert_eq!(
        result.steps, 13,
        "force stop at the 12th repeat + 1 handoff step"
    );
    assert!(matches!(
        result.stop_reason,
        LoopTurnStopReason::RepeatBreaker
    ));

    let tool_msgs: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(
        tool_msgs.len(),
        13,
        "12 executed calls + the vetoed handoff call"
    );
    assert!(
        tool_msgs[12]
            .content
            .contains("This turn was ended by the repeat breaker"),
        "the handoff call must be answered with the veto text: {}",
        tool_msgs[12].content
    );
    assert!(tool_msgs[12].content.contains("was not executed"));
    let handoff_assistant = result
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
        .expect("the vetoed call keeps its assistant message");
    assert_eq!(handoff_assistant.tool_calls[0].id, "tc12");

    assert_eq!(tool_msgs[0].content, "stub");
    assert_eq!(tool_msgs[1].content, "stub");
    assert!(
        tool_msgs[2]
            .content
            .contains("repeated several times in a row")
    );

    assert!(tool_msgs[4].content.contains("issued 5 times in a row"));

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
    let events = events.lock().unwrap();
    let native_index = events
        .iter()
        .position(|event| event["type"] == "tool.native" && event["tool_call_id"] == "tc12")
        .expect("the vetoed handoff call must still report its result");
    let call_index = events
        .iter()
        .position(|event| event["type"] == "tool.call.started" && event["tool_call_id"] == "tc12")
        .expect("the vetoed handoff call must be announced before its result");
    assert!(
        call_index < native_index,
        "tool.call must precede tool.native for the vetoed call"
    );
    assert_eq!(events[call_index]["args"]["path"], "/a.txt");
}

#[tokio::test]
async fn test_changing_malformed_payloads_do_not_force_stop() {
    struct BrokenLlm {
        call: AtomicU32,
    }
    impl LLM for BrokenLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "broken-json-llm"
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
                if call >= 12 {
                    return Ok(LLMChatResponse {
                        content: "recovered".into(),
                        thinking: vec![],
                        tool_calls: Vec::new(),
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    });
                }
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: format!("bad{call}"),
                        name: "Bash".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                        args_parse_error: Some(crate::turn_loop::types::ArgsParseError {
                            raw: format!(r#"{{"command":"ls {call}"#),
                            error: "EOF while parsing an object".into(),
                        }),
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }
    let llm = BrokenLlm {
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-changing-malformed".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
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

    let _breaker = ArmedRepeatBreaker::new().await;
    let result = run_turn(input, &callbacks).await.unwrap();

    assert_eq!(
        result.steps, 13,
        "12 rejected calls + the step that recovers"
    );
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let tool_msgs: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 12);
    for (index, message) in tool_msgs.iter().enumerate() {
        assert!(
            !message.content.contains("repeated several times in a row"),
            "call {index} must not earn a reminder: {}",
            message.content
        );
        assert!(
            !message.content.contains("Write your final response now"),
            "call {index} must not carry the force-stop reminder: {}",
            message.content
        );
    }
}

struct SameBrokenCallLlm {
    raw: &'static str,
    stop_after: u32,
    call: AtomicU32,
}

impl LLM for SameBrokenCallLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "broken-repeat-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        let raw = self.raw;
        let done = call >= self.stop_after;
        Box::pin(async move {
            if done {
                return Ok(LLMChatResponse {
                    content: "handoff".into(),
                    thinking: vec![],
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                });
            }
            Ok(LLMChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls: vec![ToolCall {
                    id: format!("bad{call}"),
                    name: "Bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: Some(crate::turn_loop::types::ArgsParseError {
                        raw: raw.to_string(),
                        error: "EOF while parsing an object".into(),
                    }),
                }],
                finish_reason: Some("tool_calls".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

fn broken_turn_input<'a>(llm: &'a SameBrokenCallLlm, max_steps: u32) -> RunTurnInput<'a> {
    RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-rejected-repeat".into(),
        llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps,
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
    }
}

#[tokio::test]
async fn test_repeated_rejected_calls_force_stop_the_turn() {
    let llm = SameBrokenCallLlm {
        raw: r#"{"command":"#,
        stop_after: 40,
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let _breaker = ArmedRepeatBreaker::new().await;
    let result = run_turn(broken_turn_input(&llm, 20), &callbacks)
        .await
        .unwrap();

    assert_eq!(result.steps, 13, "12 rejected calls + the handoff step");
    assert_eq!(result.stop_reason, LoopTurnStopReason::RepeatBreaker);
    let tool_msgs: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(
        tool_msgs.len(),
        13,
        "12 rejected calls + the call the handoff step issued"
    );

    for message in &tool_msgs[..12] {
        assert!(
            message.content.contains("Invalid Bash arguments"),
            "{}",
            message.content
        );
    }

    assert!(
        tool_msgs[12]
            .content
            .contains("This turn was ended by the repeat breaker"),
        "the handoff call is answered with the veto text: {}",
        tool_msgs[12].content
    );
    assert!(
        tool_msgs[2]
            .content
            .contains("repeated several times in a row"),
        "streak 3 earns reminder 1: {}",
        tool_msgs[2].content
    );
    assert!(
        tool_msgs[11]
            .content
            .contains("Write your final response now"),
        "streak 12 carries the final-response reminder: {}",
        tool_msgs[11].content
    );
}

#[tokio::test]
async fn test_handoff_runs_when_the_force_stop_lands_on_the_step_cap() {
    let llm = SameBrokenCallLlm {
        raw: r#"{"command":"#,
        stop_after: 40,
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let _breaker = ArmedRepeatBreaker::new().await;
    let result = run_turn(broken_turn_input(&llm, 12), &callbacks)
        .await
        .unwrap();

    assert_eq!(result.steps, 13, "the cap does not swallow the handoff");
    assert_eq!(result.stop_reason, LoopTurnStopReason::RepeatBreaker);
}

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
    let (capturing, events) = EventCapturingCallbacks::new(callbacks);
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
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
    let captured = events.lock().unwrap();
    let retrying: Vec<&serde_json::Value> = captured
        .iter()
        .filter(|event| event.get("type").and_then(|v| v.as_str()) == Some("llm.step.retrying"))
        .collect();
    assert_eq!(retrying.len(), 1, "one retry is one retrying event");
    assert_eq!(retrying[0]["turn_id"], "test-retry-counter");
    assert_eq!(retrying[0]["failed_attempt"], 1);
    assert_eq!(retrying[0]["next_attempt"], 2);
    assert!(retrying[0]["step"].is_number());
    assert!(
        retrying[0]["step_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("test-retry-counter-step-")),
        "the event closes the step it belongs to: {}",
        retrying[0]
    );
    assert!(
        retrying[0]["error_name"].is_string(),
        "the failure itself rides along: {}",
        retrying[0]
    );
}

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
        llm_request_byte_budget: u64::MAX,
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
