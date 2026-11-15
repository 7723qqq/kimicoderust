use super::super::*;
use super::common::*;
use super::*;

#[tokio::test]
async fn test_inactive_goal_runs_the_turn_as_ordinary_chat() {
    for (status, turn_id) in [
        (GoalStatus::Paused, "test-paused"),
        (GoalStatus::Blocked, "test-blocked"),
    ] {
        let llm = PredictTestLlm {
            system_prompt: "You are helpful.".into(),
            model_name: "test-model".into(),
            return_tool_calls: false,
            tool_responses: vec![],
        };
        let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));
        let goal = GoalContext {
            goal_id: "g1".into(),
            objective: "Do thing".into(),
            status,
            token_budget: None,
            turn_budget: None,
            tokens_used: 0,
            turns_used: 0,
            wall_clock_budget_ms: None,
            wall_clock_ms: 0,
            updated_at: 0,
            terminal_reason: None,
        };
        let input = RunTurnInput {
            llm_request_byte_budget: u64::MAX,
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            previous_turn_aborted: false,
            turn_id: turn_id.into(),
            llm: &llm,
            messages: vec![LLMMessage {
                role: kimi_core::types::Role::User,
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
        assert!(
            matches!(result.stop_reason, LoopTurnStopReason::EndTurn),
            "{turn_id}: an inactive goal must not drive the turn"
        );
        assert_eq!(result.steps, 1, "{turn_id}");
    }
}

#[tokio::test]
async fn test_goal_token_budget_exhausted() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, captured_events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let capturing: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let goal = GoalContext {
        goal_id: "g3".into(),
        objective: "Do thing".into(),
        status: GoalStatus::Active,
        token_budget: Some(100),
        turn_budget: None,

        tokens_used: 100,
        turns_used: 0,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-budget-tokens".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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

    let result = run_turn(input, &capturing).await.unwrap();
    assert!(matches!(
        result.stop_reason,
        LoopTurnStopReason::BudgetLimited
    ));
    assert_eq!(result.steps, 0);

    let events = captured_events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "goal.budget.limit_reached" && e["goal_id"] == "g3"),
        "budget exhaustion must emit goal.budget.limit_reached, got {events:?}"
    );
}

#[tokio::test]
async fn test_goal_turn_budget_exhausted() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let goal = GoalContext {
        goal_id: "g4".into(),
        objective: "Do thing".into(),
        status: GoalStatus::Active,
        token_budget: None,
        turn_budget: Some(3),
        tokens_used: 0,

        turns_used: 3,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-budget-turns".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
    assert!(matches!(
        result.stop_reason,
        LoopTurnStopReason::BudgetLimited
    ));
    assert_eq!(result.steps, 0);
}

#[tokio::test]
async fn test_goal_active_with_budget_completes() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let goal = GoalContext {
        goal_id: "g5".into(),
        objective: "Do thing".into(),
        status: GoalStatus::Active,
        token_budget: Some(10000),
        turn_budget: Some(100),
        tokens_used: 100,
        turns_used: 1,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-active-goal".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn test_goal_wall_clock_deadline_triggers_budget_limit() {
    struct SlowLlm;
    impl LLM for SlowLlm {
        fn system_prompt(&self) -> &str {
            "slow"
        }
        fn model_name(&self) -> &str {
            "slow-model"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            Box::pin(async move {
                if let Some(token) = params.cancel {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_millis(1000)) => {
                            Ok(LLMChatResponse {
                                content: "done".into(),
                                thinking: vec![],
                                tool_calls: vec![],
                                finish_reason: Some("stop".into()),
                                usage: TokenUsage::default(),
                            timing: None,
                            })
                        }
                        _ = token.cancelled() => {
                            Err(Box::new(std::io::Error::other("cancelled")) as Box<dyn std::error::Error + Send + Sync>)
                        }
                    }
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
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

    let llm = SlowLlm;
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let goal = GoalContext {
        goal_id: "g-deadline".into(),
        objective: "Fast finish".into(),
        status: GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        tokens_used: 0,
        turns_used: 0,
        wall_clock_budget_ms: Some(50),
        wall_clock_ms: 0,
        updated_at: 0,
        terminal_reason: None,
    };

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-deadline-turn".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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

    let result = run_turn(input, &callbacks).await;
    let evts = events.lock().unwrap();
    assert!(
        evts.iter()
            .any(|e| e["type"] == "goal.budget.limit_reached" && e["budget_type"] == "wall_clock"),
        "deadline scheduler must emit limit_reached event, got: {evts:?}"
    );
    drop(evts);
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_cancellation_set_before_turn_aborts() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-cancel-before".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
        cancellation: Some(cancel_flag),
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::Aborted));
    assert_eq!(result.steps, 0);
}

struct CancelDuringToolCallbacks {
    inner: Arc<dyn HostCallbacks>,
    cancellation: Arc<std::sync::atomic::AtomicBool>,
}

impl HostCallbacks for CancelDuringToolCallbacks {
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
        self.cancellation.store(true, Ordering::Relaxed);
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        self.inner.check_permission(request)
    }

    fn emit_event(&self, event: serde_json::Value) {
        self.inner.emit_event(event);
    }
}

#[tokio::test]
async fn test_cancellation_during_tool_execution_aborts() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-cancel".into(),
            name: "read".into(),
            arguments: Some(serde_json::json!({ "path": "/a.txt" }).to_string()),
            extras: None,
            raw_id: None,
            _stream_index: None,
        }],
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

    let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(CancelDuringToolCallbacks {
        inner: rpc_callbacks(server.clone()),
        cancellation: cancellation.clone(),
    });

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-cancel-during-tools".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
        cancellation: Some(cancellation),
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks)
        .await
        .expect("a cancel must not fail the turn");
    assert!(matches!(result.stop_reason, LoopTurnStopReason::Aborted));
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn a_tool_finishing_inside_the_abort_grace_keeps_its_result() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-grace".into(),
            name: "read".into(),
            arguments: Some(serde_json::json!({ "path": "/a.txt" }).to_string()),
            extras: None,
            raw_id: None,
            _stream_index: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "the file contents".into(),
                is_error: false,
                note: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(CancelDuringToolCallbacks {
        inner: rpc_callbacks(server.clone()),
        cancellation: cancellation.clone(),
    });

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-grace-keeps-result".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
        cancellation: Some(cancellation),
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let started = std::time::Instant::now();
    let result = run_turn(input, &callbacks)
        .await
        .expect("a cancel inside the grace must not fail the turn");

    assert!(matches!(result.stop_reason, LoopTurnStopReason::Aborted));
    let tool_text = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        tool_text.contains("the file contents"),
        "the real result must reach the model: {tool_text:?}"
    );
    assert!(
        !tool_text.contains("The user manually interrupted"),
        "a tool that succeeded must not be reported as interrupted: {tool_text:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the turn must not wait out the full grace for a call that finished: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn test_tool_result_stop_turn_ends_turn_completed() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-stop".into(),
            name: "read".into(),
            arguments: Some(serde_json::json!({ "path": "/a.txt" }).to_string()),
            extras: None,
            raw_id: None,
            _stream_index: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                delivery: None,
                stop_turn: true,
                content: "goal reached".into(),
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
        turn_id: "test-stop-turn".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
        .expect("stop_turn must end the turn cleanly");
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn test_cancellation_cleared_runs_normally() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-cancel-clear".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: kimi_core::types::Role::User,
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
        cancellation: Some(cancel_flag),
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(result.steps, 1);
}
