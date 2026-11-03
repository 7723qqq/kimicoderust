use super::*;
use crate::callbacks::RpcHostCallbacks;
use crate::rpc::server::RpcServer;
use crate::rpc::types::{self, JsonRpcError, TokenUsage, ToolExecuteResponse};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Helper: create an RpcHostCallbacks from an RpcServer.
///
/// Registers the per-step host seams (`list_tools`, `state_read`) with
/// no-op answers: with no local handler the server falls back to a stdio
/// round-trip that stalls for the full timeout, which used to cost every
/// native-transport test 30s per step.
fn rpc_callbacks(server: Arc<RpcServer>) -> Arc<dyn HostCallbacks> {
    RpcServer::register_arc(&server, types::methods::HOST_LIST_TOOLS, |_params| {
        Box::pin(async move {
            let resp = types::ListToolsResponse { tools: vec![] };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, |_params| {
        Box::pin(async move {
            let resp = types::StateReadResponse {
                value: serde_json::Value::Null,
            };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    Arc::new(RpcHostCallbacks { server })
}

/// HostCallbacks decorator that records emitted events so tests can
/// assert on fire-and-forget notifications (e.g. goal budget limits).
struct EventCapturingCallbacks {
    inner: Arc<dyn HostCallbacks>,
    events: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl EventCapturingCallbacks {
    fn new(inner: Arc<dyn HostCallbacks>) -> (Self, Arc<Mutex<Vec<serde_json::Value>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner,
                events: events.clone(),
            },
            events,
        )
    }
}

impl HostCallbacks for EventCapturingCallbacks {
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
        self.events.lock().unwrap().push(event.clone());
        self.inner.emit_event(event);
    }

    fn telemetry(&self, event: serde_json::Value) {
        self.events.lock().unwrap().push(event.clone());
        self.inner.telemetry(event);
    }
}

struct PredictTestLlm {
    system_prompt: String,
    model_name: String,
    return_tool_calls: bool,
    tool_responses: Vec<ToolCall>,
}

impl LLM for PredictTestLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let return_tc = self.return_tool_calls;
        let tcs = self.tool_responses.clone();
        Box::pin(async move {
            if return_tc {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: tcs,
                    finish_reason: Some("tool_calls".into()),
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                        ..Default::default()
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
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                        ..Default::default()
                    },
                    timing: None,
                })
            }
        })
    }
}

/// A model that answers with a scripted sequence of responses, one per
/// step, and repeats the last entry once the script runs out.
///
/// `PredictTestLlm` returns the same tool calls on every step, which cannot
/// express "the model reads the refusal and complies next time" — the shape
/// a veto needs to be tested in.
struct SequencedLlm {
    system_prompt: String,
    model_name: String,
    steps: Vec<(Vec<ToolCall>, &'static str)>,
}

impl LLM for SequencedLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
    fn model_name(&self) -> &str {
        &self.model_name
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        // Every completed step leaves exactly one assistant message behind,
        // so that count is how far the script has advanced. Counting the
        // tool results instead would skip a step whose batch was vetoed (no
        // tool ran, but the refusal is still a tool message).
        let completed = params
            .messages
            .iter()
            .filter(|m| m.role == "assistant")
            .count();
        let index = completed.min(self.steps.len().saturating_sub(1));
        let (tool_calls, finish_reason) = self.steps[index].clone();
        Box::pin(async move {
            Ok(LLMChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls,
                finish_reason: Some(finish_reason.into()),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
                timing: None,
            })
        })
    }
}

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
                    },
                    ToolCall {
                        id: "bash-1".into(),
                        name: "Bash".into(),
                        arguments: serde_json::json!({ "command": "echo hi" }),
                        extras: None,
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
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
}

/// A model that never complies must not spin: the step budget is the same
/// bound v2 relies on, and running out of it fails the turn as
/// `max_steps` rather than answering as if the work had been done.
#[tokio::test]
async fn a_model_that_never_complies_runs_out_of_steps() {
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
                },
                ToolCall {
                    id: "bash-1".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({ "command": "echo hi" }),
                    extras: None,
                },
            ],
            "tool_calls",
        )],
    };

    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
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
/// `RunTurnInput.hook_guard` and must not disturb the turn: they fire
/// fire-and-forget at the turn head (and before any compaction), so a
/// turn carrying them completes exactly like one without.
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
        command: "echo hook-fired".into(),
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

/// Portable `exit 2` with a reason on stderr for Stop-hook tests.
fn stop_veto_command(reason: &str) -> String {
    if cfg!(windows) {
        format!("echo {reason} 1>&2 & exit /b 2")
    } else {
        format!(
            "echo '{reason}' >&2; exit 2",
            reason = reason.replace('\'', "'\\''")
        )
    }
}

fn stop_hook_guard() -> Arc<crate::tools::external_hooks::HookGuard> {
    Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        crate::permission::HookDef {
            event: "Stop".into(),
            matcher: String::new(),
            command: stop_veto_command("keep going"),
            timeout: Some(10),
            cwd: None,
            env: None,
        },
    ]))
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
/// `display` would make hosts render a card for a plain tool.
#[tokio::test]
async fn a_tool_without_a_display_ships_none() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-plain".into(),
            name: "WebSearch".into(),
            arguments: serde_json::json!({ "query": "x" }),
            extras: None,
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
                && event.get("tool_name").and_then(|t| t.as_str()) == Some("WebSearch")
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
        }
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

/// An LLM that answers every request with one fixed response, so a test can
/// choose the finish reason and whether the answer carries content.
struct FixedResponseLlm {
    finish_reason: String,
    content: String,
}

impl LLM for FixedResponseLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }

    fn model_name(&self) -> &str {
        "test-model"
    }

    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let finish_reason = self.finish_reason.clone();
        let content = self.content.clone();
        Box::pin(async move {
            Ok(LLMChatResponse {
                content,
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some(finish_reason),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
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

/// Callbacks wrapper recording `set_turn_goal` bindings.
struct GoalBindingCallbacks {
    inner: Arc<dyn HostCallbacks>,
    #[allow(clippy::type_complexity)]
    bound: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

impl HostCallbacks for GoalBindingCallbacks {
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
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        self.inner.check_permission(request)
    }

    fn set_turn_goal(&self, turn_id: &str, goal_id: Option<&str>) {
        self.bound
            .lock()
            .unwrap()
            .push((turn_id.to_string(), goal_id.map(str::to_string)));
    }
}

#[tokio::test]
async fn test_turn_binds_start_goal_for_stale_check() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let base = rpc_callbacks(server.clone());
    let bound = Arc::new(Mutex::new(Vec::new()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(GoalBindingCallbacks {
        inner: base.clone(),
        bound: bound.clone(),
    });

    let goal = crate::turn_loop::types::GoalContext {
        goal_id: "g-1".into(),
        objective: "objective".into(),
        status: crate::turn_loop::types::GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        wall_clock_budget_ms: None,
        tokens_used: 0,
        turns_used: 0,
        wall_clock_ms: 0,
        updated_at: 0,
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-goal".into(),
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
        goal: Some(goal),
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(result.is_ok());
    assert_eq!(
        bound.lock().unwrap().as_slice(),
        &[("turn-goal".to_string(), Some("g-1".to_string()))],
        "run_turn must bind the turn-start goal for the stale gate"
    );
}

#[tokio::test]
async fn test_turn_binds_no_goal_when_goalless() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: false,
        tool_responses: vec![],
    };
    let server = Arc::new(RpcServer::new());
    let base = rpc_callbacks(server.clone());
    let bound = Arc::new(Mutex::new(Vec::new()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(GoalBindingCallbacks {
        inner: base.clone(),
        bound: bound.clone(),
    });
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-goal-free".into(),
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
    assert_eq!(
        bound.lock().unwrap().as_slice(),
        &[("turn-goal-free".to_string(), None)],
        "a goal-less turn binds None so the stale gate skips it"
    );
}

// ── empty provider responses, end to end (v2 empty_response) ───────────

/// A native-transport LLM that answers the first `empty_answers` calls
/// with a 200 carrying no content, then a real answer.
///
/// `transport()` is `native-http` on purpose. The empty-response guard arms
/// only for the transport that actually assembles the assistant message
/// (see `turn_step::empty_response_retryable`) — the host-proxy leg owns its
/// own transcript, where an empty body is a legitimate answer — so a stub
/// left on the default `"custom"` would bypass the very thing under test and
/// make this test pass for the wrong reason.
struct EmptyThenAnswerLlm {
    empty_answers: u32,
    calls: AtomicU32,
    answer: String,
}

impl LLM for EmptyThenAnswerLlm {
    fn system_prompt(&self) -> &str {
        "You are helpful."
    }
    fn model_name(&self) -> &str {
        "empty-then"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn transport(&self) -> &'static str {
        "native-http"
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let empty_answers = self.empty_answers;
        let answer = self.answer.clone();
        Box::pin(async move {
            let content = if call < empty_answers {
                String::new()
            } else {
                answer
            };
            Ok(LLMChatResponse {
                content,
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
                timing: None,
            })
        })
    }
}

/// A provider that behaves the way a real one does about a duplicated
/// `tool_call_id`: it reads the request, and rejects its *shape* with a 400
/// naming the problem. Only a request whose duplicates are gone is
/// accepted. This is the only way to prove the strict retry is reachable
/// from the real loop rather than merely callable.
struct RejectsDuplicateToolCallsLlm {
    calls: AtomicU32,
    /// Tool-call ids seen per request, recorded so the test can assert the
    /// second request was actually clean rather than accidentally accepted.
    seen_per_call: std::sync::Mutex<Vec<Vec<String>>>,
}

impl RejectsDuplicateToolCallsLlm {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
            seen_per_call: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl LLM for RejectsDuplicateToolCallsLlm {
    fn system_prompt(&self) -> &str {
        "You are helpful."
    }
    fn model_name(&self) -> &str {
        "strict-shape"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn transport(&self) -> &'static str {
        "native-http"
    }
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut seen: Vec<String> = Vec::new();
        let mut duplicate: Option<String> = None;
        for message in params.messages.iter() {
            for tool_call in &message.tool_calls {
                if seen.contains(&tool_call.id) {
                    duplicate = Some(tool_call.id.clone());
                }
                seen.push(tool_call.id.clone());
            }
        }
        self.seen_per_call.lock().unwrap().push(seen);
        Box::pin(async move {
            // The verdict is a function of the request alone. A mock that
            // also failed "the first time" would pass this test whether or
            // not the retry actually removed the duplicate.
            if let Some(id) = duplicate {
                return Err(Box::new(std::io::Error::other(format!(
                    "llm http status 400: messages: tool_use ids must be unique (duplicate {id})"
                )))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(LLMChatResponse {
                content: "recovered".to_string(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
                timing: None,
            })
        })
    }
}

/// End-to-end through the real loop: a history carrying a duplicated
/// `tool_call_id` is rejected by the provider, and the turn recovers on the
/// strict resend instead of failing the user's request. Asserts the second
/// request really was clean — otherwise this would pass on a mock that
/// simply stopped complaining.
#[tokio::test]
async fn a_turn_resends_with_the_strict_projection_after_a_shape_rejection() {
    let llm = RejectsDuplicateToolCallsLlm::new();
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-strict".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "Run it twice.".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                }],
                ..Default::default()
            },
            LLMMessage {
                role: "tool".into(),
                content: "done".into(),
                tool_call_id: Some("call_1".into()),
                ..Default::default()
            },
            // The replayed step: the same call declared a second time.
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "Bash".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                }],
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

    let result = run_turn(input, &callbacks)
        .await
        .expect("the strict resend must recover the turn");

    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        2,
        "one rejection, one resend"
    );
    let seen = llm.seen_per_call.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].iter().filter(|id| *id == "call_1").count(),
        2,
        "the first request carried the duplicate"
    );
    assert_eq!(
        seen[1].iter().filter(|id| *id == "call_1").count(),
        1,
        "the resend carried exactly one declaration"
    );
    assert!(
        result
            .messages
            .iter()
            .rev()
            .any(|m| m.role == "assistant" && m.content == "recovered"),
        "the turn ends on the recovered answer"
    );
}

/// rather than the retry helper in isolation: a provider that answers 200
/// with nothing used to end the turn as a success, so the model stopped
/// early and nothing — transcript, telemetry, UI — could tell it from a
/// real reply.
#[tokio::test]
async fn a_turn_retries_through_empty_provider_responses_and_answers() {
    let llm = EmptyThenAnswerLlm {
        empty_answers: 2,
        calls: AtomicU32::new(0),
        answer: "the real answer".into(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-empty".into(),
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

    let result = run_turn(input, &callbacks)
        .await
        .expect("the turn recovers from the empty responses");

    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        3,
        "two empty answers were retried, the third carried content"
    );
    assert_eq!(
        result.llm_retries, 2,
        "the turn must record the retries, not silently absorb them"
    );
    assert_eq!(result.steps, 1, "recovery costs attempts, not steps");
    let final_assistant = result
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .expect("the turn ends on an assistant message");
    assert_eq!(
        final_assistant.content, "the real answer",
        "the recovered answer is what the model reads"
    );
}

/// The other half: a provider that never answers must **fail** rather than
/// book an empty turn as a success. Without this, the retry could mask a
/// hard failure as a very slow success.
#[tokio::test]
async fn a_turn_fails_when_every_response_is_empty() {
    let llm = EmptyThenAnswerLlm {
        empty_answers: u32::MAX,
        calls: AtomicU32::new(0),
        answer: "never used".into(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "turn-empty-forever".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "Hello!".into(),
            ..Default::default()
        }],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: Some(2),
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

    let err = run_turn(input, &callbacks)
        .await
        .expect_err("a provider that never answers must not read as success");
    assert_eq!(
        llm.calls.load(Ordering::SeqCst),
        2,
        "the attempt budget is spent, and no more"
    );
    assert!(
        err.to_string().contains("empty response"),
        "the failure must name the cause: {err}"
    );
}

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
    use super::turn_stop_reason_from_finish;
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

// ── Tool-call dedup (v2 `toolDedupeService` mirror, G-6 #2) ─────────

/// A same-step repeat of an identical call never executes: the host
/// runs the original once, and the repeat receives the same result.
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
                            },
                            // Same key, different call id — the repeat.
                            ToolCall {
                                id: "tc2".into(),
                                name: "read".into(),
                                arguments: serde_json::json!({"path": "/a.txt"}),
                                extras: None,
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
    let input = RunTurnInput {
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
    // Both calls keep their result messages, with identical content.
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
}

/// Calls with non-native names are exempt from engine-side dedup: they
/// run on the host, whose own dedup service stays authoritative.
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
                            },
                            ToolCall {
                                id: "tc2".into(),
                                name: "echo".into(),
                                arguments: serde_json::json!({"text": "hello"}),
                                extras: None,
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

/// Cross-step repeats earn the escalating reminders, and the 12th
/// consecutive identical call force-stops the turn as `completed`.
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
    let input = RunTurnInput {
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
        // Generous ceiling: the force stop, not max_steps, must end it.
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
    let result = run_turn(input, &callbacks).await.unwrap();

    assert_eq!(
        result.steps, 13,
        "force stop at the 12th repeat + 1 handoff step"
    );
    assert!(matches!(
        result.stop_reason,
        LoopTurnStopReason::RepeatBreaker
    ));
    assert!(result.messages.iter().any(|m| {
        m.content
            .contains("The repeat breaker has stopped the turn")
    }));
    let tool_msgs: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 12);
    // Streak 1 and 2 stay untouched; 3 earns reminder 1.
    assert_eq!(tool_msgs[0].content, "stub");
    assert_eq!(tool_msgs[1].content, "stub");
    assert!(
        tool_msgs[2]
            .content
            .contains("repeated several times in a row")
    );
    // Streak 5 embeds the count (reminder 2).
    assert!(tool_msgs[4].content.contains("issued 5 times in a row"));
    // Streak 8 and 12 carry the final-response reminder; 12 stops the turn.
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
}

// ── Turn telemetry counters ─────────────────────────────────────────

/// The turn result carries the LLM retry count: the transient failure
/// on the first chat attempt counts as one retry.
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

    let input = RunTurnInput {
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
}

/// `RunTurnInput::max_attempts` must cap the retry loop (v2
/// `loopControl.maxAttemptsPerStep`, stepRetryService.ts:138-146): a
/// permanently failing LLM is called exactly `max_attempts` times before
/// the turn errors out. The engine default (no host override) is 10 —
/// `DEFAULT_MAX_RETRY_ATTEMPTS`, pinned by the retry.rs unit test.
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

// ── Goal budget tests ───────────────────────────────────────────────

/// v2 binds a turn only to a goal that was active when the turn started: a
/// paused or blocked goal is ordinary chat, with no per-step budget gate and
/// no goal steering. (The goal reminder injection still tells the model the
/// goal exists and is not being pursued.)
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
        };
        let input = RunTurnInput {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            previous_turn_aborted: false,
            turn_id: turn_id.into(),
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

/// A goal whose token budget is already exhausted (tokens_used >= budget)
/// must stop with `BudgetLimited` before running any step.
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
        // Already used 100 tokens — at the budget limit.
        tokens_used: 100,
        turns_used: 0,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-budget-tokens".into(),
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

/// A goal whose turn budget is already exhausted must stop with
/// `BudgetLimited`.
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
        // Already used 3 turns — at the limit.
        turns_used: 3,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-budget-turns".into(),
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

/// An active goal with remaining budget should let the turn proceed
/// normally and complete with `EndTurn`.
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
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-active-goal".into(),
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

/// When a goal has a small wall-clock budget and the step takes longer,
/// the active deadline timer must fire and cancel the turn with `BudgetLimited`.
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
        wall_clock_budget_ms: Some(50), // 50ms budget vs 1000ms LLM
        wall_clock_ms: 0,
        updated_at: 0,
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-deadline-turn".into(),
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

// ── Cancellation tests ──────────────────────────────────────────────

/// When the cancellation flag is set before the turn starts, the loop
/// must abort immediately with `Aborted` and 0 steps.
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-cancel-before".into(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::Aborted));
    assert_eq!(result.steps, 0);
}

/// HostCallbacks decorator that raises the cancellation flag as a tool
/// call runs, so the scheduler observes the cancel inside a batch.
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

/// A cancel landing while tools execute ends the turn as `Aborted`: the
/// host asked to stop, so the turn must not surface a hard error.
#[tokio::test]
async fn test_cancellation_during_tool_execution_aborts() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-cancel".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "/a.txt" }),
            extras: None,
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-cancel-during-tools".into(),
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

/// The grace's whole point, exercised through the **real turn loop**: the
/// tool above answers instantly, so it now wins the abort race and keeps its
/// result. That test's assertions did not change, so it cannot tell the two
/// behaviours apart — this one can, by giving the tool a delay that lands
/// inside the 2 s window and then checking what the model actually reads.
///
/// Before the grace, a cancel discarded the future the instant the flag was
/// seen, so this result was replaced by `abortedToolOutput` and the model
/// was told the user had interrupted a tool that had in fact succeeded.
#[tokio::test]
async fn a_tool_finishing_inside_the_abort_grace_keeps_its_result() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-grace".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "/a.txt" }),
            extras: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
        Box::pin(async move {
            // Lands comfortably inside the 2 s grace window.
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-grace-keeps-result".into(),
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

    // The turn is still cancelled — the user did press Ctrl-C — but the
    // call that finished inside the window keeps its answer.
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

/// A tool result carrying `stop_turn: true` ends the turn as `EndTurn`
/// (v2 `completed`, loopService.ts:2117-2119 via engineOverride.ts:58):
/// no continuation step runs even though the scripted LLM would keep
/// issuing tool calls. Without the stop-turn check this turn would burn
/// all 5 max_steps; the `steps == 1` assertion is the discriminator.
#[tokio::test]
async fn test_tool_result_stop_turn_ends_turn_completed() {
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "tc-stop".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "/a.txt" }),
            extras: None,
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-stop-turn".into(),
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

    let result = run_turn(input, &callbacks)
        .await
        .expect("stop_turn must end the turn cleanly");
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(result.steps, 1);
}

/// When the cancellation flag is cleared, the turn runs normally.
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-cancel-clear".into(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));
    assert_eq!(result.steps, 1);
}

// ── Goal steering text injection test ────────────────────────────────

/// Verify that when a goal is active, the system prompt is enriched
/// with steering text containing the objective and budget info.
#[tokio::test]
async fn test_goal_steering_injected_into_system_prompt() {
    use std::sync::Mutex;

    // LLM that captures the messages it receives so we can inspect
    // the system prompt.
    struct CaptureLlm {
        captured_system: Arc<Mutex<Option<String>>>,
        call: AtomicU32,
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
            let captured = self.captured_system.clone();
            let call = self.call.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 && !params.messages.is_empty() {
                    *captured.lock().unwrap() = Some(params.messages[0].content.clone());
                }
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_tokens: 5,
                        output_tokens: 3,
                        total_tokens: 8,
                        ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let llm = CaptureLlm {
        captured_system: captured.clone(),
        call: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());

    let goal = GoalContext {
        goal_id: "g-steer".into(),
        objective: "Write a hello world program".into(),
        status: GoalStatus::Active,
        token_budget: Some(1000),
        turn_budget: Some(10),
        tokens_used: 100,
        turns_used: 1,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-steering".into(),
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

    let captured = captured
        .lock()
        .unwrap()
        .clone()
        .expect("system prompt was captured");
    assert!(
        captured.contains("base prompt"),
        "should contain base system prompt"
    );
    assert!(
        captured.contains("Write a hello world program"),
        "should contain objective"
    );
    assert!(captured.contains("Goal"), "should contain Goal header");
    assert!(captured.contains("Budgets:"), "should contain budget info");
    assert!(captured.contains("1000"), "should mention token budget");
    assert!(captured.contains("10"), "should mention turn budget");
}

// ── Max steps enforcement test ──────────────────────────────────────

/// When the LLM always returns tool calls, the loop must stop at
/// max_steps with EndTurn (the loop exits after the max_steps
/// iterations).
#[tokio::test]
async fn test_max_steps_enforcement() {
    // LLM that always returns a tool call — never stops on its own.
    let llm = PredictTestLlm {
        system_prompt: "You are helpful.".into(),
        model_name: "test-model".into(),
        return_tool_calls: true,
        tool_responses: vec![ToolCall {
            id: "loop".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "/x"}),
            extras: None,
        }],
    };

    let server = Arc::new(RpcServer::new());
    RpcServer::register_arc(&server, types::methods::HOST_EXECUTE_TOOL, |_params| {
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
        turn_id: "test-max-steps".into(),
        llm: &llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "loop".into(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    // The loop runs max_steps iterations; steps = max_steps.
    assert_eq!(result.steps, 3, "should stop at max_steps");
}

// ── render_goal_steering tests ──────────────────────────────────────

#[test]
fn test_render_goal_steering_basic() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Write tests".into(),
        status: GoalStatus::Active,
        token_budget: Some(1000),
        turn_budget: Some(10),
        tokens_used: 100,
        turns_used: 1,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
    };
    let text = render_goal_steering(&goal, 50, 1, 0);
    assert!(text.contains("Write tests"), "should contain objective");
    assert!(text.contains("Goal"), "should contain Goal header");
    assert!(text.contains("Budgets:"), "should contain budget section");
    assert!(text.contains("1000"), "should mention token budget");
    assert!(
        text.contains("within budget"),
        "should say within budget when low"
    );
}

#[test]
fn test_render_goal_steering_wall_clock_budget() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Finish".into(),
        status: GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        wall_clock_budget_ms: Some(10_000),
        wall_clock_ms: 5000,
        tokens_used: 0,
        turns_used: 0,
        updated_at: 0,
    };
    let text = render_goal_steering(&goal, 0, 0, 500);
    assert!(
        text.contains("time 5s/10s (remaining 4s)"),
        "should show wall-clock budget"
    );
    assert!(
        text.contains("elapsed"),
        "should mention elapsed time in progress"
    );
}

#[test]
fn test_format_elapsed() {
    assert_eq!(format_elapsed(500), "0s");
    assert_eq!(format_elapsed(59_000), "59s");
    assert_eq!(format_elapsed(65_000), "1m05s");
    assert_eq!(format_elapsed(3_600_000), "1h0m");
    assert_eq!(format_elapsed(7_200_000), "2h0m");
}

#[test]
fn test_render_goal_steering_near_limit() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Finish".into(),
        status: GoalStatus::Active,
        token_budget: Some(100),
        turn_budget: None,
        tokens_used: 80,
        turns_used: 0,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
    };
    // 80 + 10 = 90 / 100 = 0.9 >= 0.75 → should say "nearing"
    let text = render_goal_steering(&goal, 10, 0, 0);
    assert!(
        text.contains("nearing a budget"),
        "should warn about nearing budget"
    );
}

#[test]
fn test_render_goal_steering_no_budgets() {
    let goal = GoalContext {
        goal_id: "g".into(),
        objective: "Do thing".into(),
        status: GoalStatus::Active,
        token_budget: None,
        turn_budget: None,
        tokens_used: 0,
        turns_used: 0,
        wall_clock_budget_ms: None,
        wall_clock_ms: 0,
        updated_at: 0,
    };
    let text = render_goal_steering(&goal, 0, 0, 0);
    assert!(text.contains("Do thing"), "should contain objective");
    assert!(
        !text.contains("Budgets:"),
        "should not contain budget section when no budgets"
    );
}

// ── Context compaction wiring tests ────────────────────────────────

/// When the message history crosses the context trigger threshold, the
/// loop compacts it before the LLM call: the system prompt and the
/// most recent messages survive, the middle is replaced by a summary
/// placeholder.
#[tokio::test]
async fn test_context_compaction_before_llm_call() {
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
                    content: if params.messages[0]
                        .content
                        .contains("conversation summarizer")
                    {
                        "Earlier user and assistant discussed the task.".into()
                    } else {
                        String::new()
                    },
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

    // Default trigger threshold is 128k - 50k reserved = 81_072 tokens;
    // at 4 chars/token that is ~324k chars. Three 120k-char messages
    // (90k estimated tokens) cross it.
    let big = "x".repeat(120_000);
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));

    let captured = captured.lock().unwrap();
    // v2 compaction shape: system, the compacted range's head user input
    // (verbatim), the elision note, the prefixed summary, the continuation
    // note, the preserved recent user input, then the turn injections.
    assert_eq!(captured[0].role, "system");
    let summary_message = captured
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the prefixed summary must be present");
    assert!(
        summary_message
            .content
            .ends_with("Earlier user and assistant discussed the task."),
        "the generated summary must ride the compaction prefix"
    );
    assert!(
        captured.iter().any(|m| {
            m.content.contains("roughly") && m.content.contains("tokens in between were dropped")
        }),
        "the elision note must mark the omitted middle"
    );
    let continuation_count = captured
        .iter()
        .filter(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_CONTINUATION_TEXT)
        })
        .count();
    assert_eq!(
        continuation_count, 1,
        "exactly one continuation note, carried by the compacted shape itself"
    );
    assert!(
        captured.iter().any(|m| m.content == big),
        "most recent message preserved"
    );
    assert!(
        captured
            .iter()
            .any(|m| m.content.starts_with("<system-reminder>\n")
                && !m.content.contains("roughly")
                && !m.content.contains("compaction")),
        "injection appended after compaction"
    );
}

/// v2 `postProcessSummary` (fullCompactionService.ts:844): a compaction
/// in a session with todos appends the rendered list to the written
/// summary, so the todo state survives the fold that dropped the older
/// history. The default `host/state_read` answers Null for every domain;
/// the todo domain gets a real list here.
#[tokio::test]
async fn test_auto_compaction_summary_carries_the_todo_list() {
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
                    content: if params.messages[0]
                        .content
                        .contains("conversation summarizer")
                    {
                        "Earlier user and assistant discussed the task.".into()
                    } else {
                        String::new()
                    },
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

    let big = "x".repeat(120_000);
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    // Overwrite the no-op state_read (a HashMap insert, so this wins) and
    // answer only the todo domain — goal/plan keep the Null the step-head
    // refresh expects.
    RpcServer::register_arc(&server, types::methods::HOST_STATE_READ, |params| {
        Box::pin(async move {
            let value = if params.get("domain").and_then(|v| v.as_str()) == Some("todo") {
                serde_json::json!([
                    { "id": "t1", "title": "ship the fix", "status": "in_progress" }
                ])
            } else {
                serde_json::Value::Null
            };
            let resp = types::StateReadResponse { value };
            serde_json::to_value(&resp).map_err(|e| JsonRpcError::internal_error(e.to_string()))
        })
    });
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-todos".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: big.clone(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: big.clone(),
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

    let result = run_turn(input, &callbacks).await.unwrap();
    assert!(matches!(result.stop_reason, LoopTurnStopReason::EndTurn));

    let captured = captured.lock().unwrap();
    let summary_message = captured
        .iter()
        .find(|m| {
            m.content
                .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX)
        })
        .expect("the prefixed summary must be present");
    assert!(
        summary_message
            .content
            .contains("Earlier user and assistant discussed the task."),
        "the generated summary rides the compaction prefix"
    );
    assert!(
        summary_message.content.contains("## TODO List")
            && summary_message.content.contains("ship the fix"),
        "the summary carries the todo list (v2 postProcessSummary)"
    );
}

// ── Injection wiring tests ─────────────────────────────────────────

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

#[tokio::test]
async fn test_context_overflow_triggers_emergency_compaction_and_recovers() {
    struct OverflowRecoverLlm {
        call_count: AtomicU32,
    }
    impl LLM for OverflowRecoverLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "overflow-model"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if count == 0 {
                    // First call fails with a context overflow error.
                    Err(Box::new(std::io::Error::other(
                        "llm http status 400 Bad Request: context_length_exceeded",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else if count == 1 {
                    // The summarizer produces the replacement text.
                    Ok(LLMChatResponse {
                        content: "Earlier user and assistant discussed the task.".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    // Third call succeeds after compaction. v2 shape:
                    // system + head user input (u1, kept verbatim) +
                    // elision note + prefixed summary + continuation +
                    // the uncompacted tail (u2/a2/u3) + date reminder.
                    assert_eq!(params.messages.len(), 8);
                    assert_eq!(params.messages[0].role, "system");
                    assert_eq!(params.messages[1].content, "u1");
                    // The compacted range here covers only u1/a1 — its
                    // head keeps u1 verbatim; u2/u3 live in the
                    // uncompacted tail.
                    assert_eq!(params.messages[2].role, "user");
                    assert!(
                        params.messages[2]
                            .content
                            .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX),
                        "the summary must ride the compaction prefix"
                    );
                    let summary_message = &params.messages[2];
                    assert!(
                        summary_message
                            .content
                            .ends_with("Earlier user and assistant discussed the task."),
                        "the generated summary must replace the omitted history"
                    );
                    assert_eq!(
                        params.messages[3].content,
                        crate::compaction::COMPACTION_CONTINUATION_TEXT,
                        "continuation note carried by the compacted shape, ahead of the tail"
                    );
                    assert_eq!(params.messages[4].content, "u2");
                    assert_eq!(params.messages[5].content, "a2");
                    assert_eq!(params.messages[6].content, "u3");
                    assert!(
                        params.messages[7].content.starts_with(
                            "<system-reminder>
"
                        ),
                        "date reminder appended after emergency compaction"
                    );
                    Ok(LLMChatResponse {
                        content: "Recovered successfully".into(),
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

    let llm = OverflowRecoverLlm {
        call_count: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    // The compaction pair rides the same input: PreCompact carries the
    // message count before the trim, PostCompact the trigger and the
    // post-trim token count. Removing either dispatch site inside the
    // overflow-recovery arm leaves its capture file empty.
    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let pre = dir.path().join("pre-compact.json");
    let post = dir.path().join("post-compact.json");
    let hook_guard = Some(Arc::new(crate::tools::external_hooks::HookGuard::new(
        vec![
            capture::hook("PreCompact", "auto", &pre),
            capture::hook("PostCompact", "auto", &post),
        ],
    )));

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-recovery".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "u1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "u2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "u3".into(),
                ..Default::default()
            },
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await.unwrap();
    assert_eq!(llm.call_count.load(Ordering::SeqCst), 3);
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);
    assert_eq!(
        result.messages.last().unwrap().content,
        "Recovered successfully"
    );
    let pre_payload = capture::wait(&pre).await;
    assert_eq!(pre_payload["hook_event_name"], "PreCompact");
    assert!(pre_payload["message_count"].as_u64().unwrap() > 0);
    let post_payload = capture::wait(&post).await;
    assert_eq!(post_payload["hook_event_name"], "PostCompact");
    assert_eq!(post_payload["trigger"], "auto");
    assert!(post_payload["estimated_token_count"].as_u64().unwrap() > 0);
}

/// v2 #3750: the host's `loopControl.compactionMaxAttempts` reaches the
/// summarizer through the turn loop, so a failing emergency compaction
/// stops at the configured request count instead of the engine default.
///
/// The step overflows exactly once and then fails for an unrelated reason,
/// so exactly one compaction round runs and the per-round cap is the only
/// thing that could produce a second summarizer request.
#[tokio::test]
async fn test_compaction_attempt_cap_reaches_the_turn_loop_summarizer() {
    struct FailingSummarizerLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for FailingSummarizerLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "cap-model"
        }
        fn is_retryable_error(&self, error: &str) -> bool {
            // Only the summarizer's 500 is retryable; the step errors must
            // fail straight through so the test does not sit in the
            // step-retry backoff.
            error.contains("500")
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            // The summarizer prompt is the only call whose system message
            // is the summarizer instruction; the step call carries the
            // turn's own system prompt.
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                return Box::pin(async move {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 500 summarizer unavailable",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                });
            }
            let step = self.step_calls.fetch_add(1, Ordering::SeqCst);
            let message = if step == 0 {
                // First step: a context overflow, so the emergency
                // compaction path runs.
                "llm http status 400 Bad Request: context_length_exceeded"
            } else {
                // The retried step fails for a reason compaction cannot
                // help with, ending the turn after one round.
                "llm http status 401 Unauthorized: bad key"
            };
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = FailingSummarizerLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-cap".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "u1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "u2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "u3".into(),
                ..Default::default()
            },
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: Some(1),
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let error = run_turn(input, &callbacks).await.unwrap_err();
    assert!(
        error.to_string().contains("summarizer unavailable"),
        "{error}"
    );
    assert_eq!(
        llm.summarizer_calls.load(Ordering::SeqCst),
        1,
        "the configured cap bounds the summarizer requests"
    );
    assert_eq!(
        llm.step_calls.load(Ordering::SeqCst),
        1,
        "failed summarization must not retry a step with destroyed history"
    );
}

/// v2 `recoverFromContextOverflow` (fullCompactionService.ts:468-478)
/// compacts and **retries the step**, allowing
/// `maxOverflowCompactionAttempts` (3) consecutive rounds before the turn
/// fails. Recovering exactly once — what the loop used to do — fails a turn
/// v2 recovers on its second or third attempt.
#[tokio::test]
async fn context_overflow_recovery_retries_the_step() {
    struct AlwaysOverflowLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for AlwaysOverflowLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "overflow-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            // Neither failure is retried by the step layer, so the only
            // repetition this test can observe is the overflow recovery.
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            let message = if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                // Summarise successfully so the compaction actually shrinks
                // the history and the next round has something to do.
                return Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: "summary of the omitted prefix".into(),
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
                });
            } else {
                self.step_calls.fetch_add(1, Ordering::SeqCst);
                "llm http status 400 Bad Request: context_length_exceeded".to_string()
            };
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = AlwaysOverflowLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    // A history long enough that a compaction still has a prefix to remove
    // on the second round.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
        content: "sys".into(),
        ..Default::default()
    }];
    for index in 0..12 {
        messages.push(LLMMessage {
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("turn {index} with enough text to be worth compacting"),
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-retry".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
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

    assert!(run_turn(input, &callbacks).await.is_err());
    let step_calls = llm.step_calls.load(Ordering::SeqCst);
    let rounds = llm.summarizer_calls.load(Ordering::SeqCst);
    // Each recovery round compacts (one summarizer request) and retries the
    // step, so the rounds are what distinguishes this from the old
    // single-shot recovery: that one produced exactly one round.
    assert!(
        rounds >= 2,
        "the overflow recovery must repeat, not give up after one round; \
             saw {rounds} round(s) and {step_calls} step call(s)"
    );
    assert!(
        rounds <= 3,
        "the recovery is bounded by maxOverflowCompactionAttempts (3); saw {rounds}"
    );
    assert_eq!(
        step_calls,
        rounds + 1,
        "each round retries the step exactly once"
    );
}

/// The emergency path used to announce `compaction.completed` and only
/// then decide the round had done nothing. `count == 0` makes
/// `force_compact_messages_with_summary_report` answer with a bare no-op —
/// no summarizer request, the messages untouched,
/// `tokensAfter == tokensBefore` — so that card claimed a success for zero
/// work and the turn failed a line later anyway.
///
/// v2 gets the same outcome from the other side: `fullCompactionService`
/// throws on an empty summary inside `compactionRound`, so
/// `compactionWorker` never reaches its `CompactionCompleted` dispatch and
/// the `catch` lands on `cancelActive` -> `CompactionCancelled`. The turn
/// loop already owes a terminal event once `compaction.started` is out, so
/// the question is only which one; `cancelled` is the one that matches
/// what happened.
///
/// `[system, single user]` is what drives `count` to 0:
/// `can_split_after` refuses to cut after a `user` message, so the split
/// search finds no candidate and `fit_compact_count_to_window(0)` floors
/// the count.
#[tokio::test]
async fn overflow_without_a_split_point_reports_cancelled_not_completed() {
    struct AlwaysOverflowLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for AlwaysOverflowLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "overflow-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                return Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: "summary of the omitted prefix".into(),
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
                });
            }
            self.step_calls.fetch_add(1, Ordering::SeqCst);
            let message = "llm http status 400 Bad Request: context_length_exceeded".to_string();
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = AlwaysOverflowLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let messages = vec![
        LLMMessage {
            role: "system".into(),
            content: "sys".into(),
            ..Default::default()
        },
        LLMMessage {
            role: "user".into(),
            content: "one oversized turn that cannot be split anywhere".into(),
            ..Default::default()
        },
    ];

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-no-split".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
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

    assert!(run_turn(input, &callbacks).await.is_err());
    assert_eq!(
        llm.summarizer_calls.load(Ordering::SeqCst),
        0,
        "count == 0 must not reach the summarizer; if this is non-zero the \
             history has a split point and the test is not exercising the no-op"
    );

    let compaction: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| event.get("type").and_then(|kind| kind.as_str()))
        .filter(|kind| kind.starts_with("compaction."))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        compaction,
        ["compaction.started", "compaction.cancelled"],
        "an emergency compaction that did nothing must close with a terminal \
             event saying so, not with a success card the turn then fails on"
    );
}

/// A summarizer that fails outright is a different event from one that
/// produces nothing, and until now nothing observed which one was emitted:
/// renaming the `compaction_failed` telemetry event left every test in this
/// module green. The event name is the whole payload a consumer routes on,
/// so an unobserved name is an unobserved event.
#[tokio::test]
async fn a_failing_summarizer_reports_compaction_failed() {
    struct FailingSummarizerLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for FailingSummarizerLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "failing-summarizer-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                // The failure the emergency path must report, as opposed to
                // an empty summary, which is a no-op with its own event.
                return Box::pin(async move {
                    Err(Box::new(std::io::Error::other("summarizer exploded"))
                        as Box<dyn std::error::Error + Send + Sync>)
                });
            }
            self.step_calls.fetch_add(1, Ordering::SeqCst);
            let message = "llm http status 400 Bad Request: context_length_exceeded".to_string();
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = FailingSummarizerLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    // A history with a split point, so the emergency path actually asks the
    // summarizer instead of flooring the count to zero.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
        content: "sys".into(),
        ..Default::default()
    }];
    for index in 0..12 {
        messages.push(LLMMessage {
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("turn {index} with enough text to be worth compacting"),
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-failed".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
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

    assert!(run_turn(input, &callbacks).await.is_err());
    let asked = llm.summarizer_calls.load(Ordering::SeqCst);
    assert!(
        asked > 0,
        "the summarizer must have been asked; it was not, so this is not the failure path"
    );

    let recorded = events.lock().unwrap().clone();
    let failed: Vec<&serde_json::Value> = recorded
        .iter()
        .filter(|event| {
            event.get("event").and_then(|name| name.as_str()) == Some("compaction_failed")
        })
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "one compaction failed once; saw {recorded:?}"
    );
    // v2's `CompactionFailedEvent` property names and required set. `reason`
    // and a string `turn_id` were this engine's invention: v2 has no
    // `reason` field, its `turn_id` is a numeric per-agent turn index, and
    // its `error_type` is the class name.
    assert_eq!(failed[0]["source"], "auto");
    assert!(failed[0]["tokens_before"].is_u64());
    assert!(failed[0]["duration_ms"].is_u64());
    assert_eq!(failed[0]["round"], 1, "the first overflow round");
    // `retry_count` is the per-round budget the config allows, which is what
    // v2 reports for the round that ran out of retries.
    assert!(failed[0]["retry_count"].is_u64());
    let error_type = failed[0]["error_type"].as_str().unwrap_or_default();
    assert!(
        !error_type.is_empty(),
        "the failure class must be named: {failed:?}"
    );
    assert!(failed[0].get("reason").is_none());
    assert!(failed[0].get("turn_id").is_none());

    // The terminal card still goes out: the turn owes one once
    // compaction.started has been emitted.
    let terminal: Vec<String> = recorded
        .iter()
        .filter_map(|event| event.get("type").and_then(|kind| kind.as_str()))
        .filter(|kind| kind.starts_with("compaction."))
        .map(str::to_owned)
        .collect();
    assert_eq!(terminal, ["compaction.started", "compaction.cancelled"]);
}

/// v2 `nextProjectionPolicyForError`: a provider "request too large"
/// degrades the older media and retries the step; a second rejection
/// strips every media part. The step layer retries neither, so the
/// repetition this test observes is the projection fallback's.
#[tokio::test]
async fn a_too_large_request_degrades_then_strips_and_retries() {
    struct TooLargeLlm {
        step_calls: AtomicU32,
        seen_media: std::sync::Mutex<Vec<usize>>,
    }
    impl LLM for TooLargeLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "too-large-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let media = params
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter(|block| !matches!(block, crate::rpc::types::ContentBlock::Text { .. }))
                .count();
            self.seen_media.lock().unwrap().push(media);
            let call = self.step_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call < 2 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 413: request too large",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    Ok(LLMChatResponse {
                        content: "ok".into(),
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

    let llm = Arc::new(TooLargeLlm {
        step_calls: AtomicU32::new(0),
        seen_media: std::sync::Mutex::new(Vec::new()),
    });
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    // Real blobs, so the resolver inlines them instead of degrading the
    // reference to its unavailable placeholder. The saved ids are what
    // the messages reference.
    let mut file_ids = Vec::new();
    for index in 0..4 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                format!("PNGDATA-{index}").as_bytes(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);

    // Four media references across the history: the degrade keeps the
    // two newest, the strip takes the rest.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
        content: "sys".into(),
        ..Default::default()
    }];
    for file_id in &file_ids {
        messages.push(LLMMessage {
            role: "user".into(),
            content: "look".into(),
            blocks: vec![crate::rpc::types::ContentBlock::MediaRef {
                file_id: file_id.clone(),
                kind: crate::rpc::types::MediaKind::Image,
            }],
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-too-large".into(),
        llm: llm.as_ref(),
        messages,
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
        media: Some(&resolver),
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(
        result.is_ok(),
        "the stripped retry must succeed: {result:?}"
    );
    // Three step calls: the original, the degraded retry, the stripped
    // retry — and the media count falls 4 → 2 → 0.
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 0],
        "the degrade keeps the two newest, the strip takes the rest"
    );
}

/// v2 #4183: interrupting the turn cancels an in-flight media upload — the
/// resolve future is dropped, taking the upload's HTTP request with it —
/// and an already-cancelled turn never starts one at all.
#[tokio::test]
async fn an_interruption_cancels_an_in_flight_media_upload() {
    struct UploadLlm {
        target: crate::llm::media_resolver::MediaTarget,
    }
    impl LLM for UploadLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn media_target(&self) -> Option<crate::llm::media_resolver::MediaTarget> {
            Some(self.target.clone())
        }
        fn media_upload_credential(
            &self,
        ) -> Option<crate::llm::files_upload::CredentialFuture<'_>> {
            Some(Box::pin(async { Ok("token".to_string()) }))
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            unreachable!("the upload never completes, so no chat request is built")
        }
    }

    // A server that reports each connection and never answers: the upload
    // stays in flight until the client gives up on it. The sockets are held
    // so accepting one does not close it.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (conn_tx, mut conn_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
            let _ = conn_tx.send(());
        }
    });

    let dir = std::env::temp_dir().join(format!("kimi-upload-cancel-{}", fastrand::u64(..)));
    let store = crate::server::files::FileStore::with_root(dir);
    let meta = store
        .save("clip.mp4", "video/mp4", None, b"MP4DATA")
        .unwrap();
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(store);
    let llm = UploadLlm {
        target: crate::llm::media_resolver::MediaTarget {
            protocol: "openai".into(),
            base_url: format!("http://{addr}"),
            provider_key: "kimi".into(),
            image_in: true,
            video_in: true,
            audio_in: true,
            uploads_media: true,
        },
    };
    let messages = vec![LLMMessage {
        role: "user".into(),
        content: String::new(),
        blocks: vec![crate::turn_loop::types::ContentBlock::MediaRef {
            file_id: meta.id.clone(),
            kind: crate::rpc::types::MediaKind::Video,
        }],
        ..Default::default()
    }];
    let server_rpc = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server_rpc);
    let mut budget = crate::llm::media_budget::MediaBudget::default();

    // Mid-upload cancellation: the resolve returns promptly with nothing,
    // instead of the turn waiting the upload out.
    {
        let token = tokio_util::sync::CancellationToken::new();
        let call = budgeted_request(
            &mut budget,
            Some(&resolver),
            &llm,
            &messages,
            callbacks.as_ref(),
            Some(&token),
        );
        tokio::pin!(call);
        tokio::select! {
            _ = conn_rx.recv() => {}
            _ = &mut call => panic!("the upload must still be in flight"),
        }
        token.cancel();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), &mut call)
            .await
            .expect("a cancelled upload must not be waited out")
            .unwrap();
        assert!(
            out.is_none(),
            "a cancelled resolve yields no request messages"
        );
    }

    // An already-cancelled token never starts the upload at all (v2's
    // `throwIfAborted` at entry).
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let out = budgeted_request(
        &mut budget,
        Some(&resolver),
        &llm,
        &messages,
        callbacks.as_ref(),
        Some(&token),
    )
    .await
    .unwrap();
    assert!(out.is_none());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), conn_rx.recv())
            .await
            .is_err(),
        "a cancelled turn must not start an upload"
    );
    server.abort();
}

/// v2 #4413: a media recovery the provider forced on one turn is agent-level
/// state — the next turn starts from the recovered projection instead of
/// re-sending the rejected media, and a history rewrite (undo) or a model
/// switch invalidates it.
#[tokio::test]
async fn a_media_recovery_applies_to_later_turns_until_invalidated() {
    struct RecoveringLlm {
        step_calls: AtomicU32,
        seen_media: std::sync::Mutex<Vec<usize>>,
        model: std::sync::Mutex<String>,
    }
    impl LLM for RecoveringLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            // The Mutex guard cannot escape; leak once per switch — tests run
            // a handful of switches at most.
            let name = self.model.lock().unwrap().clone();
            Box::leak(name.into_boxed_str())
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let media = params
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter(|block| !matches!(block, crate::rpc::types::ContentBlock::Text { .. }))
                .count();
            self.seen_media.lock().unwrap().push(media);
            let call = self.step_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 413: request too large",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    Ok(LLMChatResponse {
                        content: "ok".into(),
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

    let llm = Arc::new(RecoveringLlm {
        step_calls: AtomicU32::new(0),
        seen_media: std::sync::Mutex::new(Vec::new()),
        model: std::sync::Mutex::new("rec-model".to_string()),
    });
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let mut file_ids = Vec::new();
    for index in 0..4 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                format!("PNGDATA-{index}").as_bytes(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);
    let recovery = crate::llm::media_budget::SharedMediaRecovery::default();

    // The session history keeps the original references; each turn gets a
    // fresh copy (production re-reads the store per turn).
    let history = |prompt: &str| {
        let mut messages = vec![LLMMessage {
            role: "system".into(),
            content: "sys".into(),
            ..Default::default()
        }];
        for file_id in &file_ids {
            messages.push(LLMMessage {
                role: "user".into(),
                content: "look".into(),
                blocks: vec![crate::rpc::types::ContentBlock::MediaRef {
                    file_id: file_id.clone(),
                    kind: crate::rpc::types::MediaKind::Image,
                }],
                ..Default::default()
            });
        }
        messages.push(LLMMessage {
            role: "user".into(),
            content: prompt.into(),
            ..Default::default()
        });
        messages
    };
    let input_for = |messages: Vec<LLMMessage>, turn_id: &str| RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: turn_id.into(),
        llm: llm.as_ref(),
        messages,
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
        media: Some(&resolver),
        media_dropped: None,
        media_recovery: Some(recovery.clone()),
        toolset: None,
    };

    // Turn 1: the 413 fires the degrade (4 → 2) and records the recovery.
    let result = run_turn(input_for(history("one"), "t1"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 2);
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2]);
    assert_eq!(
        recovery
            .lock()
            .unwrap()
            .recovery
            .as_ref()
            .map(|r| &r.action),
        Some(&crate::llm::media_budget::MediaRecoveryAction::Degraded(2)),
    );

    // Turn 2: the recovery applies before the first request — no rejection
    // needed, the projection is already degraded.
    let result = run_turn(input_for(history("two"), "t2"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 2],
        "the second turn must start degraded instead of re-sending 4 media"
    );

    // A model switch invalidates the recovery (v2 keys it by model alias).
    *llm.model.lock().unwrap() = "other-model".to_string();
    let result = run_turn(input_for(history("three"), "t3"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2, 2, 4]);

    // Back on the original model the recovery applies again (it was keyed
    // for it and never invalidated by the switch).
    *llm.model.lock().unwrap() = "rec-model".to_string();
    let result = run_turn(input_for(history("four"), "t4"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2, 2, 4, 2]);

    // A history rewrite (undo / set_history / clear_history) advances the
    // revision and invalidates the recovery.
    recovery.lock().unwrap().advance_revision();
    let result = run_turn(input_for(history("five"), "t5"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 2, 4, 2, 4]
    );
}

/// v2 `nextProjectionPolicyForError`'s image-format arm
/// (`isImageFormatError`): the provider rejects an image, and the
/// request is retried with every media part stripped — no degrade
/// round, the way v2 gives it none.
#[tokio::test]
async fn an_image_format_rejection_strips_the_media_and_retries() {
    struct ImageFormatLlm {
        step_calls: AtomicU32,
        seen_media: std::sync::Mutex<Vec<usize>>,
    }
    impl LLM for ImageFormatLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "image-format-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let media = params
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter(|block| !matches!(block, crate::rpc::types::ContentBlock::Text { .. }))
                .count();
            self.seen_media.lock().unwrap().push(media);
            let call = self.step_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 400: unsupported image format image/heic",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    Ok(LLMChatResponse {
                        content: "ok".into(),
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

    let llm = Arc::new(ImageFormatLlm {
        step_calls: AtomicU32::new(0),
        seen_media: std::sync::Mutex::new(Vec::new()),
    });
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let mut file_ids = Vec::new();
    for index in 0..3 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                format!("PNGDATA-{index}").as_bytes(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);

    let mut messages = vec![LLMMessage {
        role: "system".into(),
        content: "sys".into(),
        ..Default::default()
    }];
    for file_id in &file_ids {
        messages.push(LLMMessage {
            role: "user".into(),
            content: "look".into(),
            blocks: vec![crate::rpc::types::ContentBlock::MediaRef {
                file_id: file_id.clone(),
                kind: crate::rpc::types::MediaKind::Image,
            }],
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-image-format".into(),
        llm: llm.as_ref(),
        messages,
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
        media: Some(&resolver),
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(
        result.is_ok(),
        "the stripped retry must succeed: {result:?}"
    );
    // Two step calls, and the media goes straight to zero — the
    // image-format arm has no degrade round.
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 2);
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[3, 0]);
}

/// The media budget degrades an over-budget request instead of failing it:
/// the oldest media leave the request, the history keeps them, and the
/// omission reaches the user as a `warning` event.
#[tokio::test]
async fn test_over_budget_media_are_omitted_from_the_request_and_warned_about() {
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
                        input_tokens: 5,
                        output_tokens: 3,
                        total_tokens: 8,
                        ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let image = |tag: &str, bytes: usize| LLMMessage {
        role: "user".into(),
        content: String::new(),
        blocks: vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: format!("{tag}{}", "A".repeat(bytes)),
            name: None,
        }],
        tool_calls: Vec::new(),
        tool_call_id: None,

        prompt_id: None,
        origin: None,
    };
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-media-budget".into(),
        llm: &llm,
        messages: vec![
            image("a", 8 * 1024 * 1024),
            image("b", 8 * 1024 * 1024),
            image("c", 8 * 1024 * 1024),
        ],
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

    let turn = run_turn(input, &callbacks).await.unwrap();

    let sent = captured.lock().unwrap().clone();
    let omitted = |message: &LLMMessage| {
        matches!(
            message.blocks.first(),
            Some(ContentBlock::Text { text })
                if text == "[image omitted: dropped to fit the request media budget]"
        )
    };
    // The turn head is the rebuilt system message, and the loop injects a
    // date reminder of its own — the media are the messages that carry a
    // media block or the placeholder one left behind.
    let media: Vec<&LLMMessage> = sent
        .iter()
        .filter(|m| {
            omitted(m)
                || m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }))
        })
        .collect();
    assert_eq!(media.len(), 3, "{sent:?}");
    assert!(omitted(media[0]), "the oldest media leaves the request");
    assert!(omitted(media[1]));
    assert!(
        !omitted(media[2]),
        "the newest media stays: {:?}",
        media[2].blocks
    );
    assert!(
        turn.messages
            .iter()
            .filter(|m| m.role == "user")
            .all(|m| !omitted(m)),
        "the history keeps every block, so a later request can still see them"
    );
    let warnings: Vec<serde_json::Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.get("type").and_then(|t| t.as_str()) == Some("warning"))
        .cloned()
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["code"], "media-budget-exceeded");
    assert_eq!(
        warnings[0]["message"],
        "Conversation media exceeded the 20 MB per-request budget; \
             2 older media item(s) were omitted."
    );
}

// ── AGENTS.md bash operand dirs (`host/parse_bash`) ─────────────────────

/// The process cwd for the whole turn, restored on drop — the seam and the
/// reminder both anchor at `current_dir`, so the fixture drives them into a
/// temp workspace (the same shape the CLI runs in).
struct CwdGuard(std::path::PathBuf);
impl CwdGuard {
    fn enter(path: &std::path::Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(previous)
    }
}
impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

/// Serializes this file's chdir-driving tests: each holds a temp workspace
/// as the process cwd for a whole turn, and two such windows overlapping
/// mis-resolves each other's relative probe. (The suite's other chdir user,
/// the tower-denial test, keeps its own shorter window.)
static BASH_TARGET_CWD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One scripted exchange: step 1 emits the configured Bash call, step 2
/// stops. Every request's messages are captured so the injection pass that
/// follows the Bash call is observable.
struct BashThenStopLlm {
    bash_args: serde_json::Value,
    requests: Arc<Mutex<Vec<Vec<LLMMessage>>>>,
}
impl LLM for BashThenStopLlm {
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
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let requests = self.requests.clone();
        let bash_args = self.bash_args.clone();
        Box::pin(async move {
            requests.lock().unwrap().push(params.messages.to_vec());
            let first = requests.lock().unwrap().len() == 1;
            let usage = TokenUsage {
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                ..Default::default()
            };
            if first {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: "tc-bash".into(),
                        name: "Bash".into(),
                        arguments: bash_args,
                        extras: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage,
                    timing: None,
                })
            } else {
                Ok(LLMChatResponse {
                    content: "done".into(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage,
                    timing: None,
                })
            }
        })
    }
}

/// A `HostCallbacks` wrapper whose `parse_bash` answer is scripted, with the
/// requests recorded for inspection.
struct ParseBashStub {
    inner: Arc<dyn HostCallbacks>,
    requests: Arc<Mutex<Vec<crate::rpc::types::ParseBashRequest>>>,
    answer: Result<Option<Vec<String>>, String>,
}
impl HostCallbacks for ParseBashStub {
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
        self.inner.execute_tool(request)
    }
    fn check_permission(
        &self,
        request: crate::rpc::types::PermissionCheckRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<crate::rpc::types::PermissionDecision, String>>
    {
        self.inner.check_permission(request)
    }
    fn parse_bash(
        &self,
        request: crate::rpc::types::ParseBashRequest,
    ) -> crate::rpc::types::BoxFuture<'static, Result<Option<Vec<String>>, String>> {
        self.requests.lock().unwrap().push(request);
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }
}

fn bash_turn_input<'a>(llm: &'a BashThenStopLlm) -> RunTurnInput<'a> {
    RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-bash-targets".into(),
        llm,
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
    }
}

fn stub_tool_server() -> Arc<RpcServer> {
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
    server
}

fn reminder_in(request: &[LLMMessage]) -> Option<&LLMMessage> {
    request
        .iter()
        .find(|m| m.content.contains("AGENTS.md file(s) apply"))
}

/// The full chain: the seam asks the host parser, the returned operand dirs
/// feed the reminder, and the next request names the discovered files.
#[tokio::test]
async fn bash_operand_dirs_reach_the_agents_md_reminder_through_the_host_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _serialize = BASH_TARGET_CWD_LOCK.lock().await;
    let _cwd = CwdGuard::enter(root);

    let requests: Arc<Mutex<Vec<crate::rpc::types::ParseBashRequest>>> =
        Arc::new(Mutex::new(Vec::new()));
    let stub = ParseBashStub {
        inner: rpc_callbacks(stub_tool_server()),
        requests: requests.clone(),
        answer: Ok(Some(vec![root.join("sub").to_string_lossy().into_owned()])),
    };
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(stub);
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));

    // The seam asked exactly once, with the command and the call's
    // effective cwd (no declared `cwd` → the process one).
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "ls sub");
    assert_eq!(
        std::path::PathBuf::from(&requests[0].cwd),
        std::env::current_dir().unwrap()
    );
    assert!(!requests[0].home.is_empty());

    // The returned operand dir fed the reminder: the chain lists the root
    // file and the one in `sub`.
    let captured = captured.lock().unwrap();
    let reminder = reminder_in(&captured[1]).expect("the operand dir must inject");
    assert!(reminder.content.contains("AGENTS.md"), "{reminder:?}");
    assert!(reminder.content.contains("sub"), "{reminder:?}");
}

/// Degradation arm one: no parser wired (the server's shape — the trait
/// default) means the operands contribute nothing, while the turn runs on
/// undisturbed.
#[tokio::test]
async fn bash_operands_contribute_nothing_when_the_host_has_no_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    // The only AGENTS.md sits where *only* an operand could reach it.
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _serialize = BASH_TARGET_CWD_LOCK.lock().await;
    let _cwd = CwdGuard::enter(root);

    let callbacks = rpc_callbacks(stub_tool_server());
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    let captured = captured.lock().unwrap();
    assert!(
        reminder_in(&captured[1]).is_none(),
        "no parser, no operand dirs: {:?}",
        captured[1]
    );
}

/// Degradation arm two: without a parser the Bash call's own declared `cwd`
/// still probes, exactly as before this seam existed.
#[tokio::test]
async fn the_declared_cwd_still_probes_when_the_host_has_no_parser() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    let _serialize = BASH_TARGET_CWD_LOCK.lock().await;
    let _cwd = CwdGuard::enter(root);

    let callbacks = rpc_callbacks(stub_tool_server());
    let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = BashThenStopLlm {
        bash_args: serde_json::json!({ "command": "ls", "cwd": "sub" }),
        requests: captured.clone(),
    };

    let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
    assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
    let captured = captured.lock().unwrap();
    let reminder = reminder_in(&captured[1]).expect("the declared cwd must still inject");
    assert!(reminder.content.contains("sub"), "{reminder:?}");
}

/// Degradation arm three: a wired host whose parse fails (`Ok(None)`, v2's
/// `!parsed.ok` arm) or whose channel errors keeps the cwd-only shape —
/// the declared cwd probes, the operands do not.
#[tokio::test]
async fn a_failed_parse_keeps_the_cwd_only_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir(root.join("sub")).unwrap();
    std::fs::create_dir(root.join("other")).unwrap();
    std::fs::write(root.join("sub").join("AGENTS.md"), "sub rules").unwrap();
    std::fs::write(root.join("other").join("AGENTS.md"), "other rules").unwrap();
    let _serialize = BASH_TARGET_CWD_LOCK.lock().await;
    let _cwd = CwdGuard::enter(root);

    for answer in [Ok(None), Err("channel dead".to_string())] {
        let requests: Arc<Mutex<Vec<crate::rpc::types::ParseBashRequest>>> =
            Arc::new(Mutex::new(Vec::new()));
        let stub = ParseBashStub {
            inner: rpc_callbacks(stub_tool_server()),
            requests: requests.clone(),
            answer,
        };
        let callbacks: Arc<dyn HostCallbacks> = Arc::new(stub);
        let captured: Arc<Mutex<Vec<Vec<LLMMessage>>>> = Arc::new(Mutex::new(Vec::new()));
        let llm = BashThenStopLlm {
            bash_args: serde_json::json!({ "command": "ls other", "cwd": "sub" }),
            requests: captured.clone(),
        };

        let turn = run_turn(bash_turn_input(&llm), &callbacks).await.unwrap();
        assert!(matches!(turn.stop_reason, LoopTurnStopReason::EndTurn));
        let captured = captured.lock().unwrap();
        let reminder = reminder_in(&captured[1]).expect("the declared cwd must still inject");
        assert!(reminder.content.contains("sub"), "{reminder:?}");
        assert!(
            !reminder.content.contains("other"),
            "a failed parse must not leak operand dirs: {reminder:?}"
        );
    }
}
