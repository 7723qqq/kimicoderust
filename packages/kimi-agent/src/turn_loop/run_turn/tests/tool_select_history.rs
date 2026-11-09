use super::super::*;
use super::common::*;
use super::*;

struct CapturingLlm {
    requests: Arc<Mutex<Vec<Vec<LLMMessage>>>>,
}

impl LLM for CapturingLlm {
    fn system_prompt(&self) -> &str {
        "system"
    }

    fn model_name(&self) -> &str {
        "capturing"
    }

    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(params.messages.to_vec());
        Box::pin(async move {
            Ok(LLMChatResponse {
                content: "done".into(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: crate::rpc::types::TokenUsage::default(),
                timing: None,
            })
        })
    }
}

/// v2 `shapeHistory` (`toolSelectService.ts:125-130`): a model without
/// dynamic tool loading must not see a prior `<tools_added>` announcement
/// telling it to call `select_tools`, so the announcement is stripped before
/// the provider sees it.
#[tokio::test]
async fn an_unsupported_prior_announcement_is_stripped_from_history() {
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let dir = tempfile::tempdir().unwrap();
    // No `dynamically_loaded_tools` capability and the flag off: unsupported.
    let toolset =
        Arc::new(crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm = CapturingLlm {
        requests: Arc::clone(&requests),
    };

    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "tool-select-history".into(),
        llm: &llm,
        messages: vec![
            crate::injection::injection_message_named(
                crate::injection::wrap_system_reminder(
                    "<tools_added>\nmcp__srv__search\n</tools_added>",
                ),
                crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
            ),
            LLMMessage::user("hello"),
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
        toolset: Some(toolset),
    };

    run_turn(input, &callbacks).await.unwrap();
    let requests = requests.lock().unwrap();
    let sent = requests.first().expect("one provider request");
    assert!(
        !sent
            .iter()
            .any(|message| message.content.contains("<tools_added>")),
        "the stale announcement must be stripped: {sent:?}"
    );
    assert!(
        sent.iter().any(|message| message.content == "hello"),
        "the real prompt survives: {sent:?}"
    );
}

/// v2 `shapeHistory`: with dynamic loading on, only the newest
/// `<tools_relevant>` block survives; an older one would describe a stale
/// catalogue.
#[tokio::test]
async fn only_the_newest_retrieved_announcement_survives_shaping() {
    fn retrieved(text: &str) -> LLMMessage {
        crate::injection::injection_message_named(
            crate::injection::wrap_system_reminder(text),
            crate::tools::tool_select_state::LOADABLE_TOOLS_VARIANT,
        )
    }
    let messages = vec![
        retrieved("<tools_relevant>\nold — first\n</tools_relevant>"),
        LLMMessage::user("continue"),
        retrieved("<tools_relevant>\nnew — second\n</tools_relevant>"),
    ];
    let shaped = crate::tools::tool_select_state::shape_history(messages, true);
    assert_eq!(shaped.len(), 2);
    assert!(shaped[1].content.contains("new — second"));
    assert!(!shaped.iter().any(|m| m.content.contains("old — first")));
}
