use super::super::*;
use super::common::*;
use super::*;

#[tokio::test]
async fn test_run_turn_no_tool_calls_with_messages() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-3".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "First message".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "Second message".into(),
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

    let result = run_turn(input, &callbacks).await;
    assert!(result.is_ok());
    let turn = result.unwrap();
    assert_eq!(turn.steps, 1);
}

/// LLM that always completes with a fixed finish reason (no tool calls).
struct FinishReasonLlm {
    finish_reason: &'static str,
}
impl LLM for FinishReasonLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "finish-reason-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let fr = self.finish_reason;
        Box::pin(async move {
            Ok(LLMChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some(fr.into()),
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

/// The provider finish-reason vocabulary mapped onto stop reasons, as a
/// direct unit test of the table (the per-reason turn tests below drive
/// the same function through a full turn for the two common cases).
#[test]
fn finish_reason_vocabulary_maps_to_stop_reasons() {
    use super::super::turn_stop_reason_from_finish;
    for reason in ["length", "max_tokens"] {
        assert!(
            matches!(
                turn_stop_reason_from_finish(Some(reason)),
                LoopTurnStopReason::MaxTokens
            ),
            "{reason} must map to MaxTokens"
        );
    }
    // The filtered family: OpenAI's content_filter, Anthropic's refusal,
    // and Google's safety vocabulary (lowercased by the transport) — v2's
    // kosong adapters map every one of them to `filtered`.
    for reason in [
        "content_filter",
        "refusal",
        "safety",
        "recitation",
        "blocklist",
        "prohibited_content",
        "spii",
        "image_safety",
    ] {
        assert!(
            matches!(
                turn_stop_reason_from_finish(Some(reason)),
                LoopTurnStopReason::Filtered
            ),
            "{reason} must map to Filtered"
        );
    }
    for reason in [
        None,
        Some("stop"),
        Some("end_turn"),
        Some("tool_calls"),
        Some("tool_use"),
    ] {
        assert!(
            matches!(
                turn_stop_reason_from_finish(reason),
                LoopTurnStopReason::EndTurn
            ),
            "{reason:?} must end the turn normally"
        );
    }
}

#[tokio::test]
async fn test_finish_reason_length_maps_to_max_tokens() {
    let llm = FinishReasonLlm {
        finish_reason: "length",
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-finish-length".into(),
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
    let turn = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::MaxTokens));
}

#[tokio::test]
async fn test_finish_reason_max_tokens_maps_to_max_tokens() {
    let llm = FinishReasonLlm {
        finish_reason: "max_tokens",
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-finish-max-tokens".into(),
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
    let turn = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::MaxTokens));
}

#[tokio::test]
async fn test_finish_reason_content_filter_maps_to_filtered() {
    let llm = FinishReasonLlm {
        finish_reason: "content_filter",
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-finish-filtered".into(),
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
    let turn = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::Filtered));
}

#[tokio::test]
async fn test_truncation_on_exhausted_steps_surfaces_max_tokens() {
    // The LLM always returns tool calls with a `length` finish reason;
    // the turn ends by max_steps exhaustion and must report MaxTokens,
    // not EndTurn.
    struct AlwaysToolTruncatedLlm;
    impl LLM for AlwaysToolTruncatedLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "always-tool-truncated"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            Box::pin(async move {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: "tc1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "/a.txt"}),
                        extras: None,
                        args_parse_error: None,
                    }],
                    finish_reason: Some("length".into()),
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
    let llm = AlwaysToolTruncatedLlm;
    let server = Arc::new(RpcServer::new());
    // Register a stub tool handler so the loop's per-step tool execution
    // does not block waiting for a host response.
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
        turn_id: "test-exhausted-truncated".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
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
    let turn = run_turn(input, &callbacks).await.unwrap();
    assert_eq!(turn.steps, 3);
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::MaxTokens));
}

#[tokio::test]
async fn test_cache_usage_accumulates_across_steps() {
    struct CacheUsageLlm {
        call: AtomicU32,
    }
    impl LLM for CacheUsageLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "cache-usage-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: "tc1".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path": "/a.txt"}),
                            extras: None,
                            args_parse_error: None,
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage {
                            input_tokens: 10,
                            output_tokens: 5,
                            total_tokens: 15,
                            input_cache_read: 4,
                            input_cache_creation: 3,
                        },
                        timing: None,
                    })
                } else {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_tokens: 8,
                            output_tokens: 2,
                            total_tokens: 10,
                            input_cache_read: 6,
                            input_cache_creation: 1,
                        },
                        timing: None,
                    })
                }
            })
        }
    }
    let llm = CacheUsageLlm {
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
        turn_id: "test-cache-usage".into(),
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
    let turn = run_turn(input, &callbacks).await.unwrap();
    assert_eq!(turn.usage.input_tokens, 18);
    assert_eq!(turn.usage.output_tokens, 7);
    assert_eq!(turn.usage.input_cache_read, 10);
    assert_eq!(turn.usage.input_cache_creation, 4);
}

/// Regression (found via the napi-integration suite after a fresh
/// `.node` build): the compaction rebind inside the step loop
/// (`let mut messages = compacted;`) shadowed the outer history
/// binding, so every step restarted from `[system] + user` and the
/// model never saw tool results. The history must accumulate across
/// steps: the second request carries the assistant tool_calls message
/// and the tool result.
#[tokio::test]
async fn test_history_accumulates_across_steps() {
    struct RecordingLlm {
        call: AtomicU32,
        requests: std::sync::Mutex<Vec<Vec<LLMMessage>>>,
    }
    impl LLM for RecordingLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "recording-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(params.messages.to_vec());
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: "tc1".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path": "/a.txt"}),
                            extras: None,
                            args_parse_error: None,
                        }],
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
    let llm = RecordingLlm {
        call: AtomicU32::new(0),
        requests: std::sync::Mutex::new(Vec::new()),
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
        turn_id: "test-history-accumulation".into(),
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
    run_turn(input, &callbacks).await.unwrap();
    let requests = llm.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    // Step 2's history: system + user + assistant(tool_calls) + tool result.
    let second = &requests[1];
    assert!(
        second
            .iter()
            .any(|m| m.role == "assistant" && m.tool_calls.iter().any(|tc| tc.id == "tc1")),
        "step 2 history must contain the assistant tool_calls message: {second:?}"
    );
    assert!(
        second.iter().any(|m| m.role == "tool"
            && m.tool_call_id.as_deref() == Some("tc1")
            && m.content == "stub"),
        "step 2 history must contain the tool result: {second:?}"
    );
}

/// A failed tool result reaches the model wrapped in v2's status sentinel.
///
/// The wrapper's own unit tests only exercise `render_status`; nothing
/// observed that the turn loop applies it. Replacing the call with the raw
/// content left this whole module, `server::` and both e2e tests green — the
/// feature could be switched off entirely without a red test.
#[tokio::test]
async fn a_failed_tool_result_reaches_the_model_wrapped() {
    struct OneToolLlm {
        call: AtomicU32,
        requests: std::sync::Mutex<Vec<Vec<LLMMessage>>>,
    }
    impl LLM for OneToolLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "one-tool-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(params.messages.to_vec());
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: "tc-err".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path": "/a.txt"}),
                            extras: None,
                            args_parse_error: None,
                        }],
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

    let llm = OneToolLlm {
        call: AtomicU32::new(0),
        requests: std::sync::Mutex::new(Vec::new()),
    };
    let server = Arc::new(RpcServer::new());
    // A tool that fails. `is_error` is what selects the error arm.
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "boom".into(),
                is_error: true,
                note: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-tool-error-wrapper".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "read it".into(),
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

    let requests = llm.requests.lock().unwrap();
    let second = &requests[1];
    let tool_message = second
        .iter()
        .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("tc-err"))
        .expect("the tool result must reach the model");
    assert_eq!(
        tool_message.content, "<system>ERROR: Tool execution failed.</system>\nboom",
        "a failed result must carry the status sentinel the model reads"
    );
}

/// The tool note reaches the model, appended after the status wrapper.
///
/// v2 `renderToolResultForModel` appends it last. This engine put it only in
/// an event, and the note is not decoration: Read fills it with the guidance
/// the model needs ("use Bash to read the elided content", "Edit and Write
/// expect UTF-8"), so hiding it left the model holding a truncated file with
/// no idea why.
#[tokio::test]
async fn a_tool_note_reaches_the_model_after_the_status() {
    struct NotingToolLlm {
        call: AtomicU32,
        requests: std::sync::Mutex<Vec<Vec<LLMMessage>>>,
    }
    impl LLM for NotingToolLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "noting-tool-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(params.messages.to_vec());
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: "tc-note".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path": "/a.txt"}),
                            extras: None,
                            args_parse_error: None,
                        }],
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

    let llm = NotingToolLlm {
        call: AtomicU32::new(0),
        requests: std::sync::Mutex::new(Vec::new()),
    };
    let server = Arc::new(RpcServer::new());
    // The note is what Read puts its model-facing guidance in.
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "file contents".into(),
                is_error: false,
                note: Some("<system>3 lines were truncated.</system>".into()),
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-tool-note".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "read it".into(),
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

    let requests = llm.requests.lock().unwrap();
    let tool_message = requests[1]
        .iter()
        .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("tc-note"))
        .expect("the tool result must reach the model");
    assert_eq!(
        tool_message.content, "file contents\n<system>3 lines were truncated.</system>",
        "the note must follow the status wrapper, as v2 appends it"
    );
}

/// Upstream #3966: the persisted (model-facing) tool result of a
/// wall-time tool (Bash here) opens with a `Wall time: X.XXX seconds`
/// header; the `read` case above (outside the set) keeps the bare
/// content.
#[tokio::test]
async fn test_tool_result_history_carries_wall_time_header_for_the_set() {
    struct OneShotToolLlm;
    impl LLM for OneShotToolLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "oneshot-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            Box::pin(async move {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: "tc1".into(),
                        name: "Bash".into(),
                        arguments: serde_json::json!({"command": "echo hi"}),
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
    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "hi\n".into(),
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
        turn_id: "test-wall-time-header".into(),
        llm: &OneShotToolLlm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 1,
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
    let outcome = run_turn(input, &callbacks).await.unwrap();
    let tool_message = outcome
        .messages
        .iter()
        .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("tc1"))
        .expect("the tool result must be in the final history");
    assert!(
        tool_message.content.starts_with("Wall time: ") && tool_message.content.ends_with("\nhi\n"),
        "the Bash result must carry the wall-time header: {:?}",
        tool_message.content
    );
}

/// Thinking round-trip: step 1's reasoning (with provider signature) must
/// reach step 2 inside the assistant history message — providers that
/// attested the thinking reject follow-ups without it.
#[tokio::test]
async fn test_thinking_blocks_round_trip_into_assistant_history() {
    use crate::rpc::types::ContentBlock;
    struct ThinkingLlm {
        call: AtomicU32,
        requests: std::sync::Mutex<Vec<Vec<LLMMessage>>>,
    }
    impl LLM for ThinkingLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "thinking-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(params.messages.to_vec());
            Box::pin(async move {
                if call == 0 {
                    Ok(LLMChatResponse {
                        content: "checking".into(),
                        thinking: vec![ContentBlock::Think {
                            think: "need to list files".into(),
                            encrypted: Some("sig-abc".into()),
                            details_index: None,
                            reasoning_key: None,
                            details_summary: None,
                        }],
                        tool_calls: vec![ToolCall {
                            id: "tc1".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path": "/a.txt"}),
                            extras: None,
                            args_parse_error: None,
                        }],
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
    let llm = ThinkingLlm {
        call: AtomicU32::new(0),
        requests: std::sync::Mutex::new(Vec::new()),
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
        turn_id: "test-thinking-round-trip".into(),
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
    run_turn(input, &callbacks).await.unwrap();
    let requests = llm.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    let assistant = second
        .iter()
        .find(|m| m.role == "assistant" && m.tool_calls.iter().any(|tc| tc.id == "tc1"))
        .expect("step 2 history must contain the assistant tool_calls message");
    assert_eq!(assistant.blocks.len(), 1, "{assistant:?}");
    match &assistant.blocks[0] {
        ContentBlock::Think {
            think, encrypted, ..
        } => {
            assert_eq!(think, "need to list files");
            assert_eq!(encrypted.as_deref(), Some("sig-abc"));
        }
        other => panic!("assistant history must carry the Think block, got {other:?}"),
    }
}
