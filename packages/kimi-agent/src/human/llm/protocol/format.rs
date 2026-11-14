use std::sync::Arc;

use crate::human::llm::errors::LlmRemoteErrorMessage;
use crate::human::llm::finish_reason::FinishInfo;
use crate::human::llm::message::{Message, StreamedMessagePart, ToolDescription};
use crate::human::llm::model::LlmModel;
use crate::human::llm::usage::TokenUsagePatch;

#[derive(Debug, Clone)]
pub struct FormatRequestInput {
    pub model: LlmModel,
    pub system_prompt: Option<String>,
    pub cache_key: Option<String>,
    pub thinking: Option<crate::human::llm::thinking::ThinkingRequestOptions>,
    pub response_format: Option<crate::human::llm::response_format::ResponseFormat>,
    pub determinism: Option<crate::human::llm::requester::requester::DeterminismRequestOptions>,
    pub extra_params: Option<crate::human::llm::requester::requester::ExtraParams>,
    pub tool_message_conversion: Option<crate::human::llm::requester::requester::ToolMessageConversion>,
    pub max_completion_tokens: Option<u64>,
    pub max_context_tokens: Option<u64>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDescription>,
    pub used_context_tokens: Option<u64>,
}

pub fn resolve_context_window_tokens(input: &FormatRequestInput) -> Option<u64> {
    let explicit = input.max_context_tokens.filter(|value| *value > 0);
    let window = explicit.or(input.model.max_context_size);
    window.filter(|value| *value > 0)
}

pub fn remaining_context_tokens(input: &FormatRequestInput) -> Option<i64> {
    let window = resolve_context_window_tokens(input)?;
    let used = input.used_context_tokens?;
    Some(window as i64 - used as i64)
}

pub fn resolve_max_completion_cap(input: &FormatRequestInput) -> Option<u64> {
    let max_completion = input.max_completion_tokens.filter(|value| *value > 0)?;
    let remaining = remaining_context_tokens(input);
    match remaining {
        Some(remaining) if remaining > 0 => Some(max_completion.min(remaining as u64)),
        Some(_) => None,
        None => Some(max_completion),
    }
}

#[derive(Clone)]
pub struct StreamParseSink {
    pub on_delta: Arc<dyn Fn(StreamedMessagePart) + Send + Sync>,
    pub on_finish: Arc<dyn Fn(FinishInfo) + Send + Sync>,
    pub on_message_id: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pub on_usage: Option<Arc<dyn Fn(TokenUsagePatch) + Send + Sync>>,
    pub on_error: Option<Arc<dyn Fn(LlmRemoteErrorMessage) + Send + Sync>>,
}

pub struct StreamParserOptions<TChunk> {
    pub resolve_usage: Option<Arc<dyn Fn(&TChunk, Option<TokenUsagePatch>) -> Option<TokenUsagePatch> + Send + Sync>>,
}

impl<TChunk> Default for StreamParserOptions<TChunk> {
    fn default() -> Self {
        Self {
            resolve_usage: None,
        }
    }
}

pub type StreamParser<TChunk> = Arc<
    dyn Fn(TChunk, &StreamParseSink) -> Result<(), crate::wire::record::WireAsyncError>
        + Send
        + Sync,
>;

pub trait ProtocolFormat<TChunk>: Send + Sync {
    fn create_stream_parser(&self, options: StreamParserOptions<TChunk>) -> StreamParser<TChunk>;
}
