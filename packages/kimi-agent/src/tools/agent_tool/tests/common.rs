use super::super::*;
use super::*;

pub(super) struct SummaryLlm;
impl LLM for SummaryLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "summary-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<TurnChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async {
            Ok(TurnChatResponse {
                content: "findings: the loop is in run_turn.rs".into(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

pub(super) struct ToolForeverLlm;
impl LLM for ToolForeverLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "tool-forever-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<TurnChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async {
            Ok(TurnChatResponse {
                content: String::new(),
                thinking: vec![],
                tool_calls: vec![ToolCall {
                    id: format!("tc-{}", fastrand::u64(..)),
                    name: "echo".into(),
                    arguments: serde_json::json!({}),
                    extras: None,
                    args_parse_error: None,
                }],
                finish_reason: Some("tool_calls".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

pub(super) struct NoopCallbacks;
impl HostCallbacks for NoopCallbacks {
    fn llm_chat(&self, _: LlmChatRequest) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn execute_tool(
        &self,
        _: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        Box::pin(async {
            Ok(ToolExecuteResponse {
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
        _: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        Box::pin(async { Ok(PermissionDecision::allow()) })
    }
    fn ask_question(
        &self,
        _: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        Box::pin(async { Err("not used".into()) })
    }
    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        Box::pin(async {
            Ok(ListToolsResponse {
                tools: ["echo", "TaskList", "TaskOutput", "TaskStop"]
                    .into_iter()
                    .map(|name| ToolInfo {
                        name: name.into(),
                        description: name.into(),
                        input_schema: serde_json::json!({}),
                    })
                    .collect(),
            })
        })
    }
}

pub(super) struct MissingTaskOutputCallbacks;
impl HostCallbacks for MissingTaskOutputCallbacks {
    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        NoopCallbacks.llm_chat(request)
    }
    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        NoopCallbacks.execute_tool(request)
    }
    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        NoopCallbacks.check_permission(request)
    }
    fn ask_question(
        &self,
        request: AskQuestionRequest,
    ) -> BoxFuture<'static, Result<AskQuestionResponse, String>> {
        NoopCallbacks.ask_question(request)
    }
    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        Box::pin(async {
            Ok(ListToolsResponse {
                tools: ["echo", "TaskList", "TaskStop"]
                    .into_iter()
                    .map(|name| ToolInfo {
                        name: name.into(),
                        description: name.into(),
                        input_schema: serde_json::json!({}),
                    })
                    .collect(),
            })
        })
    }
}

pub(super) async fn manager_with(llm: Arc<dyn LLM>) -> Arc<SubagentManager> {
    let manager = Arc::new(SubagentManager::new());
    manager
        .set_runtime(llm, Arc::new(NoopCallbacks), None)
        .await;
    manager
}

pub(super) async fn register_looper(manager: &SubagentManager) {
    manager
        .register_discovered_definition(crate::subagent::types::SubagentDefinition {
            name: "looper".into(),
            description: "loops".into(),
            system_prompt: "You loop.".into(),
            tools: vec![],
            disallowed_tools: vec![],
            when_to_use: None,
            prompt_prefix: None,
            summary_policy: None,
            model: None,
            subagents: None,
        })
        .await;
}

pub(super) async fn as_main_agent<F: std::future::Future>(fut: F) -> F::Output {
    crate::tools::CALLER_AGENT_ID
        .scope(crate::callbacks::MAIN_AGENT_ID.to_string(), fut)
        .await
}

pub(super) struct RecordingPromptLlm {
    pub(super) prompts: Mutex<Vec<String>>,
    pub(super) messages_sent: Mutex<Vec<Vec<crate::turn_loop::types::LLMMessage>>>,

    pub(super) responses: Vec<String>,
    pub(super) calls: Mutex<usize>,

    pub(super) alias: Option<String>,
}

impl RecordingPromptLlm {
    pub(super) fn new(responses: Vec<String>) -> Self {
        Self {
            prompts: Mutex::new(Vec::new()),
            messages_sent: Mutex::new(Vec::new()),
            responses,
            calls: Mutex::new(0),
            alias: None,
        }
    }

    pub(super) fn with_alias(mut self, alias: &str) -> Self {
        self.alias = Some(alias.to_string());
        self
    }

    pub(super) fn call_count(&self) -> usize {
        *self.calls.lock().unwrap()
    }

    pub(super) fn first_prompt(&self) -> String {
        self.prompts.lock().unwrap()[0].clone()
    }
}

impl LLM for RecordingPromptLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "recording-prompt-llm"
    }
    fn model_alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<TurnChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let mut prompts = self.prompts.lock().unwrap();
        self.messages_sent
            .lock()
            .unwrap()
            .push(params.messages.to_vec());
        if let Some(last_user) = params
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user" && !m.content.starts_with("<system-reminder>"))
        {
            prompts.push(last_user.content.clone());
        }
        drop(prompts);
        let mut calls = self.calls.lock().unwrap();
        let index = (*calls).min(self.responses.len() - 1);
        *calls += 1;
        let content = self.responses[index].clone();
        drop(calls);
        Box::pin(async move {
            Ok(TurnChatResponse {
                content,
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("stop".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

pub(super) struct EventRecorder {
    pub(super) inner: NoopCallbacks,
    pub(super) events: Mutex<Vec<serde_json::Value>>,
    pub(super) guard: Option<Arc<crate::tools::external_hooks::HookGuard>>,
}

impl EventRecorder {
    pub(super) fn new() -> Self {
        Self {
            inner: NoopCallbacks,
            events: Mutex::new(Vec::new()),
            guard: None,
        }
    }

    pub(super) fn with_hook_guard(guard: Arc<crate::tools::external_hooks::HookGuard>) -> Self {
        Self {
            guard: Some(guard),
            ..Self::new()
        }
    }

    pub(super) fn types(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| event.get("type").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect()
    }
}

impl HostCallbacks for EventRecorder {
    fn list_tools(&self) -> BoxFuture<'static, Result<ListToolsResponse, String>> {
        self.inner.list_tools()
    }
    fn llm_chat(
        &self,
        request: LlmChatRequest,
    ) -> BoxFuture<'static, Result<LlmChatResponse, String>> {
        self.inner.llm_chat(request)
    }
    fn execute_tool(
        &self,
        request: ToolExecuteRequest,
    ) -> BoxFuture<'static, Result<ToolExecuteResponse, String>> {
        self.inner.execute_tool(request)
    }
    fn check_permission(
        &self,
        request: PermissionCheckRequest,
    ) -> BoxFuture<'static, Result<PermissionDecision, String>> {
        self.inner.check_permission(request)
    }
    fn emit_event(&self, event: serde_json::Value) {
        self.events.lock().unwrap().push(event);
    }
    fn hook_guard(&self) -> Option<Arc<crate::tools::external_hooks::HookGuard>> {
        self.guard.clone()
    }
}

pub(super) async fn manager_with_callbacks(
    llm: Arc<dyn LLM>,
    callbacks: Arc<dyn HostCallbacks>,
) -> Arc<SubagentManager> {
    let manager = Arc::new(SubagentManager::new());
    manager.set_runtime(llm, callbacks, None).await;
    manager
}

pub(super) fn pool_runtime(alias_llm: Arc<RecordingPromptLlm>) -> SecondaryModelRuntime {
    let config = SecondaryModelPool {
        default_model: "strong".into(),
        caller_model_alias: Some("main-model".into()),
        models: vec![SecondaryModelEntry {
            alias: "strong".into(),
            hint: "Pick this for hard problems.".into(),
            llm: Default::default(),
        }],
    };
    SecondaryModelRuntime::new(
        config,
        [(
            "strong".to_string(),
            alias_llm as Arc<dyn crate::turn_loop::types::LLM>,
        )]
        .into_iter()
        .collect(),
    )
}
