use std::sync::Arc;

use crate::_base::utils::abort::AbortSignal;
pub use crate::human::llm::media::upload::VideoUploadInput;
use crate::human::llm::message::ContentPart;
use crate::rpc::types::BoxFuture;
use crate::wire::record::WireAsyncError;

pub const READ_MEDIA_FILE_TOOL_NAME: &str = "ReadMediaFile";
pub const MAX_MEDIA_MEGABYTES: u64 = 100;
pub const MAX_MEDIA_BYTES: u64 = MAX_MEDIA_MEGABYTES * 1024 * 1024;

#[derive(Clone, Default)]
pub struct VideoUploadOptions {
    pub signal: Option<AbortSignal>,
}

pub type VideoUploader = Arc<
    dyn Fn(
            VideoUploadInput,
            VideoUploadOptions,
        ) -> BoxFuture<'static, Result<ContentPart, WireAsyncError>>
        + Send
        + Sync,
>;
