use super::super::*;
use super::common::*;
use super::*;

#[tokio::test]
async fn a_swarm_tool_call_rides_its_display_on_the_started_event() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-swarm".into(),
            name: "AgentSwarm".into(),
            arguments: serde_json::json!({
                "description": "three-way review",
                "prompt_template": "review {{item}}",
                "items": ["a", "b", "c"],
            }),
            extras: None,
            args_parse_error: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                content: "<agent_swarm_result></agent_swarm_result>".into(),
                is_error: false,
                note: None,
                stop_turn: false,
                delivery: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-swarm-display".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "go".into(),
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

    let _ = run_turn(input, &callbacks).await;

    let started = events
        .lock()
        .unwrap()
        .iter()
        .find(|event| {
            event.get("type").and_then(|t| t.as_str()) == Some("tool.call.started")
                && event.get("tool_name").and_then(|t| t.as_str()) == Some("AgentSwarm")
        })
        .cloned()
        .expect("the swarm call announced itself");

    assert_eq!(
        started["display"]["kind"], "agent_call",
        "the host gets a card, not the raw arguments: {started}"
    );
    assert_eq!(started["display"]["agent_name"], "swarm (3 subagents)");
    assert_eq!(started["display"]["prompt"], "three-way review");
}

#[tokio::test]
async fn a_tool_without_a_display_ships_none() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-plain".into(),
            name: "ExitPlanMode".into(),
            arguments: serde_json::json!({}),
            extras: None,
            args_parse_error: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            let resp = ToolExecuteResponse {
                content: "stub".into(),
                is_error: false,
                note: None,
                stop_turn: false,
                delivery: None,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-no-display".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "go".into(),
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

    let _ = run_turn(input, &callbacks).await;

    let started = events
        .lock()
        .unwrap()
        .iter()
        .find(|event| {
            event.get("type").and_then(|t| t.as_str()) == Some("tool.call.started")
                && event.get("tool_name").and_then(|t| t.as_str()) == Some("ExitPlanMode")
        })
        .cloned()
        .expect("the call announced itself");
    assert!(
        started.get("display").is_none(),
        "no display key at all, not a null: {started}"
    );
}

#[tokio::test]
async fn test_tool_call_lifecycle_events_bracket_execution() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-lifecycle".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({ "query": "x" }),
            extras: None,
            args_parse_error: None,
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
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-tool-call-lifecycle".into(),
        llm: &llm,
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

    let _ = run_turn(input, &callbacks).await;
    let types: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| {
            event
                .get("type")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        types.iter().any(|t| t == "tool.call.started"),
        "a started event must bracket the call: {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "tool.call.completed"),
        "a completed event must close the call: {types:?}"
    );

    let started = events
        .lock()
        .unwrap()
        .iter()
        .find(|event| event.get("type").and_then(|t| t.as_str()) == Some("tool.call.started"))
        .cloned()
        .unwrap();
    assert_eq!(started["tool_call_id"], "tc-lifecycle");
    assert_eq!(started["tool_name"], "WebSearch");
}

#[tokio::test]
async fn step_boundaries_are_paired_one_begin_and_one_end_per_step() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-boundary".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({ "query": "x" }),
            extras: None,
            args_parse_error: None,
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
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-step-boundaries".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 2,
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
    assert_eq!(turn.steps, 2, "two tool steps ran");

    let boundaries: Vec<serde_json::Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(
                event.get("type").and_then(|t| t.as_str()),
                Some("llm.step.begin" | "llm.step.end")
            )
        })
        .cloned()
        .collect();

    let sequence: Vec<&str> = boundaries
        .iter()
        .filter_map(|event| event.get("type").and_then(|t| t.as_str()))
        .collect();
    assert_eq!(
        sequence,
        [
            "llm.step.begin",
            "llm.step.end",
            "llm.step.begin",
            "llm.step.end"
        ],
        "every begin is closed by exactly one end before the next step: {sequence:?}"
    );

    for (index, pair) in boundaries.chunks(2).enumerate() {
        for event in pair {
            assert_eq!(event["turn_id"], "test-step-boundaries");
            assert_eq!(event["step"], serde_json::json!(index + 1));
            assert_eq!(
                event["step_id"],
                serde_json::json!(format!("test-step-boundaries-step-{}", index + 1)),
                "v2 stamps the same stepId on started and completed"
            );
        }
        assert_eq!(
            pair[0]["step_id"], pair[1]["step_id"],
            "begin and end of one step must share the id"
        );
    }
}

#[tokio::test]
async fn test_filtered_finish_surfaces_filtered_reason() {
    struct FilteredLlm;
    impl LLM for FilteredLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "filtered"
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
                    tool_calls: vec![],
                    finish_reason: Some("content_filter".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-filtered".into(),
        llm: &FilteredLlm,
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

#[test]
fn every_filtered_spelling_maps_to_filtered() {
    for spelling in [
        "filtered",
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
                turn_stop_reason_from_finish(Some(spelling)),
                LoopTurnStopReason::Filtered
            ),
            "{spelling} must map to Filtered"
        );
    }
    assert!(matches!(
        turn_stop_reason_from_finish(Some("length")),
        LoopTurnStopReason::MaxTokens
    ));
    assert!(matches!(
        turn_stop_reason_from_finish(Some("max_tokens")),
        LoopTurnStopReason::MaxTokens
    ));
    for ordinary in [Some("stop"), Some("tool_use"), Some("end_turn"), None] {
        assert!(
            matches!(
                turn_stop_reason_from_finish(ordinary),
                LoopTurnStopReason::EndTurn
            ),
            "{ordinary:?} must end the turn normally"
        );
    }
}

#[test]
fn the_retry_guard_and_the_turn_mapper_agree_on_every_finish_reason() {
    let vocabulary = [
        "filtered",
        "content_filter",
        "refusal",
        "safety",
        "recitation",
        "blocklist",
        "prohibited_content",
        "spii",
        "image_safety",
        "length",
        "max_tokens",
        "stop",
        "tool_use",
        "end_turn",
    ];
    for reason in vocabulary {
        let retry_guard_says_filtered =
            crate::turn_loop::turn_step::is_content_filtered(Some(reason));
        let turn_says_filtered = matches!(
            turn_stop_reason_from_finish(Some(reason)),
            LoopTurnStopReason::Filtered
        );
        assert_eq!(
            retry_guard_says_filtered, turn_says_filtered,
            "{reason}: the retry guard and the turn mapper disagree"
        );
    }
}

async fn stop_reason_of_a_single_response(
    turn_id: &str,
    finish_reason: &str,
    content: &str,
) -> LoopTurnStopReason {
    let llm = FixedResponseLlm {
        finish_reason: finish_reason.to_string(),
        content: content.to_string(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let turn = run_turn(
        RunTurnInput {
            llm_request_byte_budget: u64::MAX,
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            previous_turn_aborted: false,
            turn_id: turn_id.to_string(),
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
        },
        &callbacks,
    )
    .await
    .unwrap();
    turn.stop_reason
}

#[tokio::test]
async fn the_canonical_filtered_spelling_fails_the_turn() {
    let stop = stop_reason_of_a_single_response("test-canonical-filtered", "filtered", "").await;
    assert!(
        matches!(stop, LoopTurnStopReason::Filtered),
        "filtered must fail the turn, got {stop:?}"
    );
}

#[tokio::test]
async fn a_filtered_response_with_content_still_fails_the_turn() {
    let stop =
        stop_reason_of_a_single_response("test-filtered-content", "filtered", "partial").await;
    assert!(
        matches!(stop, LoopTurnStopReason::Filtered),
        "filtered with content must still fail the turn, got {stop:?}"
    );
}

#[tokio::test]
async fn an_ordinary_finish_reason_completes_the_turn() {
    let stop = stop_reason_of_a_single_response("test-ordinary-stop", "stop", "hello").await;
    assert!(
        matches!(stop, LoopTurnStopReason::EndTurn),
        "stop must end the turn normally, got {stop:?}"
    );
}
