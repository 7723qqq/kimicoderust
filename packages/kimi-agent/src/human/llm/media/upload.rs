use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::human::llm::message::ContentPart;
use crate::human::llm::model::LlmModel;
use crate::wire::record::WireAsyncError;

pub type VideoURLPart = ContentPart;
pub type ImageURLPart = ContentPart;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoUploadInput {
    pub data: Vec<u8>,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[derive(Clone)]
pub struct MediaVideoUploadOptions {
    pub model: LlmModel,
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

pub type MediaVideoUploader = Arc<
    dyn Fn(
            VideoUploadInput,
            MediaVideoUploadOptions,
        ) -> BoxFuture<'static, Result<VideoURLPart, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageUploadInput {
    pub data: Vec<u8>,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

pub type MediaImageUploader = Arc<
    dyn Fn(
            ImageUploadInput,
            MediaVideoUploadOptions,
        ) -> BoxFuture<'static, Result<ImageURLPart, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Clone, Default)]
pub struct ProviderMediaContribution {
    pub inline_video: Option<bool>,
    pub upload_video: Option<MediaVideoUploader>,
    pub upload_image: Option<MediaImageUploader>,
}
