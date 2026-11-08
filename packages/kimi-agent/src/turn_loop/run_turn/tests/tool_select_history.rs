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

/// v2 #4719 / #4720: a history recorded while the model could load tools
/// must not keep telling a model without that capability to call
/// `select_tools`. The turn loop rewrites the prior announcement into the
/// runtime-change reminder before the provider sees it.
#[tokio::test]
async fn an_unsupported_prior_announcement_reaches_the_model_as_a_runtime_reminder() {
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
        messages: vec![LLMMessage::user(
            "<tools_added>\nmcp__srv__search\n</tools_added>",
        )],
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
    let reminder = sent
        .iter()
        .find(|message| message.role == "user")
        .expect("the user message is still sent");
    assert!(
        reminder
            .content
            .contains("Use the tool definitions provided in this request."),
        "the prior select_tools announcement must be rewritten: {:?}",
        reminder.content
    );
    assert!(
        reminder
            .content
            .contains("<tools_added>\nmcp__srv__search\n</tools_added>")
    );
    assert!(
        !reminder
            .content
            .contains("Only the announced names are loadable")
    );
}

/// v2 #4733 `notifyUpdatedTools`: a same-name MCP update stays pending and
/// the next turn carries one reminder that the update is not applied in this
/// session.
#[tokio::test]
async fn a_pinned_same_name_update_notifies_the_model_once() {
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(crate::mcp::McpManager::new());
    let old = vec![crate::mcp::types::McpTool {
        name: "search".into(),
        description: Some("old search".into()),
        input_schema: serde_json::json!({ "type": "object" }),
    }];
    manager
        .add_client(crate::mcp::McpClient::mock_with_tools("srv", old))
        .await;
    let updated = vec![crate::mcp::types::McpTool {
        name: "search".into(),
        description: Some("updated search".into()),
        input_schema: serde_json::json!({ "type": "object" }),
    }];
    manager
        .add_client(crate::mcp::McpClient::mock_with_tools("srv", updated))
        .await;
    let toolset = Arc::new(
        crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None)
            .unwrap()
            .with_mcp(manager)
            .with_tool_select_enabled(true)
            .with_model_capabilities(Some(vec!["dynamically_loaded_tools".into()])),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm = CapturingLlm {
        requests: Arc::clone(&requests),
    };
    let input = RunTurnInput {
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "tool-update-once".into(),
        llm: &llm,
        messages: vec![LLMMessage::user("continue")],
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
        sent.iter().any(|message| message
            .content
            .contains("has an update, but the update is not applied in this session")),
        "the pinned update reminder must reach the model: {sent:?}"
    );
}
