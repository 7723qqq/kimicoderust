use std::sync::Arc;

use crate::callbacks::HostCallbacks;
use crate::rpc::types::{self, LlmChatMessage};
use kimi_core::types::*;

pub struct HostLlmProxy {
    system_prompt: String,
    model_name: String,
    callbacks: Option<Arc<dyn HostCallbacks>>,

    request_id: Option<String>,

    tool_call_ids: std::sync::Mutex<Option<Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>>>,
}

impl HostLlmProxy {
    pub fn new(system_prompt: String, model_name: String) -> Self {
        Self {
            system_prompt,
            model_name,
            callbacks: None,
            request_id: None,
            tool_call_ids: std::sync::Mutex::new(None),
        }
    }

    pub fn with_callbacks(mut self, callbacks: Arc<dyn HostCallbacks>) -> Self {
        self.callbacks = Some(callbacks);
        self
    }

    pub fn with_request_id(mut self, request_id: String) -> Self {
        self.request_id = Some(request_id);
        self
    }

    pub fn with_server(self, server: Arc<crate::rpc::server::RpcServer>) -> Self {
        let cb = Arc::new(crate::callbacks::RpcHostCallbacks { server });
        self.with_callbacks(cb)
    }
}

impl LLM for HostLlmProxy {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn set_tool_call_ids(&self, ledger: Arc<kimi_core::tool_call_id::ToolCallIdNormalizer>) {
        *self.tool_call_ids.lock().unwrap_or_else(|e| e.into_inner()) = Some(ledger);
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn transport(&self) -> &'static str {
        "host-proxy"
    }

    fn is_retryable_error(&self, error: &str) -> bool {
        if crate::llm::http::is_cancelled_error(error) {
            return false;
        }
        if let Some(code) = kimi_core::error::llm_http_status(error) {
            return crate::llm::http::is_retryable_status_code(code);
        }

        const RETRYABLE: &[&str] = &[
            "status 429",
            "overloaded",
            "timed out",
            "timeout",
            "connect",
            "connection",
            "sse decode error",
            "econnreset",
            "econnrefused",
            "socket hang up",
        ];
        let lower = error.to_lowercase();
        RETRYABLE.iter().any(|s| lower.contains(s))
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> crate::rpc::types::BoxFuture<
        '_,
        Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>,
    > {
        let system_prompt = self.system_prompt.clone();
        let model_name = self.model_name.clone();
        let callbacks = self.callbacks.clone();
        let request_id = self.request_id.clone();

        Box::pin(async move {
            let Some(callbacks) = callbacks else {
                return Err("HostLlmProxy: callbacks not set".into());
            };

            let request = chat_request(system_prompt, model_name, request_id, &params);

            let response = callbacks.llm_chat(request).await.map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(std::io::Error::other(e))
                },
            )?;

            let tool_calls: Vec<ToolCall> = response
                .tool_calls
                .into_iter()
                .map(|tc| ToolCall {
                    id: tc.id,
                    name: tc.name,
                    arguments: tc.arguments,
                    extras: tc.extras,
                    args_parse_error: None,
                })
                .collect();

            let usage = crate::rpc::types::TokenUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                total_tokens: response.usage.total_tokens,
                input_cache_read: response.usage.input_cache_read,
                input_cache_creation: response.usage.input_cache_creation,
            };

            let mut result = LLMChatResponse {
                content: response.content,
                thinking: response.thinking,
                tool_calls,
                finish_reason: response.finish_reason,
                usage,
                timing: None,
            };

            let ledger = self
                .tool_call_ids
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(ledger) = ledger {
                kimi_core::tool_call_id::remap_response_tool_calls(
                    &mut result,
                    &ledger.begin_response(),
                );
            }
            Ok(result)
        })
    }
}

/// Project engine-side chat parameters onto the host wire request.
///
/// Kept separate from [`HostLlmProxy::chat`] so the mapping (including the
/// per-request output cap) is testable without a host callback object.
fn chat_request(
    system_prompt: String,
    model_name: String,
    request_id: Option<String>,
    params: &LLMChatParams,
) -> types::LlmChatRequest {
    let messages: Vec<LlmChatMessage> = params
        .messages
        .iter()
        .map(|m| LlmChatMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            blocks: m.blocks.clone(),
        })
        .collect();

    let tools: Vec<types::ToolDef> = params
        .tools
        .iter()
        .map(|t| types::ToolDef {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect();

    types::LlmChatRequest {
        system_prompt,
        model_name,
        messages,
        tools,
        request_id,
        max_output_tokens: params.effective_max_output_tokens(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(max_output_tokens: Option<u32>) -> LLMChatParams {
        LLMChatParams {
            messages: Arc::from([]),
            tools: Arc::from([]),
            cancel: None,
            used_context_tokens: None,
            max_context_tokens: None,
            max_output_tokens,
            max_output_size: None,
        }
    }

    #[test]
    fn the_host_proxy_forwards_the_per_request_output_cap() {
        let request = chat_request(
            "sys".into(),
            "model".into(),
            Some("req-1".into()),
            &params(Some(4096)),
        );

        assert_eq!(request.max_output_tokens, Some(4096));
        assert_eq!(request.request_id.as_deref(), Some("req-1"));
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["max_output_tokens"], 4096);
    }

    #[test]
    fn an_absent_cap_is_not_serialized() {
        let request = chat_request("sys".into(), "model".into(), None, &params(None));

        let json = serde_json::to_value(&request).unwrap();
        assert!(json.get("max_output_tokens").is_none());
        let decoded: types::LlmChatRequest = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.max_output_tokens, None);
    }

    fn proxy() -> HostLlmProxy {
        HostLlmProxy::new(String::new(), String::new())
    }

    #[test]
    fn status_coded_failures_follow_the_v2_retryable_set() {
        let p = proxy();

        for code in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
            assert!(
                p.is_retryable_error(&format!("llm http status {code}: boom")),
                "status {code} must be retryable"
            );
        }

        for code in [400, 401, 402, 403, 404, 422, 501, 505, 511] {
            assert!(
                !p.is_retryable_error(&format!("llm http status {code}: boom")),
                "status {code} must fail fast"
            );
        }
    }

    #[test]
    fn a_body_mentioning_a_transport_word_does_not_make_a_client_error_retryable() {
        let p = proxy();

        assert!(!p.is_retryable_error("llm http status 400: invalid connection parameter"));
        assert!(
            !p.is_retryable_error("llm http status 401: your session timed out, please reconnect")
        );
        assert!(
            !p.is_retryable_error("llm http status 422: upstream socket hang up while parsing")
        );
    }

    #[test]
    fn an_unparseable_status_is_not_retryable() {
        let p = proxy();
        assert!(!p.is_retryable_error("llm http status abc: weird"));
    }

    #[test]
    fn cancellation_is_never_retryable() {
        let p = proxy();

        assert!(!p.is_retryable_error("llm cancelled: request aborted"));
        assert!(!p.is_retryable_error("llm cancelled: request aborted (connection reset by peer)"));
    }

    #[test]
    fn transport_level_failures_without_a_status_stay_retryable() {
        let p = proxy();
        for msg in [
            "error sending request for url (https://example.test): connection refused",
            "sse decode error: unexpected end of stream",
            "socket hang up",
            "overloaded",
        ] {
            assert!(p.is_retryable_error(msg), "{msg}");
        }
        assert!(!p.is_retryable_error("invalid api key"));
    }
}
