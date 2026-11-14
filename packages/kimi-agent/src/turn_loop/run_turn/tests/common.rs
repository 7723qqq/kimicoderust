use super::super::*;
use super::*;

pub(super) fn rpc_callbacks(server: Arc<RpcServer>) -> Arc<dyn HostCallbacks> {
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

pub(super) struct EventCapturingCallbacks {
    pub(super) inner: Arc<dyn HostCallbacks>,
    pub(super) events: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl EventCapturingCallbacks {
    pub(super) fn new(inner: Arc<dyn HostCallbacks>) -> (Self, Arc<Mutex<Vec<serde_json::Value>>>) {
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

pub(super) struct PredictTestLlm {
    pub(super) system_prompt: String,
    pub(super) model_name: String,
    pub(super) return_tool_calls: bool,
    pub(super) tool_responses: Vec<ToolCall>,
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

pub(super) struct SequencedLlm {
    pub(super) system_prompt: String,
    pub(super) model_name: String,
    pub(super) steps: Vec<(Vec<ToolCall>, &'static str)>,
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

pub(super) fn stop_veto_command(reason: &str) -> String {
    if cfg!(windows) {
        format!("echo {reason} 1>&2 & exit /b 2")
    } else {
        format!(
            "echo '{reason}' >&2; exit 2",
            reason = reason.replace('\'', "'\\''")
        )
    }
}

pub(super) fn stop_hook_guard() -> Arc<crate::tools::external_hooks::HookGuard> {
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

pub(super) struct FixedResponseLlm {
    pub(super) finish_reason: String,
    pub(super) content: String,
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
