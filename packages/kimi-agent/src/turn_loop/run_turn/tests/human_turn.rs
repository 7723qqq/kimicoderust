use super::super::*;
use super::common::*;
use super::*;

struct InterruptedLlm {
    response: LLMChatResponse,
    entered: Arc<tokio::sync::Notify>,
}

impl LLM for InterruptedLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }

    fn model_name(&self) -> &str {
        "interrupted-model"
    }

    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn take_interrupted_response(&self) -> Option<LLMChatResponse> {
        Some(self.response.clone())
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        self.entered.notify_one();
        Box::pin(async move {
            if let Some(cancel) = params.cancel {
                cancel.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
            Err("llm cancelled: request aborted".into())
        })
    }
}

fn interrupted_response() -> LLMChatResponse {
    LLMChatResponse {
        content: "partial answer".into(),
        thinking: vec![
            ContentBlock::Think {
                think: "   ".into(),
                encrypted: None,
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            },
            ContentBlock::Think {
                think: String::new(),
                encrypted: Some(String::new()),
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            },
            ContentBlock::Think {
                think: "reasoning".into(),
                encrypted: Some("signature".into()),
                details_index: None,
                reasoning_key: None,
                details_summary: None,
            },
        ],
        tool_calls: vec![ToolCall {
            id: "partial-call".into(),
            name: "Bash".into(),
            arguments: serde_json::json!({ "command": "echo partial" }),
            extras: None,
            args_parse_error: None,
        }],
        finish_reason: None,
        usage: TokenUsage::default(),
        timing: None,
    }
}

#[tokio::test]
async fn cancelled_stream_salvages_only_non_vacuous_assistant_content() {
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let llm = Arc::new(InterruptedLlm {
        response: interrupted_response(),
        entered: Arc::new(tokio::sync::Notify::new()),
    });
    let cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let task_llm = llm.clone();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        let input = RunTurnInput {
            llm_request_byte_budget: u64::MAX,
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            previous_turn_aborted: false,
            turn_id: "test-cancel-salvage".into(),
            llm: task_llm.as_ref(),
            messages: vec![LLMMessage::user("start")],
            tools: &[],
            tool_defs: vec![],
            max_steps: 2,
            max_attempts: Some(1),
            max_context_tokens: None,
            compaction_max_attempts: None,
            permission_mode: None,
            goal: None,
            cancellation: Some(task_cancellation),
            hook_guard: None,
            media: None,
            media_dropped: None,
            media_recovery: None,
            toolset: None,
        };
        run_turn(input, &callbacks).await.unwrap()
    });
    llm.entered.notified().await;
    cancellation.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = task.await.unwrap();

    assert_eq!(result.stop_reason, LoopTurnStopReason::Aborted);
    let assistants: Vec<&LLMMessage> = result
        .messages
        .iter()
        .filter(|message| message.role == "assistant")
        .collect();
    assert_eq!(assistants.len(), 1, "the cancelled attempt enters history");
    let salvaged = assistants[0];
    assert_eq!(salvaged.content, "partial answer");
    assert!(salvaged.tool_calls.is_empty());
    assert_eq!(salvaged.origin.as_ref().unwrap()["source"], "salvaged");
    assert_eq!(salvaged.blocks.len(), 2, "vacuous thinking is filtered");
    assert!(matches!(
        &salvaged.blocks[0],
        ContentBlock::Think { encrypted: Some(value), .. } if value.is_empty()
    ));
    assert!(matches!(
        &salvaged.blocks[1],
        ContentBlock::Think { think, encrypted: Some(value), .. }
            if think == "reasoning" && value == "signature"
    ));
}
