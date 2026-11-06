use super::super::*;
use super::common::*;
use super::*;

/// The injection pass must append the date-change reminder right before
/// the LLM call: the captured request contains a `<system-reminder>`
/// user message with the baseline date text.
#[tokio::test]
async fn test_injections_appended_before_llm_call() {
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
                    content: String::new(),
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

    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-injection".into(),
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

    let captured = captured.lock().unwrap();
    assert!(
        captured.len() >= 3,
        "system + user + at least the date injection"
    );
    assert_eq!(captured[0].role, "system");
    assert_eq!(captured[1].content, "hi");
    let date_injection = captured
        .iter()
        .find(|m| m.content.contains("Today's date is"));
    let date_injection = date_injection.expect("date reminder must be injected");
    assert_eq!(date_injection.role, "user");
    assert!(date_injection.content.starts_with("<system-reminder>\n"));
    assert!(date_injection.content.ends_with("\n</system-reminder>"));
}

/// The permission-mode reminders ride the same injection pass: entering
/// auto mode announces itself, a turn whose history already carries the
/// reminder does not repeat it, and leaving auto mode announces the exit.
#[tokio::test]
async fn test_permission_mode_reminders_follow_the_mode_transitions() {
    use crate::permission::PermissionMode;

    fn input<'a>(
        llm: &'a PredictTestLlm,
        messages: Vec<LLMMessage>,
        mode: PermissionMode,
    ) -> RunTurnInput<'a> {
        RunTurnInput {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "test-permission-mode".into(),
            llm,
            messages,
            tools: &[],
            tool_defs: vec![],
            max_steps: 5,
            max_attempts: None,
            max_context_tokens: None,
            compaction_max_attempts: None,
            permission_mode: Some(mode),
            goal: None,
            cancellation: None,
            hook_guard: None,
            media: None,
            media_dropped: None,
            media_recovery: None,
            toolset: None,
            previous_turn_aborted: false,
        }
    }

    fn count(messages: &[LLMMessage], marker: &str) -> usize {
        messages
            .iter()
            .filter(|message| message.content.contains(marker))
            .count()
    }

    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let prompt = |text: &str| LLMMessage {
        role: "user".into(),
        content: text.into(),
        ..Default::default()
    };

    let entered = run_turn(
        input(&llm, vec![prompt("hi")], PermissionMode::Auto),
        &callbacks,
    )
    .await
    .unwrap();
    assert_eq!(
        count(&entered.messages, "Auto permission mode is active."),
        1,
        "entering auto mode injects the enter reminder"
    );

    let mut history = entered.messages.clone();
    history.push(prompt("again"));
    let stayed = run_turn(input(&llm, history, PermissionMode::Auto), &callbacks)
        .await
        .unwrap();
    assert_eq!(
        count(&stayed.messages, "Auto permission mode is active."),
        1,
        "the reminder is not repeated while auto mode stays active"
    );

    let mut history = stayed.messages.clone();
    history.push(prompt("back to manual"));
    let exited = run_turn(input(&llm, history, PermissionMode::Manual), &callbacks)
        .await
        .unwrap();
    assert_eq!(
        count(
            &exited.messages,
            "Auto permission mode is no longer active."
        ),
        1,
        "leaving auto mode injects the exit reminder"
    );
}

/// v2 `todoReminder` reads `runtime.getState()` on every provider call, not
/// once when the turn starts. A write earlier in the same turn must therefore
/// be what the next eligible reminder renders.
#[tokio::test]
async fn test_todo_reminder_reads_state_updated_earlier_in_turn() {
    struct TodoSequenceLlm {
        call: AtomicU32,
        requests: Mutex<Vec<Vec<LLMMessage>>>,
    }
    impl LLM for TodoSequenceLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "todo-sequence-llm"
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
                let tool_calls = if call == 0 {
                    vec![ToolCall {
                        id: "todo-update".into(),
                        name: "TodoList".into(),
                        arguments: serde_json::json!({
                            "todos": [
                                { "title": "Initial first task", "status": "done" },
                                { "title": "Updated second task", "status": "in_progress" }
                            ]
                        }),
                        extras: None,
                    }]
                } else if call <= 10 {
                    vec![ToolCall {
                        id: format!("work-{call}"),
                        name: "Echo".into(),
                        arguments: serde_json::json!({ "step": call }),
                        extras: None,
                    }]
                } else {
                    Vec::new()
                };
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    finish_reason: Some(if call <= 10 { "tool_calls" } else { "stop" }.into()),
                    tool_calls,
                    usage: TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }

    let llm = TodoSequenceLlm {
        call: AtomicU32::new(0),
        requests: Mutex::new(Vec::new()),
    };
    let todo_state = Arc::new(Mutex::new(serde_json::json!([
        { "title": "Initial first task", "status": "in_progress" },
        { "title": "Initial second task", "status": "pending" }
    ])));
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let state_for_read = todo_state.clone();
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, move |params| {
        let state = state_for_read.lock().unwrap().clone();
        Box::pin(async move {
            let value = if params.get("domain").and_then(|v| v.as_str()) == Some("todo") {
                state
            } else {
                serde_json::Value::Null
            };
            serde_json::to_value(types::StateReadResponse { value })
                .map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let state_for_write = todo_state.clone();
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |params| {
        let request: types::ToolExecuteRequest = serde_json::from_value(params).unwrap();
        if request.tool_name == "TodoList" {
            *state_for_write.lock().unwrap() = request.arguments["todos"].clone();
        }
        Box::pin(async move {
            serde_json::to_value(ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "ok".into(),
                is_error: false,
                note: None,
            })
            .map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-todo-reminder-live-state".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "keep the todo list current".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![
            crate::turn_loop::types::ToolInfo {
                name: "TodoList".into(),
                description: "todo".into(),
                input_schema: serde_json::json!({}),
            },
            crate::turn_loop::types::ToolInfo {
                name: "Echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({}),
            },
        ],
        max_steps: 15,
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
    let final_request = requests.last().expect("the final LLM request");
    let reminder = final_request
        .iter()
        .find(|message| {
            message
                .content
                .contains("The todo list has not been updated in the last 10 steps")
        })
        .expect("the reminder fires ten assistant steps after the mid-turn write");
    assert!(
        reminder.content.contains("1/2 done")
            && reminder
                .content
                .contains("\"Updated second task\" still in_progress")
            && reminder
                .content
                .contains("2. [in_progress] Updated second task"),
        "the reminder must render the post-write state: {}",
        reminder.content
    );
}

// ── Telemetry emission tests (M1c `host/telemetry`) ─────────────────

/// The telemetry events must carry the host-injected context merged with
/// the engine-observed outcome, in v2's payload vocabulary — the host
/// forwards these to track2 verbatim, so a field drift here is a
/// dashboard drift.
#[tokio::test]
async fn test_run_turn_with_telemetry_emits_started_and_ended() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-telemetry-ok".into(),
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
    let telemetry = TelemetryContext {
        mode: "agent".into(),
        provider_type: "kimi".into(),
        protocol: "openai".into(),
        thinking_effort: Some("high".into()),
        enabled_plugins: Some("kimi-webbridge,plugin-b".into()),
    };

    let result = run_turn_with_telemetry(input, telemetry, &callbacks).await;
    assert!(result.is_ok());

    let events = events.lock().unwrap();
    let emitted: Vec<&serde_json::Value> =
        events.iter().filter(|e| e.get("event").is_some()).collect();
    assert_eq!(emitted.len(), 2, "started + ended, nothing else");
    assert_eq!(emitted[0]["event"], "turn_started");
    assert_eq!(emitted[0]["turn_id"], "test-telemetry-ok");
    assert_eq!(emitted[0]["mode"], "agent");
    assert_eq!(emitted[0]["provider_type"], "kimi");
    assert_eq!(emitted[0]["protocol"], "openai");
    assert_eq!(emitted[0]["thinking_effort"], "high");
    // v2 #3963: the enabled-plugin set rides both lifecycle events.
    assert_eq!(emitted[0]["enabled_plugins"], "kimi-webbridge,plugin-b");
    assert_eq!(emitted[1]["event"], "turn_ended");
    assert_eq!(emitted[1]["reason"], "completed");
    assert_eq!(emitted[1]["steps"], 1);
    assert!(emitted[1]["duration_ms"].is_u64());
    assert_eq!(emitted[1]["enabled_plugins"], "kimi-webbridge,plugin-b");
}

/// A context without a plugin snapshot omits the field entirely (v2 #3963:
/// absent means "no snapshot", an empty string means "known empty set").
#[tokio::test]
async fn test_run_turn_with_telemetry_omits_an_absent_plugin_snapshot() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-telemetry-no-plugins".into(),
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
    let telemetry = TelemetryContext {
        mode: "agent".into(),
        provider_type: "kimi".into(),
        protocol: "openai".into(),
        thinking_effort: None,
        enabled_plugins: None,
    };

    let result = run_turn_with_telemetry(input, telemetry, &callbacks).await;
    assert!(result.is_ok());

    let events = events.lock().unwrap();
    let emitted: Vec<&serde_json::Value> =
        events.iter().filter(|e| e.get("event").is_some()).collect();
    assert_eq!(emitted.len(), 2);
    for event in emitted {
        assert!(
            event.get("enabled_plugins").is_none(),
            "an absent snapshot must not serialize the field: {event}"
        );
    }
}

/// A Stop-hook veto is consumed transparently inside the telemetry
/// wrapper too: the turn still ends `completed` with the usual two
/// events, and the veto text plus the follow-up reply fold into the
/// returned message history.
#[tokio::test]
async fn test_run_turn_with_telemetry_consumes_stop_hook_continuation() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-telemetry-stop-hook".into(),
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
        hook_guard: Some(stop_hook_guard()),
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };
    let telemetry = TelemetryContext {
        mode: "agent".into(),
        provider_type: "kimi".into(),
        protocol: "openai".into(),
        thinking_effort: None,
        enabled_plugins: None,
    };

    let result = run_turn_with_telemetry(input, telemetry, &callbacks).await;
    assert!(result.is_ok());
    let turn = result.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));

    let events = events.lock().unwrap();
    let emitted: Vec<&serde_json::Value> =
        events.iter().filter(|e| e.get("event").is_some()).collect();
    assert_eq!(emitted.len(), 2, "started + ended, nothing else");
    assert_eq!(emitted[1]["event"], "turn_ended");
    assert_eq!(emitted[1]["reason"], "completed");

    // The veto re-prompt and the follow-up reply are in the history.
    let texts: Vec<&str> = turn.messages.iter().map(|m| m.content.as_str()).collect();
    assert!(texts.iter().any(|t| t.contains("keep going")));
}

/// An aborted turn ends as `cancelled` and additionally reports
/// `turn_interrupted` with the engine-side interrupt reason.
#[tokio::test]
async fn test_run_turn_with_telemetry_reports_cancellation_as_interrupted() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-telemetry-cancel".into(),
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
        cancellation: Some(cancel_flag),
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };
    let telemetry = TelemetryContext {
        mode: "agent".into(),
        provider_type: "kimi".into(),
        protocol: "openai".into(),
        thinking_effort: None,
        enabled_plugins: None,
    };

    let result = run_turn_with_telemetry(input, telemetry, &callbacks).await;
    assert!(matches!(
        result.unwrap().stop_reason,
        LoopTurnStopReason::Aborted
    ));

    let events = events.lock().unwrap();
    let emitted: Vec<&serde_json::Value> =
        events.iter().filter(|e| e.get("event").is_some()).collect();
    assert_eq!(emitted.len(), 3, "started + ended + interrupted");
    assert_eq!(emitted[1]["event"], "turn_ended");
    assert_eq!(emitted[1]["reason"], "cancelled");
    assert_eq!(emitted[1]["steps"], 0);
    assert_eq!(emitted[2]["event"], "turn_interrupted");
    assert_eq!(emitted[2]["at_step"], 0);
    assert_eq!(emitted[2]["interrupt_reason"], "aborted");
    assert!(
        emitted[2].get("thinking_effort").is_none(),
        "an absent context field must not be emitted"
    );
}

// ── Tool-table refresh tests (M1d `host/list_tools`) ────────────────

/// The engine must pull the host's current tool table before every LLM
/// call on native transports: a table captured at turn start goes stale
/// the moment the registry changes mid-turn (feature tools, MCP
/// reconnects), and the model would keep calling a tool that no longer
/// exists.
#[tokio::test]
async fn test_list_tools_refreshes_table_per_step() {
    struct ToolsRecordingLlm {
        call: AtomicU32,
        seen_tools: Mutex<Vec<Vec<String>>>,
    }
    impl LLM for ToolsRecordingLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "tools-recording-llm"
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
            self.seen_tools.lock().unwrap().push(
                params
                    .tools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>(),
            );
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

    let llm = ToolsRecordingLlm {
        call: AtomicU32::new(0),
        seen_tools: Mutex::new(Vec::new()),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
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
    let list_tools_calls = Arc::new(AtomicU32::new(0));
    let calls_for_handler = list_tools_calls.clone();
    RpcServer::register_arc(&server, types::methods::HOST_LIST_TOOLS, move |_params| {
        let call = calls_for_handler.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let name = if call == 0 { "fresh_a" } else { "fresh_b" };
            let resp = types::ListToolsResponse {
                tools: vec![crate::turn_loop::types::ToolInfo {
                    name: name.into(),
                    description: "fresh".into(),
                    input_schema: serde_json::json!({}),
                }],
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-list-tools-refresh".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        // The turn-start snapshot must never reach the LLM here: the
        // host answers list_tools with a fresh table on every step.
        tool_defs: vec![crate::turn_loop::types::ToolInfo {
            name: "snapshot".into(),
            description: "stale".into(),
            input_schema: serde_json::json!({}),
        }],
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
        list_tools_calls.load(Ordering::SeqCst),
        2,
        "one pull per step"
    );
    let seen = llm.seen_tools.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], vec!["fresh_a".to_string()]);
    assert_eq!(seen[1], vec!["fresh_b".to_string()]);
}

/// A host without the seam (or with a failing one) must not break the
/// turn: run_turn falls back to the turn-start snapshot.
#[tokio::test]
async fn test_list_tools_failure_falls_back_to_snapshot() {
    struct ToolsRecordingLlm {
        seen_tools: Mutex<Vec<Vec<String>>>,
    }
    impl LLM for ToolsRecordingLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "tools-recording-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            self.seen_tools.lock().unwrap().push(
                params
                    .tools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>(),
            );
            Box::pin(async move {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }

    let llm = ToolsRecordingLlm {
        seen_tools: Mutex::new(Vec::new()),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    // The seam is wired but the host answers with an error — the same
    // Err branch in run_turn as an unwired host, without the stdio
    // round-trip timeout a missing handler would cost. Registered after
    // the helper so it wins over the no-op default.
    RpcServer::register_arc(&server, types::methods::HOST_LIST_TOOLS, |_params| {
        Box::pin(async move { Err(JsonRpcError::method_not_found("host/list_tools")) })
    });

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-list-tools-fallback".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![crate::turn_loop::types::ToolInfo {
            name: "snapshot".into(),
            description: "stale".into(),
            input_schema: serde_json::json!({}),
        }],
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

    let seen = llm.seen_tools.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], vec!["snapshot".to_string()]);
}

/// Host-proxy mode rebuilds tools host-side per call; the engine must
/// not spend a round trip pulling a table it never uses.
#[tokio::test]
async fn test_list_tools_skipped_in_host_proxy_mode() {
    struct HostProxyLlm {
        seen_tools: Mutex<Vec<Vec<String>>>,
    }
    impl LLM for HostProxyLlm {
        fn system_prompt(&self) -> &str {
            "test"
        }
        fn model_name(&self) -> &str {
            "host-proxy-llm"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "host-proxy"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            self.seen_tools.lock().unwrap().push(
                params
                    .tools
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>(),
            );
            Box::pin(async move {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage::default(),
                    timing: None,
                })
            })
        }
    }

    let llm = HostProxyLlm {
        seen_tools: Mutex::new(Vec::new()),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let list_tools_calls = Arc::new(AtomicU32::new(0));
    let calls_for_handler = list_tools_calls.clone();
    RpcServer::register_arc(&server, types::methods::HOST_LIST_TOOLS, move |_params| {
        let calls = calls_for_handler.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            let resp = types::ListToolsResponse { tools: vec![] };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-list-tools-host-proxy".into(),
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
        list_tools_calls.load(Ordering::SeqCst),
        0,
        "host-proxy mode must not pull the engine-side tool table"
    );
}
