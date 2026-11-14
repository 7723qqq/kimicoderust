use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::human::llm::errors::{LlmErrorMessage, LlmRemoteErrorMessage};
use crate::human::llm::finish_reason::FinishInfo;
use crate::human::llm::message::{Message, StreamedMessagePart, ToolDescription};
use crate::human::llm::model::LlmModel;
use crate::human::llm::response_format::ResponseFormat;
use crate::human::llm::thinking::ThinkingRequestOptions;
use crate::human::llm::requester::bases::anthropic::extra_params::AnthropicExtraParams;
use crate::human::llm::requester::bases::google_genai::extra_params::GoogleGenAIExtraParams;
use crate::human::llm::requester::bases::openai::extra_params::OpenAIExtraParams;
use crate::human::llm::requester::bases::openai_responses::extra_params::OpenAIResponsesExtraParams;
use crate::human::llm::usage::TokenUsagePatch;
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct ToolCallIdPolicy {
    pub normalize: Arc<dyn Fn(String) -> String + Send + Sync>,
    pub max_length: Option<usize>,
}

pub type LlmErrorClassifier =
    Arc<dyn Fn(&WireAsyncError) -> Option<LlmRemoteErrorMessage> + Send + Sync>;

#[derive(Clone)]
pub enum LlmRequestEvent {
    Sent,
    StreamingHeaders {
        headers: HashMap<String, String>,
    },
    StreamingPart {
        part: StreamedMessagePart,
    },
    StreamingUsage {
        usage: TokenUsagePatch,
    },
    StreamingFinish {
        finish: FinishInfo,
    },
    StreamingMessageId {
        message_id: String,
    },
    FailedSyntax {
        error: LlmErrorMessage,
    },
    FailedRemote {
        error: LlmRemoteErrorMessage,
        raw_error: Option<Value>,
    },
    RequestRetrying,
    Done,
}

pub type LlmEvent = LlmRequestEvent;

#[derive(Clone, Default)]
pub struct ExtraParams {
    pub openai: Option<OpenAIExtraParams>,
    pub responses: Option<OpenAIResponsesExtraParams>,
    pub anthropic: Option<AnthropicExtraParams>,
    pub google_genai: Option<GoogleGenAIExtraParams>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMessageConversion {
    ExtractText,
    KeepParts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmCredential {
    pub api_key: Option<String>,
    pub headers: Option<HashMap<String, String>>,
}

#[async_trait]
pub trait LlmCredentialProvider: Send + Sync {
    async fn resolve(&self) -> Result<Option<LlmCredential>, WireAsyncError>;
    fn can_recover(&self, _error: &WireAsyncError) -> bool {
        false
    }
    fn invalidate(&self) {}
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeterminismRequestOptions {
    pub seed: Option<u64>,
}

#[derive(Clone)]
pub struct LlmRequestConfig {
    pub model: LlmModel,
    pub credential_provider: Option<Arc<dyn LlmCredentialProvider>>,
    pub system_prompt: Option<String>,
    pub tools: Option<Vec<ToolDescription>>,
    pub cache_key: Option<String>,
    pub thinking: Option<ThinkingRequestOptions>,
    pub response_format: Option<ResponseFormat>,
    pub max_completion_tokens: Option<u64>,
    pub max_context_tokens: Option<u64>,
    pub determinism: Option<DeterminismRequestOptions>,
    pub extra_params: Option<ExtraParams>,
    pub tool_message_conversion: Option<ToolMessageConversion>,
}

impl Default for LlmRequestConfig {
    fn default() -> Self {
        Self {
            model: LlmModel::default(),
            credential_provider: None,
            system_prompt: None,
            tools: None,
            cache_key: None,
            thinking: None,
            response_format: None,
            max_completion_tokens: None,
            max_context_tokens: None,
            determinism: None,
            extra_params: None,
            tool_message_conversion: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct LlmRequestContent {
    pub messages: Vec<Message>,
    pub used_context_tokens: Option<u64>,
}

#[derive(Clone)]
pub struct LlmRequestControl {
    pub signal: tokio_util::sync::CancellationToken,
    pub on_event: Arc<dyn Fn(LlmRequestEvent) + Send + Sync>,
}

impl LlmRequestControl {
    pub fn new(signal: tokio_util::sync::CancellationToken) -> Self {
        Self {
            signal,
            on_event: Arc::new(|_| {}),
        }
    }
}

#[async_trait]
pub trait LlmRequester: Send + Sync {
    async fn generate(
        &self,
        config: LlmRequestConfig,
        content: LlmRequestContent,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError>;
}

#[derive(Debug, Clone)]
pub struct LlmClientContext {
    pub model: LlmModel,
    pub headers: Option<HashMap<String, String>>,
}

pub struct LlmRequesterOptions<TClient> {
    pub client_factory: Option<Arc<dyn Fn(LlmClientContext) -> TClient + Send + Sync>>,
}

impl<TClient> Default for LlmRequesterOptions<TClient> {
    fn default() -> Self {
        Self {
            client_factory: None,
        }
    }
}

pub fn merge_request_headers(
    default_headers: Option<&HashMap<String, String>>,
    request_headers: Option<&HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    let mut merged = HashMap::new();
    if let Some(headers) = default_headers {
        merged.extend(headers.clone());
    }
    if let Some(headers) = request_headers {
        merged.extend(headers.clone());
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}
