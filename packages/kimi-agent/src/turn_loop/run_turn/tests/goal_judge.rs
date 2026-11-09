use super::super::*;
use super::common::*;
use super::*;

/// v2 `AgentGoalJudgeService` through `UpdateGoal`
/// (`features/goal/tools/update-goal/updateGoalTool.ts:68-83`): a `complete`
/// request is judged against the transcript before the write. A rejection
/// answers the call and never reaches the goal service.
#[tokio::test]
async fn a_rejected_completion_never_writes_the_goal() {
    struct JudgeLlm {
        call: AtomicU32,
        requests: Arc<std::sync::Mutex<Vec<Vec<LLMMessage>>>>,
    }
    impl LLM for JudgeLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "judge-llm"
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
                        tool_calls: vec![ToolCall {
                            id: "goal-complete".into(),
                            name: "UpdateGoal".into(),
                            arguments: serde_json::json!({ "status": "complete" }),
                            extras: None,
                            args_parse_error: None,
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                    // The judge request.
                    1 => Ok(LLMChatResponse {
                        content: r#"{"ok": false, "reason": "tests were never run"}"#.into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                    _ => Ok(LLMChatResponse {
                        content: "I will keep working.".into(),
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

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, |_params| {
        Box::pin(async move {
            let resp = types::StateReadResponse {
                value: serde_json::json!({
                    "goal": {
                        "goalId": "goal-1",
                        "objective": "Ship the feature",
                        "completionCriterion": "Tests pass",
                        "status": "active",
                        "turnsUsed": 1,
                        "tokensUsed": 10,
                        "inputTokensUsed": 6,
                        "outputTokensUsed": 4,
                        "wallClockMs": 1000,
                        "budget": {
                            "tokenBudget": null,
                            "turnBudget": null,
                            "wallClockBudgetMs": null,
                            "remainingTokens": null,
                            "remainingTurns": null,
                            "remainingWallClockMs": null,
                            "tokenBudgetReached": false,
                            "turnBudgetReached": false,
                            "wallClockBudgetReached": false,
                            "overBudget": false,
                            "inputTokensUsed": 6,
                            "outputTokensUsed": 4
                        },
                        "createdAt": 1,
                        "updatedAt": 2
                    }
                }),
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let writes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let write_log = writes.clone();
    RpcServer::register_arc(&server, types::methods::HOST_STATE_WRITE, move |params| {
        write_log.lock().unwrap().push(params.clone());
        Box::pin(async move {
            Err(JsonRpcError::internal_error(
                "the goal write must not run after a rejection".to_string(),
            ))
        })
    });
    let executed = Arc::new(AtomicU32::new(0));
    let executed_flag = executed.clone();
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |_params| {
        executed_flag.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Err(JsonRpcError::internal_error(
                "the rejected call must not reach tool execution".to_string(),
            ))
        })
    });

    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = JudgeLlm {
        call: AtomicU32::new(0),
        requests: requests.clone(),
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-goal-judge".into(),
        llm: &llm,
        messages: vec![LLMMessage::user("finish the goal")],
        tools: &[],
        tool_defs: vec![],
        max_steps: 4,
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
    assert_eq!(recorded.len(), 3, "goal step, judge request, closing step");
    // The judge is its own request with the judge system prompt and the
    // objective/criterion as the last user message.
    let judge_request = &recorded[1];
    assert!(
        judge_request[0].content.contains("independent judge"),
        "{:?}",
        judge_request[0]
    );
    assert!(
        judge_request.last().is_some_and(|message| message
            .content
            .contains("## Goal Objective\nShip the feature")),
        "{:?}",
        judge_request.last()
    );
    // The next step sees the rejection as the tool result.
    assert!(
        recorded[2].iter().any(|message| message.role == "tool"
            && message
                .content
                .contains("Goal completion rejected by judge: tests were never run")),
        "{:?}",
        recorded[2]
    );
    assert!(
        writes.lock().unwrap().is_empty(),
        "a rejected completion must not write goal state"
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        0,
        "the rejected call must not run as a tool"
    );
}

/// An `impossible` verdict turns the write into `blocked` (v2
/// `markBlocked({reason})`), so the goal stops instead of staying active.
/// In this host-forwarded setup the rewrite is visible as the
/// `host/execute_tool` request the engine sends.
#[tokio::test]
async fn an_impossible_verdict_blocks_the_goal() {
    struct ImpossibleLlm {
        call: AtomicU32,
    }
    impl LLM for ImpossibleLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "impossible-llm"
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
                        tool_calls: vec![ToolCall {
                            id: "goal-complete".into(),
                            name: "UpdateGoal".into(),
                            arguments: serde_json::json!({ "status": "complete" }),
                            extras: None,
                            args_parse_error: None,
                        }],
                        finish_reason: Some("tool_calls".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else if call == 1 {
                    // The judge request.
                    Ok(LLMChatResponse {
                        content: r#"{"ok": false, "impossible": true, "reason": "contradictory"}"#
                            .into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    Ok(LLMChatResponse {
                        content: "The goal is blocked.".into(),
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

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, |_params| {
        Box::pin(async move {
            let resp = types::StateReadResponse {
                value: serde_json::json!({
                    "goal": {
                        "goalId": "goal-1",
                        "objective": "Ship the feature",
                        "status": "active",
                        "turnsUsed": 1,
                        "tokensUsed": 10,
                        "inputTokensUsed": 6,
                        "outputTokensUsed": 4,
                        "wallClockMs": 1000,
                        "budget": {
                            "tokenBudget": null,
                            "turnBudget": null,
                            "wallClockBudgetMs": null,
                            "remainingTokens": null,
                            "remainingTurns": null,
                            "remainingWallClockMs": null,
                            "tokenBudgetReached": false,
                            "turnBudgetReached": false,
                            "wallClockBudgetReached": false,
                            "overBudget": false,
                            "inputTokensUsed": 6,
                            "outputTokensUsed": 4
                        },
                        "createdAt": 1,
                        "updatedAt": 2
                    }
                }),
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    // The host owns this tool call in the forwarded path; record what the
    // engine asked it to do and answer like a blocked goal write would.
    let executed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executed_log = executed.clone();
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |params| {
        executed_log.lock().unwrap().push(params.clone());
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: true,
                content: "Goal blocked.".into(),
                is_error: false,
                note: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let llm = ImpossibleLlm {
        call: AtomicU32::new(0),
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-goal-judge-impossible".into(),
        llm: &llm,
        messages: vec![LLMMessage::user("finish the goal")],
        tools: &[],
        tool_defs: vec![],
        max_steps: 4,
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

    let executed = executed.lock().unwrap();
    assert_eq!(executed.len(), 1, "the blocked write runs once");
    assert_eq!(executed[0]["tool_name"], "UpdateGoal");
    assert_eq!(
        executed[0]["arguments"]["status"], "blocked",
        "an impossible verdict must rewrite complete to blocked: {:?}",
        executed[0]
    );
}
