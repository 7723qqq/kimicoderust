use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::log::log::LogContext;
use crate::_base::utils::abort::AbortSignal;
pub use crate::app::llm_protocol::errors::AgentLLMRequestFinish;
use crate::human::llm::message::{StreamedMessagePart, ToolDescription as Tool};
use crate::human::llm::requester::requester::LlmCredentialProvider;
use crate::human::llm::thinking::ThinkingEffort;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::contract::request_trace::LLMRequestTrace;
use crate::llm_adapter::model::model_requester::ModelRequestTiming;
use crate::session::cache_observation::CacheObservation;
use crate::wire::record::WireAsyncError;

pub type AgentLLMRequestLogFields = LogContext;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentLLMRequestSource {
    #[serde(rename_all = "camelCase")]
    Turn {
        turn_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_fields: Option<AgentLLMRequestLogFields>,
    },
    #[serde(rename_all = "camelCase")]
    Operation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        log_fields: Option<AgentLLMRequestLogFields>,
    },
}

impl AgentLLMRequestSource {
    pub fn turn_id(&self) -> Option<u64> {
        match self {
            Self::Turn { turn_id, .. } => Some(*turn_id),
            Self::Operation { turn_id, .. } => *turn_id,
        }
    }

    pub fn log_fields(&self) -> Option<&AgentLLMRequestLogFields> {
        match self {
            Self::Turn { log_fields, .. } | Self::Operation { log_fields, .. } => {
                log_fields.as_ref()
            }
        }
    }
}

pub type AgentLLMRequestPartHandler = Arc<
    dyn Fn(StreamedMessagePart) -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync,
>;

#[derive(Default)]
pub struct AgentLLMRequestOverrides {
    pub messages: Option<Vec<Message>>,
    pub tools: Option<Vec<Tool>>,
    pub system_prompt: Option<String>,
    pub source: Option<AgentLLMRequestSource>,
    pub max_output_size: Option<u32>,
    pub used_context_tokens: Option<u64>,
    pub prompt_cache_probe: Option<PromptCacheProbe>,
    pub on_attempt_retry: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptCacheProbe {
    Compaction,
}

#[derive(Debug, Clone, Default)]
pub struct AgentLLMRequestAttemptFailure {
    pub usage: Option<TokenUsage>,
    pub message_id: Option<String>,
    pub timing: Option<ModelRequestTiming>,
}

pub type AgentLLMRequestAttemptFailureGetter =
    Arc<dyn Fn() -> Option<AgentLLMRequestAttemptFailure> + Send + Sync>;

pub struct AgentLLMRequestTask {
    pub trace: LLMRequestTrace,
    pub result: BoxFuture<'static, Result<AgentLLMRequestFinish, WireAsyncError>>,
    pub attempt_failure: AgentLLMRequestAttemptFailureGetter,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedTurnRequestConfig {
    pub thinking_effort: ThinkingEffort,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLLMTurnRequestSnapshot {
    pub model_alias: String,
    pub context_revision: u64,
    pub tools_hash: String,
    pub system_prompt_hash: String,
    pub message_count: usize,
    pub history_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    pub started_at: i64,
    pub finished_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<CacheObservation>,
}

pub trait IAgentLLMRequesterService: Send + Sync {
    fn prepare_turn_config(&self, turn_id: u64) -> Option<PreparedTurnRequestConfig>;

    fn current_credential_provider(&self) -> Option<Arc<dyn LlmCredentialProvider>>;

    fn credential_provider_for_turn(
        &self,
        turn_id: u64,
    ) -> Option<Arc<dyn LlmCredentialProvider>>;

    fn last_turn_request(&self) -> Option<AgentLLMTurnRequestSnapshot>;

    fn request(
        &self,
        overrides: Option<AgentLLMRequestOverrides>,
        on_part: Option<AgentLLMRequestPartHandler>,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'static, Result<AgentLLMRequestFinish, WireAsyncError>>;

    fn start(
        &self,
        overrides: Option<AgentLLMRequestOverrides>,
        on_part: Option<AgentLLMRequestPartHandler>,
        signal: Option<AbortSignal>,
    ) -> AgentLLMRequestTask;
}


pub static I_AGENT_LLM_REQUESTER_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentLLMRequesterService>>> =
    LazyLock::new(|| create_decorator("agentLLMRequesterService"));
