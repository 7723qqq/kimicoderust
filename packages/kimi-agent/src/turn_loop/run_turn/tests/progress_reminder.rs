use super::super::*;
use super::common::*;
use super::*;

/// v2 `ProgressTrackerService` (`agent/progressTrack/progressTrackerService.ts`):
/// after `EBM_BLIND_MUTATION_THRESHOLD` successful writes that nothing could
/// have verified, the next request carries the "run a verification command"
/// reminder — once, and only from the point the threshold was crossed.
#[tokio::test]
async fn three_unverified_writes_arm_the_verification_reminder() {
    struct BlindWriteLlm {
        call: AtomicU32,
        requests: Arc<std::sync::Mutex<Vec<Vec<LLMMessage>>>>,
    }
    impl LLM for BlindWriteLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "blind-write-llm"
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
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params.messages.to_vec());
            Box::pin(async move {
                if call == 0 {
                    let write = |id: &str, path: &str| ToolCall {
                        id: id.into(),
                        name: "write".into(),
                        arguments: serde_json::json!({ "path": path, "content": "x" }),
                        extras: None,
                        args_parse_error: None,
                    };
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![
                            write("tc1", "/a.rs"),
                            write("tc2", "/b.rs"),
                            write("tc3", "/c.rs"),
                        ],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    Ok(LLMChatResponse {
                        content: "done".into(),
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

    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = BlindWriteLlm {
        call: AtomicU32::new(0),
        requests: requests.clone(),
    };
    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |_params| {
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
        turn_id: "test-progress-reminder".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "change three files".into(),
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
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(recorded.len(), 2, "one tool step, then the closing step");
    let reminder = crate::turn_loop::progress_track::EBM_REMINDER;
    assert!(
        !recorded[0].iter().any(|m| m.content.contains(reminder)),
        "the first request runs before anything was written"
    );
    assert!(
        recorded[1].iter().any(|m| m.content.contains(reminder)),
        "three unverified writes arm the reminder for the next request"
    );
}

/// A verification command resets the debt: two writes, one test run, then a
/// third write must not reach the threshold.
#[tokio::test]
async fn a_verification_command_keeps_the_reminder_silent() {
    struct VerifiedLlm {
        call: AtomicU32,
        requests: Arc<std::sync::Mutex<Vec<Vec<LLMMessage>>>>,
    }
    impl LLM for VerifiedLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "verified-llm"
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
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params.messages.to_vec());
            Box::pin(async move {
                match call {
                    0 => Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![
                            ToolCall {
                                id: "w1".into(),
                                name: "write".into(),
                                arguments: serde_json::json!({ "path": "/a.rs" }),
                                extras: None,
                                args_parse_error: None,
                            },
                            ToolCall {
                                id: "w2".into(),
                                name: "write".into(),
                                arguments: serde_json::json!({ "path": "/b.rs" }),
                                extras: None,
                                args_parse_error: None,
                            },
                        ],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                    // `cargo test` is a verification command and clears the
                    // debt (the call runs as a standalone step: bash takes the
                    // whole step).
                    1 => Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: "v1".into(),
                            name: "bash".into(),
                            arguments: serde_json::json!({ "command": "cargo test --lib" }),
                            extras: None,
                            args_parse_error: None,
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                    2 => Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: "w3".into(),
                            name: "write".into(),
                            arguments: serde_json::json!({ "path": "/c.rs" }),
                            extras: None,
                            args_parse_error: None,
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                    _ => Ok(LLMChatResponse {
                        content: "done".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                }
            })
        }
    }

    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = VerifiedLlm {
        call: AtomicU32::new(0),
        requests: requests.clone(),
    };
    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |_params| {
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
        turn_id: "test-progress-verified".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "change and verify".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 6,
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
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let reminder = crate::turn_loop::progress_track::EBM_REMINDER;
    assert!(
        recorded.iter().all(|messages| !messages
            .iter()
            .any(|message| message.content.contains(reminder))),
        "a verification run between the writes keeps the count below the threshold"
    );
}

/// v2 gates the reminder on `ebmInjectedThisTurn`
/// (`progressTrackerService.ts:71-75`): the flag is set once and reset only by
/// `turn.started`, so writing more unverified files after the threshold was
/// crossed must not queue the reminder again. `blind` stays at or above the
/// threshold, so an arm-per-write gate would put the reminder on every
/// following request.
#[tokio::test]
async fn the_verification_reminder_is_injected_once_per_turn() {
    struct SteadyWriteLlm {
        call: AtomicU32,
        requests: Arc<std::sync::Mutex<Vec<Vec<LLMMessage>>>>,
    }
    impl LLM for SteadyWriteLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "steady-write-llm"
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
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params.messages.to_vec());
            Box::pin(async move {
                // Four single-write steps: the third crosses the threshold and
                // the fourth stays past it, which is what a repeatable arm would
                // turn into a second injection.
                if call < 4 {
                    Ok(LLMChatResponse {
                        content: String::new(),
                        thinking: vec![],
                        tool_calls: vec![ToolCall {
                            id: format!("tc{call}"),
                            name: "write".into(),
                            arguments: serde_json::json!({
                                "path": format!("/f{call}.rs"),
                                "content": "x",
                            }),
                            extras: None,
                            args_parse_error: None,
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    Ok(LLMChatResponse {
                        content: "done".into(),
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

    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = SteadyWriteLlm {
        call: AtomicU32::new(0),
        requests: requests.clone(),
    };
    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |_params| {
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
        turn_id: "test-progress-reminder-once".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "change four files".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 10,
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
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(recorded.len(), 5, "four tool steps, then the closing step");
    let reminder = crate::turn_loop::progress_track::EBM_REMINDER;
    // An injection is pushed onto the turn history (`run_turn`'s
    // `messages.push(injection_message_named(..))`), so every later request
    // still carries the copy the crossing request added. What must not grow is
    // the number of copies: a repeatable arm would add a second one to request
    // 4.
    let copies = |messages: &Vec<LLMMessage>| {
        messages
            .iter()
            .filter(|message| message.content.contains(reminder))
            .count()
    };
    assert_eq!(copies(&recorded[2]), 0, "nothing before the threshold");
    assert_eq!(copies(&recorded[3]), 1, "the crossing request carries it");
    assert_eq!(
        copies(&recorded[4]),
        1,
        "a later write must not inject the reminder a second time"
    );
}
