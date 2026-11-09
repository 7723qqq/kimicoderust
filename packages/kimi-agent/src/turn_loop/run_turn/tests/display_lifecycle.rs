use super::super::*;
use super::common::*;
use super::*;

/// v2's `RunnableToolExecution.display` has to ride `tool.call.started` —
/// that is the only channel a host sees before the call runs, and the one
/// the approval prompt reads. The unit tests on `agent_call_display` prove
/// its shape; this proves it is actually emitted, which is the part that
/// would otherwise rest on code review alone.
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

/// A tool that declares no display must not grow one — an empty
/// `display` would make hosts render a card for a plain tool. `ExitPlanMode`
/// needs host plan state to build its card, so it stays displayless.
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

/// `tool.native` only reports the outcome, so a consumer could never
/// tell that a call had started: the lifecycle events bracket every
/// execution, including the transport-error path.
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
    // The call id ties the pair to the same tool call.
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

/// v2 pairs `TurnStepStarted` with `TurnStepCompleted` from the loop itself
/// (loopService.ts:1393-1408). The boundary events must therefore come from
/// `run_turn`, not from the transport: one `llm.step.begin` per step, each
/// naming the turn and the 1-based ordinal, strictly before its `end` — a
/// summarizer or retry request that reuses the LLM instance can no longer
/// announce a step of its own.
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

/// A provider `content_filter` finish on the final step must surface
/// `Filtered` (v2 throws `PROVIDER_FILTERED`, loopService.ts:877-882),
/// which the session layer reports as a failed turn with the safety
/// message — not as a completion.
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

/// Every filtered spelling must land on `Filtered`, and the canonical
/// `filtered` value v2 carries must be among them — a filtered turn that
/// falls through to `EndTurn` reports success where v2 fails the turn
/// (`loopService.ts:877-882`).
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

/// The retry guard and the turn-level mapper are two copies of one
/// vocabulary, and the copies drifted once: `filtered` — the value v2
/// actually carries and the one the host delivers — reached the retry guard
/// but not the mapper, so a filtered turn was re-requested as an empty one
/// and then reported success. This is the gate that keeps them in step.
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

/// Drive a whole turn whose only LLM response carries `finish_reason` and
/// `content`, and hand back the stop reason the turn ended on.
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

/// The canonical `filtered` spelling — the one v2 actually carries, and the
/// one a host normalizing through kosong hands the engine — must fail the
/// turn, not complete it. The turn-level test above only exercised
/// `content_filter`, which is why the missing spelling shipped.
#[tokio::test]
async fn the_canonical_filtered_spelling_fails_the_turn() {
    let stop = stop_reason_of_a_single_response("test-canonical-filtered", "filtered", "").await;
    assert!(
        matches!(stop, LoopTurnStopReason::Filtered),
        "filtered must fail the turn, got {stop:?}"
    );
}

/// v2 latches `turn.filtered` from the finish reason alone
/// (`loopService.ts:1859`, `:1884-1894`) — it does not care whether the
/// response carried content, so neither does the mapper.
#[tokio::test]
async fn a_filtered_response_with_content_still_fails_the_turn() {
    let stop =
        stop_reason_of_a_single_response("test-filtered-content", "filtered", "partial").await;
    assert!(
        matches!(stop, LoopTurnStopReason::Filtered),
        "filtered with content must still fail the turn, got {stop:?}"
    );
}

/// Guard against the opposite drift: an ordinary finish reason still ends
/// the turn normally through the whole loop, not just the mapper.
#[tokio::test]
async fn an_ordinary_finish_reason_completes_the_turn() {
    let stop = stop_reason_of_a_single_response("test-ordinary-stop", "stop", "hello").await;
    assert!(
        matches!(stop, LoopTurnStopReason::EndTurn),
        "stop must end the turn normally, got {stop:?}"
    );
}
