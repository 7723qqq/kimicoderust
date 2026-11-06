use super::*;
use std::sync::atomic::AtomicU32;
use tokio::sync::Notify;

/// The swarm timeout rides host values verbatim: `0` means "explicitly no
/// timeout" (the tool maps it to the never-expiring sentinel) and must not
/// be folded into the 2h default the way an unset value is.
/// Upstream #3847: per-instance cumulative usage folds at every turn
/// completion, and `caller_tokens` reports v2's `grandTotal` (all four
/// dimensions, not the input+output `total_tokens`). An unknown caller
/// reports `None` (v2's -1).
#[tokio::test]
async fn test_caller_tokens_accumulates_all_dimensions() {
    let manager = SubagentManager::new();
    let usage = crate::rpc::types::TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        input_cache_read: 3,
        input_cache_creation: 2,
    };
    manager.record_instance_usage("subagent-1", &usage);
    manager.record_instance_usage("subagent-1", &usage);

    let cumulative = manager.cumulative_usage("subagent-1").await.unwrap();
    assert_eq!(cumulative.input_tokens, 20);
    assert_eq!(cumulative.output_tokens, 10);
    assert_eq!(cumulative.input_cache_read, 6);
    assert_eq!(cumulative.input_cache_creation, 4);
    // 20 + 6 + 4 + 10 — the cache dimensions count, unlike total_tokens.
    assert_eq!(manager.caller_tokens("subagent-1").await, Some(40));
    assert_eq!(manager.caller_tokens("main").await, None);
}

#[test]
fn test_swarm_timeout_zero_is_explicit_no_timeout() {
    let manager = SubagentManager::new();
    manager.set_swarm_timeout_ms(Some(0));
    assert_eq!(manager.swarm_timeout_ms(), Some(0));
    manager.set_swarm_timeout_ms(Some(60_000));
    assert_eq!(manager.swarm_timeout_ms(), Some(60_000));
    manager.set_swarm_timeout_ms(None);
    assert_eq!(manager.swarm_timeout_ms(), None);
}

#[test]
fn test_partial_output_section_picks_the_last_assistant_text() {
    use crate::turn_loop::types::{LLMMessage, LoopTurnStopReason, TurnResult};
    let turn = |messages: Vec<LLMMessage>| TurnResult {
        stop_reason: LoopTurnStopReason::Aborted,
        steps: 1,
        usage: Default::default(),
        events_emitted: 0,
        llm_retries: 0,
        llm_transport: String::new(),
        native_tool_calls: 0,
        messages,
        last_request_usage: None,
        stop_hook_continuation: None,
    };
    let assistant = |content: &str| LLMMessage {
        role: "assistant".into(),
        content: content.into(),
        ..Default::default()
    };

    // No assistant text yet — the report stays the bare stopped message.
    assert!(partial_output_section(&turn(vec![])).is_none());
    assert!(partial_output_section(&turn(vec![assistant("   ")])).is_none());

    // The last non-empty assistant text rides in a marked block.
    let section = partial_output_section(&turn(vec![
        assistant("first draft"),
        assistant("second draft"),
    ]))
    .unwrap();
    assert!(section.starts_with("\n\n[partial output]\n"));
    assert!(section.contains("second draft"));
    assert!(!section.contains("first draft"));
}

#[tokio::test]
async fn test_subagent_lifecycle() {
    let manager = SubagentManager::new();

    // Check built-in research definition
    let def = manager.get_definition("research").await.unwrap();
    assert_eq!(def.name, "research");
    assert!(def.tools.contains(&"fetch_url".to_string()));

    // Spawn a research subagent
    let id = manager
        .spawn("research", "Documentation Researcher")
        .await
        .unwrap();

    let list = manager.list().await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, id);
    assert_eq!(list[0].state, SubagentState::Running);

    // Update state
    manager
        .update_state(&id, SubagentState::Completed, Some("Found 5 docs".into()))
        .await;

    let list = manager.list().await;
    assert_eq!(list[0].state, SubagentState::Completed);

    // Kill subagent
    assert!(manager.kill(&id).await.unwrap());
    let list = manager.list().await;
    assert_eq!(list[0].state, SubagentState::Terminated);
}

struct MockSubagentLlm;
impl crate::turn_loop::types::LLM for MockSubagentLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }
    fn model_name(&self) -> &str {
        "mock-subagent-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async {
            Ok(crate::turn_loop::types::LLMChatResponse {
                content: "Autonomous research result complete.".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: crate::rpc::types::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 15,
                    total_tokens: 25,
                    input_cache_read: 0,
                    input_cache_creation: 0,
                },
                timing: None,
            })
        })
    }
}

struct MockCallbacks;
impl crate::callbacks::HostCallbacks for MockCallbacks {
    fn list_tools(
        &self,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::ListToolsResponse, String>,
    > {
        // The trait default answers with an error, and a subagent turn
        // used to swallow that into an empty table. A subagent now fails
        // instead, so a mock that exercises the turn has to offer a table.
        Box::pin(async { Ok(crate::rpc::types::ListToolsResponse { tools: Vec::new() }) })
    }
    fn llm_chat(
        &self,
        _req: crate::rpc::types::LlmChatRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        Box::pin(async { Err("Not needed in mock".into()) })
    }
    fn execute_tool(
        &self,
        _req: crate::rpc::types::ToolExecuteRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::ToolExecuteResponse, String>,
    > {
        Box::pin(async { Err("Not needed in mock".into()) })
    }
    fn check_permission(
        &self,
        _req: crate::rpc::types::PermissionCheckRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::PermissionDecision, String>,
    > {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }
}

/// The end-to-end shape: a subagent that actually calls a tool.
///
/// Runs a real subagent turn — the LLM answers with a tool call, the turn
/// loop executes it — and asserts both the emitted event and the tool
/// request name the subagent. The identity is not a decorator's job and it
/// does not ride the `CALLER_AGENT_ID` task-local: that scope does not
/// cross the scheduler's `tokio::spawn`
/// (`turn_loop/types.rs:864-866`). The turn loop carries the owner
/// explicitly instead, filling `ToolExecuteRequest.agent_id` from
/// `RunTurnInput.agent_id` (v2's `IAgentScopeContext` shape,
/// `toolExecutorService.ts:578-588`), and every event reads the request
/// back. An unowned tool event is what the host reads as `main`, which put
/// the subagent's tool card in the main transcript and truncated the main
/// agent's reasoning there.
#[tokio::test]
async fn a_subagents_real_tool_event_is_attributed_to_it() {
    /// One tool call, then a plain answer — the shape that used to leak.
    struct ToolCallingLlm {
        /// `chat` invocations so far. The answer is a tool call only the
        /// first time: a subagent turn has no step cap of its own, so a
        /// model that keeps asking for the same call can only be stopped by
        /// the repeat breaker, and the run's terminal state would then
        /// depend on that guard instead of on the turn.
        calls: Arc<AtomicU32>,
    }
    impl ToolCallingLlm {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicU32::new(0)),
            }
        }
    }
    impl crate::turn_loop::types::LLM for ToolCallingLlm {
        fn system_prompt(&self) -> &str {
            "mock system prompt"
        }
        fn model_name(&self) -> &str {
            "mock-subagent-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _params: crate::turn_loop::types::LLMChatParams,
        ) -> crate::rpc::types::BoxFuture<
            '_,
            Result<
                crate::turn_loop::types::LLMChatResponse,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            let usage = crate::rpc::types::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                input_cache_read: 0,
                input_cache_creation: 0,
            };
            let response = if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                crate::turn_loop::types::LLMChatResponse {
                    content: String::new(),
                    thinking: Vec::new(),
                    tool_calls: vec![crate::turn_loop::types::ToolCall {
                        id: "sub_call_1".into(),
                        name: "probe_tool".into(),
                        arguments: serde_json::json!({}),
                        extras: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage,
                    timing: None,
                }
            } else {
                crate::turn_loop::types::LLMChatResponse {
                    content: "Probe done.".into(),
                    thinking: Vec::new(),
                    tool_calls: Vec::new(),
                    finish_reason: Some("stop".into()),
                    usage,
                    timing: None,
                }
            };
            Box::pin(async move { Ok(response) })
        }
    }

    #[derive(Clone)]
    struct Recorder {
        events: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        /// The `agent_id` each tool-execute request carried, asserted
        /// against the instance id once the run settles — the request is
        /// the one carrier that survives the spawn boundary.
        request_agents: Arc<Mutex<Vec<String>>>,
    }
    impl crate::callbacks::HostCallbacks for Recorder {
        fn llm_chat(
            &self,
            _req: crate::rpc::types::LlmChatRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::LlmChatResponse, String>,
        > {
            Box::pin(async { Err("not used".into()) })
        }
        fn execute_tool(
            &self,
            req: crate::rpc::types::ToolExecuteRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            assert_eq!(req.tool_name, "probe_tool", "the subagent's own call");
            // The request's `agent_id`, not the `CALLER_AGENT_ID`
            // task-local: this runs on the scheduler's `tokio::spawn`ed
            // task, which does not inherit the subagent turn's scope, so
            // asserting on the task-local would fail on every run and would
            // point a maintainer at it instead of at the request.
            self.request_agents.lock().unwrap().push(req.agent_id);
            Box::pin(async {
                Ok(crate::rpc::types::ToolExecuteResponse {
                    content: "probe output".into(),
                    is_error: false,
                    note: None,
                    stop_turn: false,
                    delivery: None,
                })
            })
        }
        fn check_permission(
            &self,
            _req: crate::rpc::types::PermissionCheckRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
        }
        fn list_tools(
            &self,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::ListToolsResponse, String>,
        > {
            Box::pin(async {
                Ok(crate::rpc::types::ListToolsResponse {
                    tools: vec![crate::rpc::types::ToolInfo {
                        name: "probe_tool".into(),
                        description: "probe".into(),
                        input_schema: serde_json::json!({ "type": "object" }),
                    }],
                })
            })
        }
        fn emit_event(&self, event: serde_json::Value) {
            self.events.lock().unwrap().push(event);
        }
    }

    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let request_agents = Arc::new(Mutex::new(Vec::new()));
    let manager = Arc::new(SubagentManager::new());
    manager
        .set_runtime(
            Arc::new(ToolCallingLlm::new()),
            Arc::new(Recorder {
                events: events.clone(),
                request_agents: request_agents.clone(),
            }),
            None,
        )
        .await;

    let id = manager
        .spawn_and_run(
            "research",
            "Probe",
            "Call probe_tool once.",
            Arc::new(ToolCallingLlm::new()),
            Arc::new(Recorder {
                events: events.clone(),
                request_agents: request_agents.clone(),
            }),
        )
        .await
        .unwrap();

    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some(inst) = manager.get_instance(&id).await
            && inst.state == SubagentState::Completed
        {
            break;
        }
    }
    // The loop above only bounds the wait; without this the assertions
    // below would read a run that never started.
    let instance = manager
        .get_instance(&id)
        .await
        .expect("the spawned subagent must still be listed");
    assert_eq!(
        instance.state,
        SubagentState::Completed,
        "the subagent turn must end on the plain-text answer, got {:?} ({:?})",
        instance.state,
        instance.last_result
    );
    let requested = request_agents.lock().unwrap();
    assert!(
        !requested.is_empty(),
        "the subagent must actually reach its tool call"
    );
    for agent in requested.iter() {
        assert_eq!(
            agent, &id,
            "the tool request must name the subagent — the host reads a \
                 `main` request as the main agent's and files the card in the \
                 main transcript"
        );
    }
    drop(requested);

    let recorded = events.lock().unwrap();
    let tool_events: Vec<_> = recorded
        .iter()
        .filter(|e| {
            e.get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.starts_with("tool."))
        })
        .collect();
    assert!(
        !tool_events.is_empty(),
        "a tool call must surface a tool event, got: {recorded:?}"
    );
    for event in &tool_events {
        assert_eq!(
            event.get("agent_id").and_then(|v| v.as_str()),
            Some(id.as_str()),
            "a subagent's tool event must name the subagent — an anonymous \
                 one is read as `main` by the host and lands in the main \
                 transcript, truncating the main agent's reasoning. Event: {event:?}"
        );
    }
}

#[tokio::test]
async fn test_subagent_spawn_and_run() {
    let manager = Arc::new(SubagentManager::new());
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);

    let id = manager
        .spawn_and_run(
            "research",
            "Automated Codebase Scanner",
            "Investigate main loop architecture",
            llm,
            callbacks,
        )
        .await
        .unwrap();

    // Allow background Tokio task to execute run_turn
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        if let Some(inst) = manager.get_instance(&id).await
            && inst.state == SubagentState::Completed
        {
            assert!(inst.last_result.is_some());
            assert!(inst.last_result.unwrap().contains("finished in"));
            return;
        }
    }

    panic!("Subagent did not reach Completed state within timeout");
}

#[tokio::test]
async fn test_subagent_runtime_injection() {
    let manager = Arc::new(SubagentManager::new());
    assert!(manager.runtime().await.is_none());

    let llm: Arc<dyn crate::turn_loop::types::LLM> = Arc::new(MockSubagentLlm);
    let callbacks: Arc<dyn crate::callbacks::HostCallbacks> = Arc::new(MockCallbacks);
    manager
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    let runtime = manager.runtime().await.expect("runtime injected");
    assert!(Arc::ptr_eq(&runtime.llm, &llm));
    assert!(Arc::ptr_eq(&runtime.callbacks, &callbacks));
}

/// Mock LLM that records how many messages each chat call received, so
/// tests can assert that persistent turns reuse the instance history.
struct RecordingMockLlm {
    counts: Arc<Mutex<Vec<usize>>>,
}

impl RecordingMockLlm {
    fn new() -> Self {
        Self {
            counts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn message_counts(&self) -> Vec<usize> {
        self.counts.lock().unwrap().clone()
    }
}

impl crate::turn_loop::types::LLM for RecordingMockLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }
    fn model_name(&self) -> &str {
        "mock-subagent-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let counts = self.counts.clone();
        Box::pin(async move {
            counts.lock().unwrap().push(params.messages.len());
            Ok(crate::turn_loop::types::LLMChatResponse {
                content: "Autonomous research result complete.".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: crate::rpc::types::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 15,
                    total_tokens: 25,
                    input_cache_read: 0,
                    input_cache_creation: 0,
                },
                timing: None,
            })
        })
    }
}

/// Mock LLM whose first chat call blocks until released, so tests can
/// hold a turn open while asserting on the concurrent-turn guard.
struct BlockingLlm {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl crate::turn_loop::types::LLM for BlockingLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }
    fn model_name(&self) -> &str {
        "mock-subagent-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            release.notified().await;
            Ok(crate::turn_loop::types::LLMChatResponse {
                content: "done".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: crate::rpc::types::TokenUsage {
                    input_tokens: 5,
                    output_tokens: 5,
                    total_tokens: 10,
                    input_cache_read: 0,
                    input_cache_creation: 0,
                },
                timing: None,
            })
        })
    }
}

/// Mock LLM that returns a tool call on its first chat, then blocks on
/// the second — so a kill landing mid-turn is observed at the next step
/// head instead of being masked by a completing response.
struct ToolThenBlockLlm {
    calls: AtomicU32,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl crate::turn_loop::types::LLM for ToolThenBlockLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }
    fn model_name(&self) -> &str {
        "mock-subagent-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            if call == 0 {
                Ok(crate::turn_loop::types::LLMChatResponse {
                    content: String::new(),
                    thinking: Vec::new(),
                    tool_calls: vec![crate::turn_loop::types::ToolCall {
                        id: "tc1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": "/a.txt" }),
                        extras: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: crate::rpc::types::TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                        input_cache_read: 0,
                        input_cache_creation: 0,
                    },
                    timing: None,
                })
            } else {
                entered.notify_one();
                release.notified().await;
                Ok(crate::turn_loop::types::LLMChatResponse {
                    content: String::new(),
                    thinking: Vec::new(),
                    tool_calls: vec![crate::turn_loop::types::ToolCall {
                        id: "tc2".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": "/b.txt" }),
                        extras: None,
                    }],
                    finish_reason: Some("tool_calls".into()),
                    usage: crate::rpc::types::TokenUsage {
                        input_tokens: 5,
                        output_tokens: 5,
                        total_tokens: 10,
                        input_cache_read: 0,
                        input_cache_creation: 0,
                    },
                    timing: None,
                })
            }
        })
    }
}

/// Callbacks whose tool executions always succeed, for tests that need
/// the loop to survive a tool-call step.
struct OkToolCallbacks;

impl crate::callbacks::HostCallbacks for OkToolCallbacks {
    fn list_tools(
        &self,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::ListToolsResponse, String>,
    > {
        Box::pin(async { Ok(crate::rpc::types::ListToolsResponse { tools: Vec::new() }) })
    }
    fn llm_chat(
        &self,
        _req: crate::rpc::types::LlmChatRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        Box::pin(async { Err("Not needed in mock".into()) })
    }
    fn execute_tool(
        &self,
        _req: crate::rpc::types::ToolExecuteRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::ToolExecuteResponse, String>,
    > {
        Box::pin(async {
            Ok(crate::rpc::types::ToolExecuteResponse {
                delivery: None,
                stop_turn: false,
                content: "ok".into(),
                is_error: false,
                note: None,
            })
        })
    }
    fn check_permission(
        &self,
        _req: crate::rpc::types::PermissionCheckRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::PermissionDecision, String>,
    > {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }
}

#[tokio::test]
async fn test_persistent_spawn_multi_turn_and_usage() {
    let manager = Arc::new(SubagentManager::new());
    let llm = Arc::new(RecordingMockLlm::new());
    let callbacks = Arc::new(MockCallbacks);

    let id = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();

    let turn1 = manager
        .run_persistent_turn(&id, "First question", llm.clone(), callbacks.clone())
        .await
        .unwrap();
    assert_eq!(turn1.usage.total_tokens, 25);

    let turn2 = manager
        .run_persistent_turn(&id, "Second question", llm.clone(), callbacks.clone())
        .await
        .unwrap();
    assert_eq!(turn2.usage.total_tokens, 25);

    // Usage aggregates across turns.
    let usage = manager.get_persistent_usage(&id).await;
    assert_eq!(usage.input_tokens, 20);
    assert_eq!(usage.output_tokens, 30);
    assert_eq!(usage.total_tokens, 50);

    // History reuse: turn 1 saw system+user+injection (3 messages), turn 2
    // saw system+user+assistant+user+injection (5 messages). The
    // injection message is the turn-level date reminder.
    assert_eq!(llm.message_counts(), vec![3, 5]);

    // The instance is idle between turns and still listed.
    let inst = manager.get_instance(&id).await.unwrap();
    assert_eq!(inst.state, SubagentState::Idle);
    assert_eq!(manager.list().await.len(), 1);
}

#[tokio::test]
async fn test_persistent_destroy() {
    let manager = Arc::new(SubagentManager::new());
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);

    let id = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();

    assert!(manager.destroy_persistent(&id).await);
    // Destroy is idempotent: a second call reports nothing to remove.
    assert!(!manager.destroy_persistent(&id).await);

    assert!(manager.get_instance(&id).await.is_none());
    assert_eq!(manager.list().await.len(), 0);
    assert_eq!(manager.get_persistent_usage(&id).await.total_tokens, 0);

    let err = manager
        .run_persistent_turn(&id, "hi", llm.clone(), callbacks.clone())
        .await
        .unwrap_err();
    assert!(err.contains("Unknown persistent subagent"));
}

#[tokio::test]
async fn test_persistent_kill_blocks_further_turns() {
    let manager = Arc::new(SubagentManager::new());
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);

    let id = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();

    assert!(manager.kill(&id).await.unwrap());
    let inst = manager.get_instance(&id).await.unwrap();
    assert_eq!(inst.state, SubagentState::Terminated);

    let err = manager
        .run_persistent_turn(&id, "hi", llm.clone(), callbacks.clone())
        .await
        .unwrap_err();
    assert!(err.contains("terminated"));
}

#[tokio::test]
async fn test_persistent_kill_aborts_running_turn() {
    let manager = Arc::new(SubagentManager::new());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let llm = Arc::new(ToolThenBlockLlm {
        calls: AtomicU32::new(0),
        entered: entered.clone(),
        release: release.clone(),
    });
    let callbacks = Arc::new(OkToolCallbacks);

    let id = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();

    let mgr = manager.clone();
    let turn_id = id.clone();
    let turn_llm = llm.clone();
    let turn_callbacks = callbacks.clone();
    let task = tokio::spawn(async move {
        mgr.run_persistent_turn(&turn_id, "long turn", turn_llm, turn_callbacks)
            .await
    });

    // Wait until the second LLM call is in flight, then kill mid-turn.
    entered.notified().await;
    assert!(manager.kill(&id).await.unwrap());
    release.notify_one();

    let turn = task.await.unwrap().unwrap();
    assert!(matches!(
        turn.stop_reason,
        crate::turn_loop::types::LoopTurnStopReason::Aborted
    ));

    // Usage from the completed steps is still aggregated.
    assert_eq!(manager.get_persistent_usage(&id).await.total_tokens, 25);

    // The kill's Terminated state is not overwritten by turn cleanup.
    let inst = manager.get_instance(&id).await.unwrap();
    assert_eq!(inst.state, SubagentState::Terminated);
}

#[tokio::test]
async fn test_persistent_rejects_concurrent_turns() {
    let manager = Arc::new(SubagentManager::new());
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let llm = Arc::new(BlockingLlm {
        entered: entered.clone(),
        release: release.clone(),
    });
    let callbacks = Arc::new(MockCallbacks);

    let id = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();

    let mgr = manager.clone();
    let turn_id = id.clone();
    let turn_llm = llm.clone();
    let turn_callbacks = callbacks.clone();
    let task = tokio::spawn(async move {
        mgr.run_persistent_turn(&turn_id, "first", turn_llm, turn_callbacks)
            .await
    });

    // Hold the first turn open and verify the second is rejected.
    entered.notified().await;
    let err = manager
        .run_persistent_turn(&id, "second", llm.clone(), callbacks.clone())
        .await
        .unwrap_err();
    assert!(err.contains("already running"));

    release.notify_one();
    let turn = task.await.unwrap().unwrap();
    assert!(matches!(
        turn.stop_reason,
        crate::turn_loop::types::LoopTurnStopReason::EndTurn
    ));

    let inst = manager.get_instance(&id).await.unwrap();
    assert_eq!(inst.state, SubagentState::Idle);
}

#[tokio::test]
async fn test_resume_from_persistent_instance() {
    let manager = Arc::new(SubagentManager::new());
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);
    manager
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    let id = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();

    // Check resume_profile resolves it
    let profile = manager.resume_profile(&id).await;
    assert_eq!(profile.as_deref(), Some("research"));

    // Resume foreground turn continues successfully
    let outcome = manager
        .resume_foreground_turn(&id, "continue research", None)
        .await;
    assert!(outcome.is_some());
    let res = outcome.unwrap().unwrap();
    assert!(matches!(res, ForegroundTurnOutcome::Completed(_)));
}

#[tokio::test]
async fn test_cold_resume_from_sqlite_store() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let manager1 = Arc::new(SubagentManager::with_store(store.clone()));
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);
    manager1
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    let agent_id = "subagent-cold-1";
    let initial_msgs = vec![
        crate::turn_loop::types::LLMMessage::new("user", "first question"),
        crate::turn_loop::types::LLMMessage::new("assistant", "first answer"),
    ];
    manager1.set_foreground_history(agent_id, "research", "Researcher", initial_msgs.clone());

    // Verify history is retrievable
    assert_eq!(manager1.get_foreground_history(agent_id).unwrap().len(), 2);
    assert_eq!(
        manager1.resume_profile(agent_id).await.as_deref(),
        Some("research")
    );

    // Now simulate a full restart: create a new SubagentManager with no in-memory state
    let manager2 = Arc::new(SubagentManager::with_store(store.clone()));
    manager2
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    // In-memory histories are empty in manager2
    assert_eq!(
        manager2.resume_profile(agent_id).await.as_deref(),
        Some("research")
    );
    let history = manager2.get_foreground_history(agent_id);
    assert!(history.is_some());
    assert_eq!(history.unwrap().len(), 2);

    // Resume should work seamlessly from persisted state
    let outcome = manager2
        .resume_foreground_turn(agent_id, "second question", None)
        .await;
    assert!(outcome.is_some());
    let res = outcome.unwrap().unwrap();
    assert!(matches!(res, ForegroundTurnOutcome::Completed(_)));

    // The updated history should now have 4 messages (user, assistant, user, assistant)
    let updated_history = manager2.get_foreground_history(agent_id).unwrap();
    assert_eq!(updated_history.len(), 6);
    assert_eq!(updated_history.last().unwrap().role, "assistant");
}

#[tokio::test]
async fn test_spawn_and_run_registers_in_task_runner() {
    let manager = Arc::new(SubagentManager::new());
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    manager.set_task_runner(runner.clone()).await;

    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(OkToolCallbacks);

    let id = manager
        .spawn_and_run("research", "Researcher", "investigate", llm, callbacks)
        .await
        .unwrap();

    let wait_res = runner.wait(&id, 2000).await;
    assert!(matches!(
        wait_res,
        crate::storage::TaskWaitResult::Completed(_)
    ));
}

#[test]
fn profile_policy_matches_mcp_tool_names_as_globs() {
    // v2 `isToolActive`: MCP names match their patterns as globs, while
    // built-ins match exactly. The built-in profiles whitelist `mcp__*`,
    // so a server-qualified MCP tool must survive that pattern.
    let filter = ToolPolicyFilter::from_allowlist(&["mcp__*".to_string()]);
    assert!(filter.allows("mcp__github__search"));
    assert!(filter.allows("mcp__acme__do_thing"));
    assert!(!filter.allows("Read"));

    let scoped = ToolPolicyFilter::from_allowlist(&["Read".into(), "mcp__github__*".into()]);
    assert!(scoped.allows("read"), "built-ins match case-insensitively");
    assert!(scoped.allows("mcp__github__search"));
    assert!(!scoped.allows("mcp__slack__post"));

    // A denylist keeps built-ins unless named, and globs MCP names.
    let denied = ToolPolicyFilter::from_definition(&SubagentDefinition {
        name: "n".into(),
        description: "d".into(),
        system_prompt: "s".into(),
        tools: Vec::new(),
        disallowed_tools: vec!["mcp__slack__*".into()],
        when_to_use: None,
        prompt_prefix: None,
        summary_policy: None,
        model: None,
        subagents: None,
    });
    assert!(!denied.allows("mcp__slack__post"));
    assert!(denied.allows("mcp__github__search"));
    assert!(denied.allows("Write"));
}

#[tokio::test]
async fn background_task_events_attribute_to_the_runtime_session() {
    let manager = Arc::new(SubagentManager::new());
    let runner = Arc::new(crate::storage::TaskRunner::new(None));
    manager.set_task_runner(runner.clone()).await;
    type RecordedEvents = Arc<std::sync::Mutex<Vec<(Option<String>, serde_json::Value)>>>;
    let events: RecordedEvents = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = events.clone();
    runner.set_event_sink(Arc::new(move |session, event| {
        sink.lock()
            .unwrap()
            .push((session.map(str::to_string), event));
    }));

    manager
        .set_runtime(
            Arc::new(MockSubagentLlm),
            Arc::new(OkToolCallbacks),
            Some("sess-runtime".into()),
        )
        .await;
    let id = manager
        .spawn_and_run(
            "research",
            "Researcher",
            "investigate",
            Arc::new(MockSubagentLlm),
            Arc::new(OkToolCallbacks),
        )
        .await
        .unwrap();

    // The creation event lands on the runtime's session lane, carrying
    // the subagent identity.
    let first = events.lock().unwrap()[0].clone();
    assert_eq!(first.0.as_deref(), Some("sess-runtime"));
    assert_eq!(first.1["type"], "event.task.created");
    assert_eq!(first.1["task"]["id"], id);
    assert_eq!(first.1["task"]["kind"], "subagent");
    assert_eq!(first.1["task"]["session_id"], "sess-runtime");

    assert!(matches!(
        runner.wait(&id, 2000).await,
        crate::storage::TaskWaitResult::Completed(_)
    ));
}

/// A manager whose completed-scope cache uses explicit knobs instead of
/// the process environment (which the tests must not mutate).
fn manager_with_capacity(store: Arc<SqliteSessionStore>, capacity: usize) -> Arc<SubagentManager> {
    let manager = Arc::new(SubagentManager::with_store(store));
    *manager.scope_cache.lock().unwrap() = ScopeCache {
        capacity,
        evict_timeout: Duration::from_millis(DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS),
        retired: Vec::new(),
        error: None,
    };
    manager
}

#[test]
fn test_scope_cache_env_validation() {
    let mut env = HashMap::new();
    assert_eq!(
        resolve_subagent_scope_cache_size(&env).unwrap(),
        DEFAULT_SUBAGENT_SCOPE_CACHE_SIZE
    );
    env.insert(SUBAGENT_SCOPE_CACHE_SIZE_ENV.into(), " 4 ".into());
    assert_eq!(resolve_subagent_scope_cache_size(&env).unwrap(), 4);
    // `0` and negative sizes both mean "never evict" (v2 `Math.max(0, …)`).
    env.insert(SUBAGENT_SCOPE_CACHE_SIZE_ENV.into(), "0".into());
    assert_eq!(resolve_subagent_scope_cache_size(&env).unwrap(), 0);
    env.insert(SUBAGENT_SCOPE_CACHE_SIZE_ENV.into(), "-3".into());
    assert_eq!(resolve_subagent_scope_cache_size(&env).unwrap(), 0);
    env.insert(SUBAGENT_SCOPE_CACHE_SIZE_ENV.into(), "many".into());
    assert!(resolve_subagent_scope_cache_size(&env).is_err());

    let mut env = HashMap::new();
    assert_eq!(
        resolve_subagent_scope_evict_timeout_ms(&env).unwrap(),
        DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS
    );
    env.insert(SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV.into(), "250".into());
    assert_eq!(resolve_subagent_scope_evict_timeout_ms(&env).unwrap(), 250);
    env.insert(SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV.into(), "0".into());
    assert!(resolve_subagent_scope_evict_timeout_ms(&env).is_err());
    env.insert(SUBAGENT_SCOPE_EVICT_TIMEOUT_ENV.into(), "-1".into());
    assert!(resolve_subagent_scope_evict_timeout_ms(&env).is_err());
}

/// A rejected env value fails the spawn that would create the first
/// cached scope instead of silently running without eviction.
#[tokio::test]
async fn test_invalid_scope_cache_env_fails_the_spawn() {
    let manager = Arc::new(SubagentManager::new());
    manager.scope_cache.lock().unwrap().error = Some(format!(
        "{SUBAGENT_SCOPE_CACHE_SIZE_ENV} must be an integer, got \"many\"."
    ));

    let error = manager
        .spawn_with_id("subagent-bad-env", "research", "Researcher")
        .await
        .unwrap_err();
    assert!(
        error.contains(SUBAGENT_SCOPE_CACHE_SIZE_ENV),
        "got: {error}"
    );
    assert!(manager.get_instance("subagent-bad-env").await.is_none());
}

/// A completed scope is evicted once the cache overflows, and the next
/// resume rebuilds it from the persisted record instead of failing.
#[tokio::test]
async fn test_completed_scopes_evict_and_rebuild_on_resume() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let manager = manager_with_capacity(store, 1);
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);
    manager
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    for id in ["subagent-lru-1", "subagent-lru-2"] {
        manager
            .spawn_with_id(id, "research", "Researcher")
            .await
            .unwrap();
        let outcome = manager
            .run_foreground_turn(id, "first question", None)
            .await
            .unwrap();
        assert!(matches!(outcome, ForegroundTurnOutcome::Completed(_)));
    }

    // The older completion is gone from memory; the newer one stays.
    assert!(manager.get_instance("subagent-lru-1").await.is_none());
    assert!(
        !manager
            .foreground_histories
            .lock()
            .unwrap()
            .contains_key("subagent-lru-1")
    );
    assert!(manager.get_instance("subagent-lru-2").await.is_some());

    // The persisted record survives eviction, so the resume rebuilds the
    // scope and continues the conversation.
    assert_eq!(
        manager.resume_profile("subagent-lru-1").await.as_deref(),
        Some("research")
    );
    let outcome = manager
        .resume_foreground_turn("subagent-lru-1", "second question", None)
        .await;
    assert!(outcome.is_some());
    assert!(matches!(
        outcome.unwrap().unwrap(),
        ForegroundTurnOutcome::Completed(_)
    ));
    assert!(manager.get_instance("subagent-lru-1").await.is_some());
    // The resumed scope is the most recently used one, so the other
    // completed scope is the one evicted now.
    assert!(manager.get_instance("subagent-lru-2").await.is_none());
}

/// Without a persisted resume record the in-memory conversation is the
/// only copy, so nothing is evicted (the cache stays unbounded there).
#[tokio::test]
async fn test_completed_scopes_stay_resident_without_a_store() {
    let manager = Arc::new(SubagentManager::new());
    *manager.scope_cache.lock().unwrap() = ScopeCache {
        capacity: 1,
        evict_timeout: Duration::from_millis(DEFAULT_SUBAGENT_SCOPE_EVICT_TIMEOUT_MS),
        retired: Vec::new(),
        error: None,
    };
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);
    manager
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    for id in ["subagent-keep-1", "subagent-keep-2"] {
        manager
            .spawn_with_id(id, "research", "Researcher")
            .await
            .unwrap();
        let outcome = manager
            .run_foreground_turn(id, "first question", None)
            .await
            .unwrap();
        assert!(matches!(outcome, ForegroundTurnOutcome::Completed(_)));
    }

    assert!(manager.get_instance("subagent-keep-1").await.is_some());
    assert!(manager.get_instance("subagent-keep-2").await.is_some());
}

/// A persistent instance is never evicted: its conversation lives only in
/// memory, so `destroy_persistent` stays its single removal path.
#[tokio::test]
async fn test_persistent_instances_are_never_evicted() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let manager = manager_with_capacity(store, 1);
    let llm = Arc::new(MockSubagentLlm);
    let callbacks = Arc::new(MockCallbacks);
    manager
        .set_runtime(llm.clone(), callbacks.clone(), None)
        .await;

    let persistent = manager
        .spawn_persistent("research", "Researcher", llm.clone(), callbacks.clone())
        .await
        .unwrap();
    manager
        .update_state(&persistent, SubagentState::Failed, None)
        .await;
    for id in ["subagent-fg-1", "subagent-fg-2"] {
        manager
            .spawn_with_id(id, "research", "Researcher")
            .await
            .unwrap();
        manager
            .update_state(id, SubagentState::Completed, None)
            .await;
    }

    assert!(manager.get_instance(&persistent).await.is_some());
}

/// B6 回归：ToolFilterCallbacks 必须给跨 wrapper 的事件打上子代理的
/// agent_id。native LLM 的 stream sink 发出的 `llm.delta` 不携带任何
/// 归属字段，SDK 的归属回退会把无 `agent_id` 的事件解析成 `"main"`，
/// 子代理的正文/思考流便整体泄进主对话。
#[test]
fn tool_filter_callbacks_stamp_the_subagent_agent_id_on_events() {
    struct RecordingInner {
        events: std::sync::Mutex<Vec<serde_json::Value>>,
    }
    impl crate::callbacks::HostCallbacks for RecordingInner {
        fn llm_chat(
            &self,
            _req: crate::rpc::types::LlmChatRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::LlmChatResponse, String>,
        > {
            Box::pin(async { Err("Not needed in mock".into()) })
        }
        fn execute_tool(
            &self,
            _req: crate::rpc::types::ToolExecuteRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async { Err("Not needed in mock".into()) })
        }
        fn check_permission(
            &self,
            _req: crate::rpc::types::PermissionCheckRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Err("Not needed in mock".into()) })
        }
        fn emit_event(&self, event: serde_json::Value) {
            self.events.lock().unwrap().push(event);
        }
    }

    use crate::callbacks::HostCallbacks;
    let inner = Arc::new(RecordingInner {
        events: std::sync::Mutex::new(Vec::new()),
    });
    let wrapper = ToolFilterCallbacks {
        inner: inner.clone(),
        filter: ToolPolicyFilter::from_allowlist(&["Read".to_string()]),
        agent_id: "subagent-1".to_string(),
    };

    // 1. 无 agent_id 的 llm.delta（native LLM sink 的原始形状）被打标。
    wrapper.emit_event(serde_json::json!({
        "type": "llm.delta",
        "part": { "type": "text", "text": "hi" },
    }));
    // 2. 已带 agent_id 的事件不被覆盖。
    wrapper.emit_event(serde_json::json!({
        "type": "tool.call.started",
        "agent_id": "other",
    }));

    let recorded = inner.events.lock().unwrap();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0]["agent_id"], "subagent-1");
    assert_eq!(recorded[0]["type"], "llm.delta");
    assert_eq!(recorded[1]["agent_id"], "other", "已有归属不被覆盖");
}

/// The profile's tool table is a hint to the model, not a boundary: a
/// subagent can name a tool it was never offered. The wrapper therefore
/// re-checks the profile on the call path, the same way the global
/// `[tools]` switch is re-checked in the toolset before a native call.
///
/// The inner mock answers `Err("Not used")`, so an `Err` here proves the
/// call reached it and an `Ok` proves the wrapper refused first.
#[tokio::test]
async fn a_profile_refuses_a_tool_call_it_never_advertised() {
    use crate::callbacks::HostCallbacks;

    struct Inner;
    impl crate::callbacks::HostCallbacks for Inner {
        fn llm_chat(
            &self,
            _req: crate::rpc::types::LlmChatRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::LlmChatResponse, String>,
        > {
            Box::pin(async { Err("Not used in mock".into()) })
        }
        fn execute_tool(
            &self,
            req: crate::rpc::types::ToolExecuteRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::ToolExecuteResponse, String>,
        > {
            Box::pin(async move { Err(format!("reached inner: {}", req.tool_name)) })
        }
        fn check_permission(
            &self,
            _req: crate::rpc::types::PermissionCheckRequest,
        ) -> futures_util::future::BoxFuture<
            'static,
            Result<crate::rpc::types::PermissionDecision, String>,
        > {
            Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
        }
    }

    let call = |name: &str| crate::rpc::types::ToolExecuteRequest {
        turn_id: "t1".into(),
        agent_id: "subagent-1".into(),
        tool_call_id: "c1".into(),
        tool_name: name.to_string(),
        arguments: serde_json::json!({}),
    };

    let read_only = ToolFilterCallbacks {
        inner: Arc::new(Inner),
        filter: ToolPolicyFilter::from_allowlist(&["Read".to_string()]),
        agent_id: "subagent-1".to_string(),
    };

    // Outside the profile: refused by the wrapper, never executed.
    let refused = read_only
        .execute_tool(call("Bash"))
        .await
        .expect("wrapper answers");
    assert!(refused.is_error);
    assert!(
        refused.content.contains("not available to this agent"),
        "content: {}",
        refused.content
    );

    // Enforcement is case-insensitive for built-ins, so the lowercase wire
    // name a model actually emits is refused for an *excluded* tool too.
    let wire = read_only
        .execute_tool(call("bash"))
        .await
        .expect("wrapper answers");
    assert!(wire.is_error, "the wire spelling must not slip past");

    // Inside the profile: the call is forwarded, and the inner mock's own
    // error is what comes back.
    let forwarded = read_only.execute_tool(call("Read")).await;
    let reached_host = forwarded
        .as_ref()
        .is_err_and(|e| e.as_str() == "reached inner: Read");
    assert!(
        reached_host,
        "an admitted tool must reach the host: {forwarded:?}"
    );

    // ...and the same case-insensitivity must not over-block it: `read` is
    // the wire spelling of an admitted tool, not a different one.
    let admitted_wire = read_only.execute_tool(call("read")).await;
    let reached_by_wire = admitted_wire
        .as_ref()
        .is_err_and(|e| e.as_str() == "reached inner: read");
    assert!(
        reached_by_wire,
        "the admitted tool's wire spelling must still run: {admitted_wire:?}"
    );
}

/// v2 `subagents.spawn({ callerAgentId })` records the parent as a label, and
/// `requireOwnedSubagent` reads it back to refuse a cross-parent resume.
/// The native engine has no labels, so `spawn_with_id` records the
/// `CALLER_AGENT_ID` scope the caller already runs in.
#[tokio::test]
async fn spawning_records_the_calling_agent_as_the_parent() {
    let manager = SubagentManager::new();
    manager
        .register_definition(SubagentDefinition {
            name: "coder".into(),
            description: String::new(),
            system_prompt: String::new(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;

    let id = crate::tools::CALLER_AGENT_ID
        .scope("caller-a".to_string(), manager.spawn("coder", "worker"))
        .await
        .unwrap();

    let parent = manager
        .parent_of(&id)
        .await
        .expect("a spawned subagent has a parent");
    assert_eq!(parent.agent_id, "caller-a");
    assert_eq!(parent.swarm_item, None);

    // A different caller does not inherit that ownership.
    let other = manager.parent_of("subagent-never-spawned").await;
    assert_eq!(other, None, "an unknown id has no parent");
}

/// An explicit `set_parent` wins over the ambient scope: the batch resolves
/// members on the scheduler's tasks, which do not inherit `CALLER_AGENT_ID`.
#[tokio::test]
async fn an_explicit_parent_overrides_the_ambient_scope() {
    let manager = SubagentManager::new();
    crate::tools::CALLER_AGENT_ID
        .scope("wrong-caller".to_string(), async {
            manager
                .set_parent(
                    "member-1",
                    "right-caller".to_string(),
                    Some("call-9".into()),
                    Some("item".into()),
                    Some(3),
                )
                .await;
        })
        .await;
    let parent = manager.parent_of("member-1").await.unwrap();
    assert_eq!(parent.agent_id, "right-caller");
    assert_eq!(parent.swarm_item.as_deref(), Some("item"));
}

/// The parent has to survive a restart, or a cold resume of a swarm member
/// would read as unowned and be refused (#3478 cold recovery).
#[tokio::test]
async fn the_parent_survives_a_cold_restart() {
    let store = Arc::new(SqliteSessionStore::in_memory().unwrap());
    let manager = SubagentManager::with_store(store.clone());
    manager
        .set_parent(
            "cold-1",
            "main".into(),
            Some("call-8".into()),
            Some("src/a.rs".into()),
            Some(2),
        )
        .await;
    manager.persist_resume_state("cold-1", "coder", "worker", Vec::new());

    // A fresh manager over the same store: the resident map is empty, so
    // this can only come from the persisted record.
    let restarted = SubagentManager::with_store(store);
    let parent = restarted
        .parent_of("cold-1")
        .await
        .expect("the persisted record names the parent");
    assert_eq!(parent.agent_id, "main");
    assert_eq!(parent.swarm_item.as_deref(), Some("src/a.rs"));
}

/// A record written before the parent existed reads back as unowned, and an
/// unowned id is refused rather than trusted.
#[tokio::test]
async fn a_pre_parent_record_reads_back_as_unowned() {
    let state: SubagentPersistedState = serde_json::from_value(serde_json::json!({
        "id": "legacy-1",
        "profile_name": "coder",
        "role": "worker",
        "messages": [],
        "updated_at": 0,
    }))
    .expect("an old record without the parent still deserializes");
    assert_eq!(state.parent_agent_id, None);
    assert_eq!(state.swarm_item, None);
}

/// Callbacks that expose a hook guard the way `NativeToolCallbacks` does in
/// the pipeline: the subagent turn reads its hooks from here, per session.
struct GuardedCallbacks {
    guard: Arc<crate::tools::external_hooks::HookGuard>,
}

impl crate::callbacks::HostCallbacks for GuardedCallbacks {
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        Some(self.guard.clone())
    }

    fn list_tools(
        &self,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::ListToolsResponse, String>,
    > {
        Box::pin(async { Ok(crate::rpc::types::ListToolsResponse { tools: Vec::new() }) })
    }

    fn llm_chat(
        &self,
        _req: crate::rpc::types::LlmChatRequest,
    ) -> futures_util::future::BoxFuture<'static, Result<crate::rpc::types::LlmChatResponse, String>>
    {
        Box::pin(async { Err("Not needed in mock".into()) })
    }

    fn execute_tool(
        &self,
        _req: crate::rpc::types::ToolExecuteRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::ToolExecuteResponse, String>,
    > {
        Box::pin(async { Err("Not needed in mock".into()) })
    }

    fn check_permission(
        &self,
        _req: crate::rpc::types::PermissionCheckRequest,
    ) -> futures_util::future::BoxFuture<
        'static,
        Result<crate::rpc::types::PermissionDecision, String>,
    > {
        Box::pin(async { Ok(crate::rpc::types::PermissionDecision::allow()) })
    }
}

/// A hook command that appends its event name to a file, portably through
/// the platform shell the guard uses (cmd.exe / sh).
fn marker_hook(event: &str, dir: &std::path::Path) -> crate::permission::HookDef {
    crate::permission::HookDef {
        event: event.into(),
        matcher: String::new(),
        command: format!("echo {event} > {event}-marker"),
        timeout: Some(10),
        cwd: Some(dir.to_string_lossy().into_owned()),
        env: None,
    }
}

/// Portable `exit 2` with a reason on stderr, as the Stop-veto tests use.
fn stop_veto(reason: &str) -> String {
    if cfg!(windows) {
        format!("echo {reason} 1>&2 & exit /b 2")
    } else {
        format!(
            "echo '{reason}' >&2; exit 2",
            reason = reason.replace('\'', "'\''")
        )
    }
}

/// Hook markers and subagent turns are produced by external hook
/// subprocesses, which the product allows up to `DEFAULT_HOOK_TIMEOUT_SECS`
/// each (30s) and a turn may fire several in sequence. The previous 2s
/// budget asserted "faster than the product's own allowance" and failed
/// whenever `cmd.exe` spawn was starved, so these waits use a bound that can
/// actually cover the work being observed.
const HOOK_VISIBILITY_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

async fn wait_for_marker(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + HOOK_VISIBILITY_WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "marker {} was never written within {HOOK_VISIBILITY_WAIT:?}",
            path.display()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn wait_for_completion(manager: &Arc<SubagentManager>, id: &str) {
    let deadline = std::time::Instant::now() + HOOK_VISIBILITY_WAIT;
    loop {
        if let Some(instance) = manager.get_instance(id).await
            && instance.state == SubagentState::Completed
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "subagent did not complete within {HOOK_VISIBILITY_WAIT:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// v2 instantiates `AgentExternalHooksService` per agent, so a subagent turn
/// fires its own `TurnStarted` / `UserPromptSubmit` / `Stop` hooks — not just
/// the main turn's.
#[tokio::test]
async fn test_subagent_turns_fire_lifecycle_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("TurnStarted-marker");
    let submitted = dir.path().join("UserPromptSubmit-marker");
    let stopped = dir.path().join("Stop-marker");
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        marker_hook("TurnStarted", dir.path()),
        marker_hook("UserPromptSubmit", dir.path()),
        marker_hook("Stop", dir.path()),
    ]));
    let manager = Arc::new(SubagentManager::new());

    let id = manager
        .spawn_and_run(
            "research",
            "Hook Scanner",
            "inspect the hook wiring",
            Arc::new(MockSubagentLlm),
            Arc::new(GuardedCallbacks { guard }),
        )
        .await
        .unwrap();
    wait_for_completion(&manager, &id).await;

    for (path, event) in [
        (&started, "TurnStarted"),
        (&submitted, "UserPromptSubmit"),
        (&stopped, "Stop"),
    ] {
        let text = wait_for_marker(path).await;
        assert!(
            text.contains(event),
            "{event} hook wrote {text:?} to {}",
            path.display()
        );
    }
}

/// The Stop hook's one-shot continuation belongs to the turn: a subagent
/// whose clean stop is vetoed re-prompts inside the same turn instead of
/// ending with the hook's reason dropped.
#[tokio::test]
async fn test_subagent_stop_hook_continuation_runs_inside_the_turn() {
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        crate::permission::HookDef {
            event: "Stop".into(),
            matcher: String::new(),
            command: stop_veto("keep going"),
            timeout: Some(10),
            cwd: None,
            env: None,
        },
    ]));
    let manager = Arc::new(SubagentManager::new());
    let calls = Arc::new(AtomicU32::new(0));
    let llm = Arc::new(CountingSubagentLlm {
        calls: calls.clone(),
    });

    let id = manager
        .spawn_and_run(
            "research",
            "Hook Scanner",
            "finish the task",
            llm,
            Arc::new(GuardedCallbacks { guard }),
        )
        .await
        .unwrap();
    wait_for_completion(&manager, &id).await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the veto re-prompts once inside the same turn"
    );
}

struct CountingSubagentLlm {
    calls: Arc<AtomicU32>,
}

impl crate::turn_loop::types::LLM for CountingSubagentLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }

    fn model_name(&self) -> &str {
        "mock-subagent-model"
    }

    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Ok(crate::turn_loop::types::LLMChatResponse {
                content: "Autonomous research result complete.".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: crate::rpc::types::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 15,
                    total_tokens: 25,
                    input_cache_read: 0,
                    input_cache_creation: 0,
                },
                timing: None,
            })
        })
    }
}

/// The foreground path wraps the runtime callbacks in `ToolFilterCallbacks`;
/// the wrapper must forward the hook guard, or a foreground subagent runs
/// unguarded while the background path fires hooks.
#[tokio::test]
async fn test_foreground_subagent_reads_the_guard_through_the_tool_filter() {
    let dir = tempfile::tempdir().unwrap();
    let submitted = dir.path().join("UserPromptSubmit-marker");
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        marker_hook("UserPromptSubmit", dir.path()),
    ]));
    let manager = Arc::new(SubagentManager::new());
    manager
        .set_runtime(
            Arc::new(MockSubagentLlm),
            Arc::new(GuardedCallbacks { guard }),
            None,
        )
        .await;
    let id = manager.spawn("research", "Hook Scanner").await.unwrap();

    manager
        .run_foreground_turn(&id, "inspect the hook wiring", None)
        .await
        .unwrap();
    let text = wait_for_marker(&submitted).await;
    assert!(text.contains("UserPromptSubmit"), "{text:?}");
}

struct FailingSubagentLlm;
impl crate::turn_loop::types::LLM for FailingSubagentLlm {
    fn system_prompt(&self) -> &str {
        "mock system prompt"
    }

    fn model_name(&self) -> &str {
        "mock-subagent-model"
    }

    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }

    fn chat(
        &self,
        _params: crate::turn_loop::types::LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<crate::turn_loop::types::LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        Box::pin(async { Err("provider is unavailable".into()) })
    }
}

/// v2 `notifyTurnEnded` per agent: a failed subagent turn fires `StopFailure`
/// (the main pump already did this for its own turns).
#[tokio::test]
async fn test_subagent_turn_failure_fires_stop_failure_hook() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("StopFailure-marker");
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        marker_hook("StopFailure", dir.path()),
    ]));
    let manager = Arc::new(SubagentManager::new());

    let id = manager
        .spawn_and_run(
            "research",
            "Hook Scanner",
            "fail the turn",
            Arc::new(FailingSubagentLlm),
            Arc::new(GuardedCallbacks { guard }),
        )
        .await
        .unwrap();
    let text = wait_for_marker(&marker).await;
    assert!(text.contains("StopFailure"), "{text:?}");
    assert!(
        manager
            .get_instance(&id)
            .await
            .is_some_and(|instance| instance.state == SubagentState::Failed),
        "the failed turn marks the instance failed"
    );
}

/// A parent cancel drops the turn future; v2 still reports the cancelled
/// turn as `Interrupt`, so the subagent's end-of-turn hooks stay symmetric
/// with the main turn's.
#[tokio::test]
async fn test_subagent_parent_cancel_fires_interrupt_hook() {
    struct GatedLlm {
        started: Arc<tokio::sync::Semaphore>,
    }
    impl crate::turn_loop::types::LLM for GatedLlm {
        fn system_prompt(&self) -> &str {
            "mock system prompt"
        }

        fn model_name(&self) -> &str {
            "mock-subagent-model"
        }

        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }

        fn chat(
            &self,
            _params: crate::turn_loop::types::LLMChatParams,
        ) -> crate::rpc::types::BoxFuture<
            '_,
            Result<
                crate::turn_loop::types::LLMChatResponse,
                Box<dyn std::error::Error + Send + Sync>,
            >,
        > {
            let started = self.started.clone();
            Box::pin(async move {
                started.add_permits(1);
                std::future::pending::<
                    Result<
                        crate::turn_loop::types::LLMChatResponse,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                >()
                .await
            })
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("Interrupt-marker");
    let guard = Arc::new(crate::tools::external_hooks::HookGuard::new(vec![
        marker_hook("Interrupt", dir.path()),
    ]));
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let manager = Arc::new(SubagentManager::new());
    manager
        .set_runtime(
            Arc::new(GatedLlm {
                started: started.clone(),
            }),
            Arc::new(GuardedCallbacks { guard }),
            None,
        )
        .await;
    let id = manager.spawn("research", "Hook Scanner").await.unwrap();

    let parent = Arc::new(crate::subagent::types::ParentCancel::new());
    let cancel = parent.clone();
    let trigger = tokio::spawn(async move {
        let _permit = started.acquire().await.unwrap();
        cancel.trigger();
    });
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        manager.run_foreground_turn(&id, "hang forever", Some(parent.as_ref())),
    )
    .await
    .expect("the parent cancel must release the turn")
    .unwrap();
    trigger.await.unwrap();
    assert!(matches!(outcome, ForegroundTurnOutcome::ParentCancelled));
    let text = wait_for_marker(&marker).await;
    assert!(text.contains("Interrupt"), "{text:?}");
}
