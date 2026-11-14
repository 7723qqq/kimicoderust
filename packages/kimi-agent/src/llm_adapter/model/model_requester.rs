use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::human::llm::finish_reason::FinishReason;
use crate::human::llm::response_format::ResponseFormat;
use crate::human::llm::usage::TokenUsage;
use crate::llm_adapter::contract::message::{ContentPart, Message, StreamedMessagePart, Tool};
use crate::llm_adapter::model::catalog::Model;
use crate::wire::record::WireAsyncError;

pub use crate::llm::model_requester::{
    ModelRequestParams, SamplingOptions, effective_max_completion_tokens,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequestInput {
    pub system_prompt: String,
    pub tools: Vec<Tool>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRequestTiming {
    pub first_token_latency_ms: u64,
    pub stream_duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_build_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_first_token_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_decode_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_consume_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_blocked_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_first_token_offset_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_last_token_offset_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_window_on_frame_clock: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ModelRequestEvent {
    #[serde(rename = "part")]
    Part { part: StreamedMessagePart },
    #[serde(rename = "usage", rename_all = "camelCase")]
    Usage {
        usage: TokenUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    #[serde(rename = "finish", rename_all = "camelCase")]
    Finish {
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_finish_reason: Option<FinishReason>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_finish_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trace_id: Option<String>,
    },
    #[serde(rename = "timing")]
    Timing(ModelRequestTiming),
}

pub type ModelRequestStream = BoxStream<'static, Result<ModelRequestEvent, WireAsyncError>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoUploadInput {
    pub data: Vec<u8>,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageUploadInput {
    pub data: Vec<u8>,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VideoUploadSource {
    Url(String),
    Upload(VideoUploadInput),
}

#[derive(Clone, Default)]
pub struct MediaUploadOptions {
    pub signal: Option<CancellationToken>,
}

pub trait ModelRequester: Send + Sync {
    fn model(&self) -> &Model;

    fn request(
        &self,
        input: ModelRequestInput,
        signal: Option<CancellationToken>,
        params: Option<ModelRequestParams>,
    ) -> ModelRequestStream;

    fn upload_video(
        &self,
        _input: VideoUploadSource,
        _options: MediaUploadOptions,
    ) -> Option<BoxFuture<'static, Result<ContentPart, WireAsyncError>>> {
        None
    }

    fn upload_image(
        &self,
        _input: ImageUploadInput,
        _options: MediaUploadOptions,
    ) -> Option<BoxFuture<'static, Result<ContentPart, WireAsyncError>>> {
        None
    }
}
