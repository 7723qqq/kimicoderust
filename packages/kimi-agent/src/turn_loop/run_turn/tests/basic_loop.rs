use super::super::*;
use super::common::*;
use super::*;

#[tokio::test]
async fn test_run_turn_no_tool_calls() {
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
        turn_id: "test-turn-1".into(),
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
    let turn = result.unwrap();
    assert_eq!(turn.steps, 1);
}

/// The swarm gate must refuse a model response that mixes `AgentSwarm`
/// with another tool *before* either call executes, and the refusal has
/// to reach the model as the tool result so it can retry with a legal
/// batch (v2 `AgentSwarmService.onBeforeExecuteTool`).
#[tokio::test]
async fn a_vetoed_swarm_batch_is_refused_and_the_model_can_retry() {
    let agent_id = "swarm-veto-active-agent";
    let registry = crate::swarm::mode::swarm_mode_registry();
    registry.exit(agent_id);
    assert!(registry.enter(agent_id, crate::swarm::mode::SwarmModeTrigger::Manual));

    let llm = SequencedLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        steps: vec![
            // Step 1: the illegal batch — a swarm plus a sibling tool.
            (
                vec![
                    ToolCall {
                        id: "swarm-1".into(),
                        name: "AgentSwarm".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                        args_parse_error: None,
                    },
                    ToolCall {
                        id: "bash-1".into(),
                        name: "Bash".into(),
                        arguments: serde_json::json!({ "command": "echo hi" }),
                        extras: None,
                        args_parse_error: None,
                    },
                ],
                "tool_calls",
            ),
            // Step 2: after reading the refusal, answered in text. A
            // *compliant* AgentSwarm is deliberately not scripted here:
            // clearing the gate would make this test drive the whole
            // subagent stack (and the stdio round-trip that stalls it),
            // which `swarm_tool.rs` already covers on its own.
            (vec![], "stop"),
        ],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: agent_id.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-swarm-veto".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Swarm and also run a command.".into(),
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
    let turn = result.expect("the refusal resolves as a normal turn");

    // Every call in the vetoed batch carries the refusal — neither one ran.
    let refusals: Vec<_> = turn
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .filter(|m| m.content.contains("AgentSwarm must be the only tool call"))
        .collect();
    assert_eq!(
        refusals.len(),
        2,
        "both calls in the vetoed batch must carry the refusal: {:?}",
        turn.messages
    );
    assert_eq!(
        refusals[0].tool_call_id.as_deref(),
        Some("swarm-1"),
        "the refusal is linked to the call it refuses"
    );

    // The turn did not die on the veto: the model got another step and
    // answered from it. Ending the turn on the refusal (what this used to
    // do) would have left the user a dead end on a recoverable mistake.
    assert_eq!(
        turn.steps, 2,
        "veto then answer — the veto must not end the turn"
    );
    assert_eq!(turn.stop_reason, LoopTurnStopReason::EndTurn);
    registry.exit(agent_id);
}

#[tokio::test]
async fn a_call_with_unusable_arguments_is_answered_not_executed() {
    // v2 `preflightToolCall` (`toolExecutorService.ts`): a call whose payload
    // did not decode, or decoded to something that is not an object, is neither
    // executed with a fabricated `{}` nor dropped. The assistant message keeps
    // its `tool_use` — a strict provider rejects a `tool_use` with no matching
    // `tool_result` — and the call is answered with the reason, which is the
    // only thing that lets the model correct itself.
    let llm = SequencedLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        steps: vec![
            (
                vec![
                    ToolCall {
                        id: "broken-json".into(),
                        name: "Bash".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                        args_parse_error: Some(crate::turn_loop::types::ArgsParseError {
                            raw: r#"{"command": "ech"#.into(),
                            error: "EOF while parsing a string".into(),
                        }),
                    },
                    ToolCall {
                        id: "not-an-object".into(),
                        name: "Read".into(),
                        arguments: serde_json::json!("a.txt"),
                        extras: None,
                        args_parse_error: None,
                    },
                ],
                "tool_calls",
            ),
            (vec![], "stop"),
        ],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-unusable-args".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Run those.".into(),
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

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_turn(input, &callbacks),
    )
    .await
    .expect(
        "an unrunnable call must be answered, not dispatched to the host, which would block here",
    );
    let turn = result.expect("a call the engine can answer is not a turn failure");

    let assistant = turn
        .messages
        .iter()
        .find(|m| m.role == "assistant")
        .expect("the assistant message is in the history");
    assert_eq!(
        assistant
            .tool_calls
            .iter()
            .map(|tc| tc.id.as_str())
            .collect::<Vec<_>>(),
        vec!["broken-json", "not-an-object"],
        "both calls stay in the message that issued them"
    );

    let answer_for = |id: &str| {
        turn.messages
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some(id))
            .map(|m| m.content.clone())
            .unwrap_or_else(|| panic!("{id} must still get a result: {:?}", turn.messages))
    };

    let broken = answer_for("broken-json");
    assert!(
        broken.contains("Invalid Bash arguments") && broken.contains("not valid JSON"),
        "the answer names the real cause: {broken}"
    );
    assert!(
        !broken.contains("{{"),
        "both parameters are substituted: {broken}"
    );
    assert!(
        broken.contains("EOF while parsing a string"),
        "the decoder's reason reaches the model: {broken}"
    );

    let not_object = answer_for("not-an-object");
    assert!(
        not_object.contains("Invalid Read arguments") && not_object.contains("JSON object"),
        "a non-object payload is refused for the same reason v2's schema check would: {not_object}"
    );

    assert_eq!(
        turn.steps, 2,
        "the model reads both answers and continues — it is not the end of the turn"
    );
    assert_eq!(turn.stop_reason, LoopTurnStopReason::EndTurn);
}

#[tokio::test]
async fn a_mixed_batch_runs_other_tools_while_swarm_mode_is_off() {
    let agent_id = "swarm-veto-inactive-agent";
    let registry = crate::swarm::mode::swarm_mode_registry();
    registry.exit(agent_id);

    let llm = SequencedLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        steps: vec![
            (
                vec![
                    ToolCall {
                        id: "swarm-off-1".into(),
                        name: "AgentSwarm".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                        args_parse_error: None,
                    },
                    ToolCall {
                        id: "bash-off-1".into(),
                        name: "Bash".into(),
                        arguments: serde_json::json!({ "command": "echo hi" }),
                        extras: None,
                        args_parse_error: None,
                    },
                ],
                "tool_calls",
            ),
            (vec![], "stop"),
        ],
    };

    let server = Arc::new(RpcServer::new());
    let executed = Arc::new(Mutex::new(Vec::new()));
    let executed_by_host = executed.clone();
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |params| {
        let name = params["tool_name"].as_str().unwrap_or_default().to_string();
        executed_by_host.lock().unwrap().push(name);
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
        agent_id: agent_id.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-swarm-veto-inactive".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Try a swarm and run a command.".into(),
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

    let turn = run_turn(input, &callbacks)
        .await
        .expect("an inactive swarm gate does not veto the batch");
    let mut executed = executed.lock().unwrap().clone();
    executed.sort();
    assert_eq!(
        executed,
        vec!["AgentSwarm".to_string(), "Bash".to_string()],
        "the sibling tool must reach execution while swarm mode is off: {:?}",
        turn.messages
    );
    assert!(
        !turn.messages.iter().any(|message| {
            message.role == "tool"
                && message
                    .content
                    .contains("AgentSwarm must be the only tool call")
        }),
        "the inactive batch gate emits no exclusivity refusal: {:?}",
        turn.messages
    );
    assert_eq!(turn.steps, 2);
    registry.exit(agent_id);
}

/// A model that never complies must not spin: the step budget is the same
/// bound v2 relies on, and running out of it fails the turn as
/// `max_steps` rather than answering as if the work had been done.
#[tokio::test]
async fn a_model_that_never_complies_runs_out_of_steps() {
    let agent_id = "swarm-veto-retry-agent";
    let registry = crate::swarm::mode::swarm_mode_registry();
    registry.exit(agent_id);
    assert!(registry.enter(agent_id, crate::swarm::mode::SwarmModeTrigger::Manual));

    let llm = SequencedLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        steps: vec![(
            vec![
                ToolCall {
                    id: "swarm-1".into(),
                    name: "AgentSwarm".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                },
                ToolCall {
                    id: "bash-1".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({ "command": "echo hi" }),
                    extras: None,
                    args_parse_error: None,
                },
            ],
            "tool_calls",
        )],
    };

    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        agent_id: agent_id.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-swarm-veto-loop".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Swarm and also run a command.".into(),
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

    let turn = run_turn(input, &callbacks)
        .await
        .expect("the refusal resolves as a normal turn");
    assert_eq!(turn.steps, 3, "the step budget bounds the retry loop");
    assert_eq!(
        turn.stop_reason,
        LoopTurnStopReason::MaxSteps,
        "a model that never complies fails the turn, it does not fake success"
    );
    // v2 closes the step it began (loopService.ts:2201 emitStepInterrupted →
    // failMachineStep): the budget ran out with a step in flight.
    let interruptions: Vec<serde_json::Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| {
            event.get("type").and_then(|value| value.as_str()) == Some("llm.step.interrupted")
        })
        .cloned()
        .collect();
    assert_eq!(interruptions.len(), 1, "exactly one step is closed");
    assert_eq!(interruptions[0]["reason"], "max_steps");
    assert_eq!(interruptions[0]["step"], 3);
    registry.exit(agent_id);
}

/// A provider failure closes the step it opened (v2 `failMachineStep`,
/// loopService.ts:2160-2171): the turn fails, and exactly one
/// `llm.step.interrupted{reason:"error"}` names the step and the message so
/// the host's step display does not keep a dead step "streaming".
#[tokio::test]
async fn an_llm_error_closes_the_step_it_began() {
    struct FailingLlm;
    impl LLM for FailingLlm {
        fn system_prompt(&self) -> &str {
            "You are helpful."
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            Box::pin(async move {
                Err(Box::new(std::io::Error::other("provider exploded"))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-llm-error".into(),
        llm: &FailingLlm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hello".into(),
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

    let error = run_turn(input, &callbacks)
        .await
        .expect_err("a provider failure fails the turn");
    assert!(
        error.to_string().contains("provider exploded"),
        "the provider text reaches the caller: {error}"
    );

    let captured = events.lock().unwrap().clone();
    let interruptions: Vec<&serde_json::Value> = captured
        .iter()
        .filter(|event| {
            event.get("type").and_then(|value| value.as_str()) == Some("llm.step.interrupted")
        })
        .collect();
    assert_eq!(
        interruptions.len(),
        1,
        "one begun step is closed exactly once: {captured:?}"
    );
    assert_eq!(interruptions[0]["reason"], "error");
    assert_eq!(
        interruptions[0]["step"], 1,
        "the same one-based ordinal as llm.step.begin"
    );
    assert!(
        interruptions[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("provider exploded")),
        "the message carries the provider text: {}",
        interruptions[0]["message"]
    );
}

/// The interruption reminder must survive the `run_turn_continued`
/// wrapper: its first iteration *is* a fresh turn's head, so the caller's
/// `previous_turn_aborted` has to reach the injection registry there.
/// The wrapper used to rebuild every iteration with `false`, which made
/// the reminder unreachable from every production entry point (they all
/// route through it) while the reminder module's own unit tests stayed
/// green.
#[tokio::test]
async fn test_previous_turn_aborted_reaches_the_interruption_reminder() {
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
        previous_turn_aborted: true,
        turn_id: "test-turn-interruption-reminder".into(),
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

    let turn = run_turn_continued(input, &callbacks)
        .await
        .expect("turn runs");
    assert!(
        turn.messages.iter().any(|message| message
            .content
            .as_str()
            .contains("The previous turn was interrupted by the user")),
        "the interruption reminder must reach the turn's messages: {:?}",
        turn.messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
    );
}

/// The stale-todo reminder must reach a real turn: ten assistant steps
/// after the last `TodoList` write, the main agent's next step carries the
/// v2 reminder while the list still has an open item. This pins the run_turn
/// wiring (main-agent gate, tool-active gate, state read) rather than the
/// module's own unit tests.
#[tokio::test]
async fn test_todo_stale_reminder_reaches_a_stale_main_agent_turn() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    // Overwrite the no-op state_read (a HashMap insert, so this wins) and
    // answer the todo domain with a list that still has an open item — the
    // reminder stays silent for an empty or all-done list.
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, |params| {
        Box::pin(async move {
            let value = if params.get("domain").and_then(|v| v.as_str()) == Some("todo") {
                serde_json::json!([
                    { "id": "t1", "title": "Ship it", "status": "in_progress" }
                ])
            } else {
                serde_json::Value::Null
            };
            let resp = types::StateReadResponse { value };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let mut messages = vec![LLMMessage::new("user", "Start")];
    messages.push(LLMMessage {
        role: "assistant".into(),
        tool_calls: vec![crate::turn_loop::types::ToolCall {
            id: "call_todo_write".into(),
            name: "TodoList".into(),
            arguments: serde_json::json!({
                "todos": [{ "title": "Ship it", "status": "in_progress" }]
            }),
            extras: None,
            args_parse_error: None,
        }],
        ..Default::default()
    });
    for _ in 0..10 {
        messages.push(LLMMessage::new("assistant", "working"));
    }
    messages.push(LLMMessage::new("user", "Continue"));

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-todo-reminder".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![crate::tools::todo_list::todo_list_tool_def()],
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

    let turn = run_turn(input, &callbacks).await.expect("turn runs");
    assert!(
        turn.messages.iter().any(|message| message
            .content
            .contains("The todo list has not been updated in the last 10 steps (0/1 done, \"Ship it\" still in_progress).")),
        "the stale-todo reminder must reach the main agent's turn: {:?}",
        turn.messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
    );
}

/// v2 registers the todo reminder only for `MAIN_AGENT_ID`; a subagent
/// running the same stale history must not receive it.
#[tokio::test]
async fn test_todo_stale_reminder_is_main_agent_only() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let mut messages = vec![LLMMessage::new("user", "Start")];
    messages.push(LLMMessage {
        role: "assistant".into(),
        tool_calls: vec![crate::turn_loop::types::ToolCall {
            id: "call_todo_write".into(),
            name: "TodoList".into(),
            arguments: serde_json::json!({
                "todos": [{ "title": "Ship it", "status": "in_progress" }]
            }),
            extras: None,
            args_parse_error: None,
        }],
        ..Default::default()
    });
    for _ in 0..10 {
        messages.push(LLMMessage::new("assistant", "working"));
    }
    messages.push(LLMMessage::new("user", "Continue"));

    let input = RunTurnInput {
        agent_id: "subagent-1".into(),
        previous_turn_aborted: false,
        turn_id: "test-turn-todo-reminder-subagent".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![crate::tools::todo_list::todo_list_tool_def()],
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

    let turn = run_turn(input, &callbacks).await.expect("turn runs");
    assert!(
        !turn.messages.iter().any(|message| message
            .content
            .contains("The todo list has not been updated in the last ")),
        "a subagent must not receive the main agent's todo reminder"
    );
}

/// Turn-lifecycle hooks (`UserPromptSubmit` / `PreCompact`) ride
/// `RunTurnInput.hook_guard`. A prompt hook without block/append output only
/// observes, so a turn carrying one completes exactly like one without.
#[tokio::test]
async fn test_turn_lifecycle_hooks_do_not_disturb_turn() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let hook = |event: &str| crate::permission::HookDef {
        event: event.into(),
        matcher: String::new(),
        command: (if cfg!(windows) { "rem" } else { ":" }).into(),
        timeout: Some(5),
        cwd: None,
        env: None,
    };
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        hook("UserPromptSubmit"),
        hook("PreCompact"),
    ]));

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-hooks".into(),
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
        hook_guard: Some(guard),
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

struct PromptRecordingLlm {
    calls: std::sync::atomic::AtomicUsize,
    requests: std::sync::Mutex<Vec<Vec<LLMMessage>>>,
}

impl PromptRecordingLlm {
    fn new() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl LLM for PromptRecordingLlm {
    fn system_prompt(&self) -> &str {
        "You are helpful."
    }

    fn model_name(&self) -> &str {
        "prompt-hook-recorder"
    }

    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.requests.lock().unwrap().push(params.messages.to_vec());
        Box::pin(async move {
            Ok(LLMChatResponse {
                content: "model answered".into(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: crate::rpc::types::TokenUsage::default(),
                timing: None,
            })
        })
    }
}

fn prompt_hook_input<'a>(
    llm: &'a dyn LLM,
    guard: Arc<crate::tools::external_hooks::HookGuard>,
    turn_id: &str,
) -> RunTurnInput<'a> {
    RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: turn_id.into(),
        llm,
        messages: vec![LLMMessage::user("Hello!")],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: Some(guard),
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    }
}

fn prompt_hook_guard(commands: &[&str]) -> Arc<crate::tools::external_hooks::HookGuard> {
    Arc::new(crate::tools::external_hooks::HookGuard::new(
        commands
            .iter()
            .map(|command| crate::permission::HookDef {
                event: "UserPromptSubmit".into(),
                matcher: String::new(),
                command: (*command).into(),
                timeout: None,
                cwd: None,
                env: None,
            })
            .collect(),
    ))
}

fn stdout_file_command(dir: &std::path::Path, name: &str, contents: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    if cfg!(windows) {
        format!("type {}", path.to_string_lossy())
    } else {
        format!("cat {}", path.to_string_lossy())
    }
}

fn assert_prompt_hook_part(block: &ContentBlock, expected: &str) {
    let ContentBlock::Text { text, meta } = block else {
        panic!("hook output must be a text part: {block:?}");
    };
    assert_eq!(
        text,
        &format!("<hook_result hook_event=\"UserPromptSubmit\">\n{expected}\n</hook_result>")
    );
    let meta = meta.as_ref().expect("hook part carries provenance");
    assert_eq!(
        meta.get("contentType").and_then(|value| value.as_str()),
        Some("text/xml")
    );
    assert_eq!(
        meta.get("source").and_then(|value| value.as_str()),
        Some("user prompt submit hook")
    );
}

fn recorded_prompt(requests: &[Vec<LLMMessage>]) -> &LLMMessage {
    requests
        .iter()
        .flat_map(|request| request.iter())
        .rev()
        .find(|message| message.role == "user" && message.content == "Hello!")
        .expect("the model request carries the submitted prompt")
}

#[tokio::test]
async fn user_prompt_submit_exit_two_blocks_before_model_and_returns_reason() {
    let llm = PromptRecordingLlm::new();
    let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));
    let guard = prompt_hook_guard(&[&stop_veto_command("no profanity")]);
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = events.clone();
    guard.with_hook_result(Arc::new(move |_meta, event, content, blocked| {
        log.lock()
            .unwrap()
            .push((event.to_string(), content.to_string(), blocked));
    }));

    let turn = run_turn(
        prompt_hook_input(&llm, guard, "prompt-hook-exit-two"),
        &callbacks,
    )
    .await
    .expect("a blocked prompt is a completed turn, not a transport error");

    assert_eq!(
        turn.steps, 0,
        "the blocked prompt must not enter the step loop"
    );
    assert_eq!(
        llm.call_count(),
        0,
        "the blocked prompt must not call the model"
    );
    let blocked = turn
        .messages
        .last()
        .expect("the block is visible in history");
    assert_eq!(blocked.role, "assistant");
    assert_eq!(
        blocked.content,
        "<hook_result hook_event=\"UserPromptSubmit\">\nno profanity\n</hook_result>"
    );
    assert_eq!(
        blocked.origin,
        Some(serde_json::json!({
            "kind": "hook_result",
            "event": "UserPromptSubmit",
            "blocked": true,
        }))
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[("UserPromptSubmit".into(), "no profanity".into(), true)]
    );
}

#[tokio::test]
async fn user_prompt_submit_json_deny_prefers_message_over_reason() {
    let dir = tempfile::tempdir().unwrap();
    let command = stdout_file_command(
        dir.path(),
        "prompt-deny.json",
        r#"{"message":"friendly denial","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"technical reason"}}"#,
    );
    let llm = PromptRecordingLlm::new();
    let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));

    let turn = run_turn(
        prompt_hook_input(
            &llm,
            prompt_hook_guard(&[command.as_str()]),
            "prompt-hook-json-deny",
        ),
        &callbacks,
    )
    .await
    .expect("a structured denial resolves as a blocked turn");

    assert_eq!(turn.steps, 0);
    assert_eq!(llm.call_count(), 0);
    assert_eq!(
        turn.messages.last().map(|message| message.content.as_str()),
        Some("<hook_result hook_event=\"UserPromptSubmit\">\nfriendly denial\n</hook_result>")
    );
}

#[tokio::test]
async fn user_prompt_submit_appends_plain_stdout_in_hook_order() {
    let llm = PromptRecordingLlm::new();
    let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));
    let guard = prompt_hook_guard(&["echo first note", "echo second note"]);
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = events.clone();
    guard.with_hook_result(Arc::new(move |_meta, event, content, blocked| {
        log.lock()
            .unwrap()
            .push((event.to_string(), content.to_string(), blocked));
    }));

    let turn = run_turn(
        prompt_hook_input(&llm, guard, "prompt-hook-plain-append"),
        &callbacks,
    )
    .await
    .expect("plain hook output does not block the prompt");

    assert_eq!(turn.steps, 1);
    assert_eq!(llm.call_count(), 1);
    let requests = llm.requests.lock().unwrap();
    let prompt = recorded_prompt(&requests);
    assert_eq!(prompt.content, "Hello!");
    assert_eq!(
        prompt.blocks.len(),
        3,
        "two hook parts plus the original prompt must reach the model: {prompt:?}"
    );
    assert_prompt_hook_part(&prompt.blocks[0], "first note");
    assert_prompt_hook_part(&prompt.blocks[1], "second note");
    assert_eq!(
        prompt.blocks[2],
        ContentBlock::Text {
            text: "Hello!".into(),
            meta: None,
        }
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            ("UserPromptSubmit".into(), "first note".into(), false),
            ("UserPromptSubmit".into(), "second note".into(), false),
        ]
    );
}

#[tokio::test]
async fn user_prompt_submit_structured_output_and_invalid_json_follow_upstream_append_rules() {
    let dir = tempfile::tempdir().unwrap();
    let top_level = stdout_file_command(
        dir.path(),
        "top-message.json",
        r#"{"message":"top-level note"}"#,
    );
    let nested = stdout_file_command(
        dir.path(),
        "nested-message.json",
        r#"{"hookSpecificOutput":{"message":"nested note"}}"#,
    );
    let no_message = stdout_file_command(dir.path(), "continue.json", r#"{"continue":true}"#);
    let llm = PromptRecordingLlm::new();
    let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));
    let guard = prompt_hook_guard(&[
        top_level.as_str(),
        nested.as_str(),
        no_message.as_str(),
        "echo raw fallback",
    ]);

    let turn = run_turn(
        prompt_hook_input(&llm, guard, "prompt-hook-structured-append"),
        &callbacks,
    )
    .await
    .expect("append-only structured output does not block");

    assert_eq!(turn.steps, 1);
    assert_eq!(llm.call_count(), 1);
    let requests = llm.requests.lock().unwrap();
    let prompt = recorded_prompt(&requests);
    assert_eq!(
        prompt.blocks.len(),
        4,
        "continue-only JSON adds nothing: {prompt:?}"
    );
    assert_prompt_hook_part(&prompt.blocks[0], "top-level note");
    assert_prompt_hook_part(&prompt.blocks[1], "nested note");
    assert_prompt_hook_part(&prompt.blocks[2], "raw fallback");
}

#[tokio::test]
async fn user_prompt_submit_nonzero_exit_and_timeout_allow_without_appending() {
    let nonzero = if cfg!(windows) {
        "echo must not append & exit /b 1"
    } else {
        "echo must not append; exit 1"
    };
    let sleeper = if cfg!(windows) {
        "%SystemRoot%\\System32\\ping.exe -n 3 127.0.0.1 >nul"
    } else {
        "sleep 3"
    };
    let hook = |command: &str, timeout| crate::permission::HookDef {
        event: "UserPromptSubmit".into(),
        matcher: String::new(),
        command: command.into(),
        timeout: Some(timeout),
        cwd: None,
        env: None,
    };
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        hook(nonzero, 5),
        hook(sleeper, 1),
        hook("echo still allowed", 5),
    ]));
    let llm = PromptRecordingLlm::new();
    let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));

    let turn = run_turn(
        prompt_hook_input(&llm, guard, "prompt-hook-failures"),
        &callbacks,
    )
    .await
    .expect("hook execution failures allow the prompt");

    assert_eq!(turn.steps, 1, "timeout and non-zero exits must not block");
    assert_eq!(llm.call_count(), 1);
    let requests = llm.requests.lock().unwrap();
    let prompt = recorded_prompt(&requests);
    assert_eq!(
        prompt.blocks.len(),
        2,
        "only the successful plain-text hook appends: {prompt:?}"
    );
    assert_prompt_hook_part(&prompt.blocks[0], "still allowed");
}

/// A text-finished turn trips a matching Stop hook: the veto text rides
/// `TurnResult.stop_hook_continuation` while the turn itself still ends
/// `EndTurn` — the host owns the re-prompt.
#[tokio::test]
async fn test_stop_hook_veto_surfaces_continuation() {
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
        turn_id: "test-turn-stop-veto".into(),
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
        hook_guard: Some(stop_hook_guard()),
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let turn = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(turn.stop_hook_continuation.as_deref(), Some("keep going"));
}

/// An allowing Stop hook (exit 0, plain stdout) leaves the turn alone.
#[tokio::test]
async fn test_stop_hook_allow_leaves_no_continuation() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        crate::permission::HookDef {
            event: "Stop".into(),
            matcher: String::new(),
            command: "echo all good".into(),
            timeout: Some(10),
            cwd: None,
            env: None,
        },
    ]));

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-turn-stop-allow".into(),
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
        hook_guard: Some(guard),
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let turn = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(turn.stop_hook_continuation, None);
}

/// Drive a text-finished turn whose only response carries
/// `finish_reason`, with the vetoing [`stop_hook_guard`] installed, and
/// hand back the stop reason plus whatever the Stop hook contributed.
/// A veto text on `stop_hook_continuation` is the dispatch marker: the
/// engine never self-continues, so the text only lands there when the
/// hook actually ran.
async fn stop_hook_outcome(
    turn_id: &str,
    finish_reason: &str,
) -> (LoopTurnStopReason, Option<String>) {
    let llm = FixedResponseLlm {
        finish_reason: finish_reason.to_string(),
        content: "hello".into(),
    };
    let callbacks = rpc_callbacks(Arc::new(RpcServer::new()));
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
            hook_guard: Some(stop_hook_guard()),
            media: None,
            media_dropped: None,
            media_recovery: None,
            toolset: None,
        },
        &callbacks,
    )
    .await
    .unwrap();
    (turn.stop_reason, turn.stop_hook_continuation.clone())
}

/// A filtered finish skips the Stop-hook dispatch, and the guard has to
/// recognise the *whole* filter vocabulary for that to hold: v2 latches
/// `ProviderFilteredError` after `runAfterStep` and ignores
/// `hookStopTurn` (`loopService.ts:877-882`), so a veto must not turn a
/// Filtered turn into a continuation. The canonical `filtered` is the one
/// v2 carries; a single-value guard naming only OpenAI's `content_filter`
/// let it fall through to a continuation.
#[tokio::test]
async fn every_filter_spelling_skips_the_stop_hook_dispatch() {
    for filtered in [
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
        let (stop, continuation) =
            stop_hook_outcome(&format!("test-stop-hook-filter-{filtered}"), filtered).await;
        assert_eq!(
            continuation, None,
            "{filtered} must skip the Stop hook: a veto would continue the turn instead of ending it Filtered"
        );
        assert!(
            matches!(stop, LoopTurnStopReason::Filtered),
            "{filtered} must end the turn Filtered, got {stop:?}"
        );
    }
}

/// The opposite drift: the ordinary finishes still dispatch the Stop hook,
/// so the filter guard cannot be widened into "always skip".
#[tokio::test]
async fn an_ordinary_finish_still_dispatches_the_stop_hook() {
    for (finish_reason, expected) in [
        ("stop", LoopTurnStopReason::EndTurn),
        ("end_turn", LoopTurnStopReason::EndTurn),
        ("length", LoopTurnStopReason::MaxTokens),
    ] {
        let (stop, continuation) = stop_hook_outcome(
            &format!("test-stop-hook-ordinary-{finish_reason}"),
            finish_reason,
        )
        .await;
        assert_eq!(
            continuation.as_deref(),
            Some("keep going"),
            "{finish_reason} must still dispatch the Stop hook"
        );
        assert_eq!(
            stop, expected,
            "{finish_reason} mapped to the wrong stop reason"
        );
    }
}

/// `run_turn_continued` consumes a Stop veto inside the turn: the veto
/// text re-prompts the model under the same turn id, steps and token
/// usage accumulate across the iterations, and the returned continuation
/// is always consumed (`None`).
#[tokio::test]
async fn test_run_turn_continued_consumes_stop_veto() {
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
        turn_id: "test-turn-continued".into(),
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
        hook_guard: Some(stop_hook_guard()),
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let turn = run_turn_continued(input, &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(turn.stop_hook_continuation, None);
    // Two text finishes ran (PredictTestLlm reports 15 tokens each).
    assert_eq!(turn.steps, 2);
    assert_eq!(turn.usage.input_tokens, 20);
    assert_eq!(turn.usage.output_tokens, 10);
    assert_eq!(turn.usage.total_tokens, 30);
    let texts: Vec<&str> = turn.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("keep going")));
}

/// A model that keeps requesting tools until the step budget runs out
/// must end the turn as `MaxSteps` (v2 `MaxStepsExceededError`,
/// loopService.ts:841-843 → failed + interrupt_reason `max_steps`), not
/// as an indistinguishable `EndTurn` completion. Without the
/// exhaustion mapping this turn reported `EndTurn`.
#[tokio::test]
async fn test_max_steps_exhaustion_reports_max_steps() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-loop".into(),
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
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-max-steps-exhaustion".into(),
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
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::MaxSteps));
    assert_eq!(turn.steps, 2);
}

/// v2 `loopService.ts:1047-1050`: the step cap applies only when it is a
/// positive number. An explicit `0` — and an unset cap, which the engine
/// spells the same way — means "no cap". Clamping it to one step (the old
/// `max(1)`) failed every tool-bearing turn at the first call.
#[tokio::test]
async fn a_zero_step_cap_does_not_limit_the_turn() {
    fn call(id: &str, path: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": path }),
            extras: None,
            args_parse_error: None,
        }
    }
    let llm = SequencedLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        steps: vec![
            (vec![call("a", "/a.txt")], "tool_calls"),
            (vec![call("b", "/b.txt")], "tool_calls"),
            (Vec::new(), "stop"),
        ],
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
        turn_id: "test-zero-step-cap".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "go".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 0,
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
    assert_eq!(turn.steps, 3, "0 must not cap the turn at one step");
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
}
