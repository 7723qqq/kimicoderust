//! A subagent's profile system prompt as an LLM decorator.

use std::sync::Arc;

use crate::rpc::types::BoxFuture;
use crate::turn_loop::types::{LLM, LLMChatParams, LLMChatResponse};

/// Presents a profile's system prompt in place of the transport's own.
///
/// v2 creates a subagent through `agentLifecycle.create({ binding: { profile } })`,
/// so its first request carries the *profile's* `systemPrompt`, not the
/// session's. The engine shares one transport between the main agent and every
/// subagent; this decorator is the seam that gives a spawned subagent its own
/// prompt without a second transport. Every other method delegates, so model
/// identity, capability gates, media routing and the streaming sinks stay the
/// session's.
pub struct ProfilePromptLlm {
    inner: Arc<dyn LLM>,
    system_prompt: String,
}

impl ProfilePromptLlm {
    /// Wrap `inner` so its requests carry `system_prompt`. A blank prompt
    /// returns `inner` untouched: an empty system message is worse than the
    /// session's, and a profile the host registered without a prompt has
    /// nothing to say.
    pub fn wrap(inner: Arc<dyn LLM>, system_prompt: &str) -> Arc<dyn LLM> {
        if system_prompt.trim().is_empty() {
            return inner;
        }
        Arc::new(Self {
            inner,
            system_prompt: system_prompt.to_string(),
        })
    }
}

impl LLM for ProfilePromptLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn is_retryable_error(&self, error: &str) -> bool {
        self.inner.is_retryable_error(error)
    }

    fn transport(&self) -> &'static str {
        self.inner.transport()
    }

    fn model_alias(&self) -> Option<&str> {
        self.inner.model_alias()
    }

    fn provider_type(&self) -> Option<&str> {
        self.inner.provider_type()
    }

    fn protocol(&self) -> Option<&str> {
        self.inner.protocol()
    }

    fn thinking_effort(&self) -> Option<&str> {
        self.inner.thinking_effort()
    }

    fn media_target(&self) -> Option<crate::llm::media_resolver::MediaTarget> {
        self.inner.media_target()
    }

    fn media_upload_credential(&self) -> Option<crate::llm::files_upload::CredentialFuture<'_>> {
        self.inner.media_upload_credential()
    }

    fn set_tool_call_ids(&self, ledger: Arc<crate::turn_loop::tool_call_id::ToolCallIdNormalizer>) {
        self.inner.set_tool_call_ids(ledger);
    }

    fn set_stream_agent_id(&self, agent_id: &str) {
        self.inner.set_stream_agent_id(agent_id);
    }

    fn set_turn_watch(&self, watch: Option<Arc<crate::turn_loop::watch::TurnWatchState>>) {
        self.inner.set_turn_watch(watch);
    }

    fn chat(
        &self,
        params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        self.inner.chat(params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubLlm;

    impl LLM for StubLlm {
        fn system_prompt(&self) -> &str {
            "session prompt"
        }
        fn model_name(&self) -> &str {
            "stub-model"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            _: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            Box::pin(async { Err("not implemented".into()) })
        }
    }

    #[test]
    fn replaces_only_the_system_prompt() {
        let inner: Arc<dyn LLM> = Arc::new(StubLlm);
        let wrapped = ProfilePromptLlm::wrap(inner, "You are the coder.");
        assert_eq!(wrapped.system_prompt(), "You are the coder.");
        assert_eq!(wrapped.model_name(), "stub-model");
    }

    #[test]
    fn a_blank_prompt_keeps_the_inner_transport() {
        let inner: Arc<dyn LLM> = Arc::new(StubLlm);
        let wrapped = ProfilePromptLlm::wrap(inner.clone(), "   ");
        assert!(Arc::ptr_eq(&wrapped, &inner));
        assert_eq!(wrapped.system_prompt(), "session prompt");
    }
}
