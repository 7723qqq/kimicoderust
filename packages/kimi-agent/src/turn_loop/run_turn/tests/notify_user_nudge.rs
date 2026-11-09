use super::super::*;
use super::common::*;
use super::*;

/// An LLM that runs `silent_rounds` rounds of one host tool call without ever
/// calling NotifyUser, then answers.
struct SilentRoundsLlm {
    call: AtomicU32,
    rounds: u32,
    requests: Arc<std::sync::Mutex<Vec<Vec<LLMMessage>>>>,
}

impl LLM for SilentRoundsLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "silent-rounds-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(params.messages.to_vec());
        let rounds = self.rounds;
        Box::pin(async move {
            if call < rounds {
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![ToolCall {
                        id: format!("tc{call}"),
                        name: "Read".into(),
                        arguments: serde_json::json!({ "path": "/a.rs" }),
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

fn tool_execution_server() -> Arc<RpcServer> {
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
    server
}

fn input<'a>(llm: &'a SilentRoundsLlm, tool_defs: Vec<ToolInfo>) -> RunTurnInput<'a> {
    RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-notify-user-nudge".into(),
        llm,
        messages: vec![LLMMessage {
            role: "user".into(),
            content: "keep going".into(),
            origin: Some(serde_json::json!({ "kind": "user" })),
            ..Default::default()
        }],
        tools: &[],
        tool_defs,
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
    }
}

/// v2 `notifyUserNudgeService`: after eight silent tool-call rounds the next
/// request carries the nudge. This goes through the real turn loop — the
/// provider is registered on the live registry and its text reaches the LLM
/// request, not just the pure decision functions.
#[tokio::test]
async fn eight_silent_rounds_inject_the_nudge_into_the_live_request() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = SilentRoundsLlm {
        call: AtomicU32::new(0),
        rounds: 8,
        requests: requests.clone(),
    };
    let callbacks = rpc_callbacks(tool_execution_server());
    let tool_defs = vec![ToolInfo {
        name: crate::tools::core_tool_defs::NOTIFY_USER_TOOL_NAME.to_string(),
        description: String::new(),
        input_schema: serde_json::json!({ "type": "object" }),
    }];

    let result = run_turn(input(&llm, tool_defs), &callbacks).await.unwrap();
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(
        recorded.len(),
        9,
        "eight tool rounds, then the closing step"
    );
    let nudge = |messages: &Vec<LLMMessage>| {
        messages.iter().any(|message| {
            message
                .content
                .contains("rounds of tool calls without a NotifyUser update")
        })
    };
    assert!(!nudge(&recorded[7]), "seven rounds is below the threshold");
    assert!(
        nudge(&recorded[8]),
        "the eighth silent round arms the nudge for the next request"
    );
}

/// The same eight rounds stay silent when NotifyUser was not advertised: the
/// provider is never registered, so the model is not told to use a tool it
/// cannot call.
#[tokio::test]
async fn an_unadvertised_notify_user_keeps_the_nudge_silent() {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let llm = SilentRoundsLlm {
        call: AtomicU32::new(0),
        rounds: 8,
        requests: requests.clone(),
    };
    let callbacks = rpc_callbacks(tool_execution_server());

    let result = run_turn(input(&llm, vec![]), &callbacks).await.unwrap();
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        recorded.iter().all(|messages| !messages
            .iter()
            .any(|message| message.content.contains("without a NotifyUser update"))),
        "without the tool the nudge would be unanswerable"
    );
}
