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
                usage: kimi_core::types::TokenUsage::default(),
                timing: None,
            })
        })
    }
}

#[tokio::test]
async fn an_unsupported_prior_announcement_becomes_a_runtime_reminder() {
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server.clone());
    let dir = tempfile::tempdir().unwrap();

    let toolset =
        Arc::new(crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm = CapturingLlm {
        requests: Arc::clone(&requests),
    };

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "tool-select-history".into(),
        llm: &llm,
        messages: vec![
            crate::injection::injection_message_named(
                crate::injection::wrap_system_reminder(
                    "<tools_added>\nmcp__srv__search\n</tools_added>",
                ),
                crate::tools::tool_select_state::RETRIEVED_TOOLS_VARIANT,
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
    let expected = crate::injection::wrap_system_reminder(
        &crate::tools::tool_select_state::render_runtime_tool_change_reminder(
            &["mcp__srv__search".to_string()],
            &[],
        ),
    );
    assert!(
        sent.iter().any(|message| message.content == expected),
        "the stale announcement becomes a runtime reminder: {sent:?}"
    );
    assert!(
        sent.iter().any(|message| message.content == "hello"),
        "the real prompt survives: {sent:?}"
    );
}

#[tokio::test]
async fn only_the_newest_retrieved_announcement_survives_shaping() {
    fn retrieved(text: &str) -> LLMMessage {
        crate::injection::injection_message_named(
            crate::injection::wrap_system_reminder(text),
            crate::tools::tool_select_state::RETRIEVED_TOOLS_VARIANT,
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

struct PreparingCallbacks {
    inner: Arc<dyn HostCallbacks>,
    toolset: Arc<crate::tools::NativeToolset>,
}

impl HostCallbacks for PreparingCallbacks {
    fn llm_chat(
        &self,
        request: types::LlmChatRequest,
    ) -> BoxFuture<'static, Result<types::LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }

    fn execute_tool(
        &self,
        request: types::ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<types::ToolExecuteResponse, String>> {
        self.inner.execute_tool(request)
    }

    fn check_permission(
        &self,
        request: types::PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<types::PermissionDecision, String>> {
        self.inner.check_permission(request)
    }

    fn list_tools(&self) -> BoxFuture<'static, Result<types::ListToolsResponse, String>> {
        let toolset = self.toolset.clone();
        Box::pin(async move {
            let active = vec![crate::tools::tool_select_ops::ToolSelectTool {
                name: "mcp__s__alpha".to_string(),
                description: "old".to_string(),
                parameters: serde_json::json!({"type": "object"})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                source: crate::tools::tool_select_ops::ToolSelectSource::Mcp,
                disclosure: Some(crate::tools::registry::ToolDisclosure::Inline),
                origin: crate::tools::tool_select_ops::ToolSelectOrigin::Initial,
            }];
            let mut live = active.clone();
            live[0].description = "new".to_string();
            toolset.reconcile_tool_select(&active, &live).await?;
            Ok(types::ListToolsResponse { tools: vec![] })
        })
    }
}

#[tokio::test]
async fn pending_tool_updates_land_in_the_same_steps_injection_pass() {
    let server = Arc::new(RpcServer::new());
    let dir = tempfile::tempdir().unwrap();
    let toolset =
        Arc::new(crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(PreparingCallbacks {
        inner: rpc_callbacks(server),
        toolset: toolset.clone(),
    });
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm = CapturingLlm {
        requests: requests.clone(),
    };
    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "tool-update-pending".into(),
        llm: &llm,
        messages: vec![LLMMessage::user("hello")],
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
    let sent = requests.first().unwrap();
    let reminder = sent
        .iter()
        .find(|message| {
            message
                .origin
                .as_ref()
                .and_then(|origin| origin.get("variant"))
                == Some(&serde_json::Value::String(
                    "tool_update_pending".to_string(),
                ))
        })
        .expect("the update reminder lands before this step's request");
    assert!(reminder.content.contains(
        "Tool \"mcp__s__alpha\" has an update, but the update is not applied in this session."
    ));
}
