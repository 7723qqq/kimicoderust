use super::super::*;
use super::common::*;
use super::*;
use std::sync::atomic::AtomicUsize;

/// Step 1 issues `[Agent, Bash(broken payload)]`; step 2 stops.
///
/// The pair is the shape that exposes the defect: the swarm gate removes the
/// `Agent` call from the batch, and the `Bash` call's refusal was recorded
/// against position 0. If the refusal vector is indexed positionally, the
/// surviving `Bash` reads `None` and runs with arguments that never parsed.
struct AgentAndBrokenBash {
    step: AtomicUsize,
}

impl LLM for AgentAndBrokenBash {
    fn system_prompt(&self) -> &str {
        "base"
    }

    fn model_name(&self) -> &str {
        "scripted"
    }

    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let usage = TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                ..Default::default()
            };
            if step > 0 {
                return Ok(LLMChatResponse {
                    content: "done".into(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage,
                    timing: None,
                });
            }
            Ok(LLMChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls: vec![
                    ToolCall {
                        id: "tc-agent".into(),
                        name: "Agent".into(),
                        arguments: serde_json::json!({ "prompt": "look around" }),
                        extras: None,
                        args_parse_error: None,
                    },
                    ToolCall {
                        id: "tc-bash".into(),
                        name: "Bash".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                        args_parse_error: Some(ArgsParseError {
                            raw: "{\"command\":".into(),
                            error: "EOF while parsing an object".into(),
                        }),
                    },
                ],
                finish_reason: Some("tool_calls".into()),
                usage,
                timing: None,
            })
        })
    }
}

/// v2 `preflightToolCall`'s refusal is recorded per call, and the swarm gate
/// removes a call from the batch — so the refusals must follow the *calls*, not
/// the positions they occupied before the removal.
///
/// With positional indexing the surviving `Bash` reads the removed `Agent`
/// call's slot (`None`) and is dispatched to the host with `{}` arguments,
/// which is exactly the fabrication the parse-failure preflight exists to
/// prevent.
#[tokio::test]
async fn a_removed_call_must_not_shift_the_refusals_of_the_surviving_calls() {
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let executed = executed.clone();
        RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |params| {
            let executed = executed.clone();
            Box::pin(async move {
                let name = params
                    .get("tool_name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<unnamed>")
                    .to_string();
                executed.lock().unwrap().push(name);
                let resp = ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: "ran".into(),
                    is_error: false,
                    note: None,
                };
                serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
            })
        });
    }

    let registry = crate::swarm::mode::swarm_mode_registry();
    assert!(
        registry.enter(
            crate::callbacks::MAIN_AGENT_ID,
            crate::swarm::mode::SwarmModeTrigger::Manual
        ),
        "the swarm mode must be active for the gate to remove the Agent call"
    );

    let llm = AgentAndBrokenBash {
        step: AtomicUsize::new(0),
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-preflight-order".into(),
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

    let result = run_turn(input, &callbacks)
        .await
        .expect("the turn completes");
    registry.exit(crate::callbacks::MAIN_AGENT_ID);

    let dispatched = executed.lock().unwrap().clone();
    assert!(
        !dispatched.iter().any(|name| name == "Bash"),
        "Bash must not be dispatched with arguments that never parsed: {dispatched:?}"
    );

    let bash_result = result
        .messages
        .iter()
        .find(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some("tc-bash")
        })
        .expect("the refused call still gets a tool result");
    assert!(
        bash_result.content.contains("not valid JSON"),
        "the refusal must land on the call it belongs to: {:?}",
        bash_result.content
    );
}

/// Step 1 issues one `Write` whose `path` is a number; step 2 stops.
struct NumberPathWrite {
    step: AtomicUsize,
}

impl LLM for NumberPathWrite {
    fn system_prompt(&self) -> &str {
        "base"
    }

    fn model_name(&self) -> &str {
        "scripted"
    }

    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            let usage = TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                ..Default::default()
            };
            if step > 0 {
                return Ok(LLMChatResponse {
                    content: "done".into(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage,
                    timing: None,
                });
            }
            Ok(LLMChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls: vec![ToolCall {
                    id: "tc-write".into(),
                    name: "Write".into(),
                    arguments: serde_json::json!({ "path": 123, "content": "x" }),
                    extras: None,
                    args_parse_error: None,
                }],
                finish_reason: Some("tool_calls".into()),
                usage,
                timing: None,
            })
        })
    }
}

/// v2 `validateExecutableToolArgs`: a call that decodes but does not satisfy the
/// tool's declared schema is refused before execution, with the failure as the
/// model-visible result.
///
/// The turn-start snapshot supplies the definition here because the stub host
/// answers `list_tools` with an empty table — the same shape a host without that
/// seam produces, which must not silently disable the gate.
#[tokio::test]
async fn a_schema_violating_call_is_refused_before_the_tool_runs() {
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let executed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let executed = executed.clone();
        RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, move |params| {
            let executed = executed.clone();
            Box::pin(async move {
                let name = params
                    .get("tool_name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<unnamed>")
                    .to_string();
                executed.lock().unwrap().push(name);
                let resp = ToolExecuteResponse {
                    delivery: None,
                    stop_turn: false,
                    content: "ran".into(),
                    is_error: false,
                    note: None,
                };
                serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
            })
        });
    }

    let write = crate::tools::core_tool_defs::core_tool_defs()
        .into_iter()
        .find(|definition| definition.name == "Write")
        .expect("the Write tool has a definition");

    let llm = NumberPathWrite {
        step: AtomicUsize::new(0),
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-schema-preflight".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "hi".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![write],
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
        .expect("the turn completes");

    let dispatched = executed.lock().unwrap().clone();
    assert!(
        !dispatched.iter().any(|name| name == "Write"),
        "a schema-violating call must not reach the tool: {dispatched:?}"
    );

    let write_result = result
        .messages
        .iter()
        .find(|message| {
            message.role == "tool" && message.tool_call_id.as_deref() == Some("tc-write")
        })
        .expect("the refused call still gets a tool result");
    assert!(
        write_result.content.contains("must be string"),
        "the schema failure must name the violation: {:?}",
        write_result.content
    );
}
