use std::error::Error;

use base64::Engine as _;

use crate::human::llm::message::{ContentPart, VideoUrl};
use crate::llm_adapter::contract::errors::{
    APIContextOverflowError, APIProviderOverloadedError, APIProviderQuotaExhaustedError,
    APIProviderRateLimitError, APIRequestTooLargeError, APIStatusError, ProviderErrorCode,
    VideoUploadUnsupportedError, chat_error_of,
};
use crate::llm_adapter::protocol::protocol::Protocol;

pub fn is_media_upload_auth_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    if chat_error_of(error).is_some_and(|error| error.code == ProviderErrorCode::Auth) {
        return true;
    }
    if error
        .downcast_ref::<crate::_base::errors::serialize::RestoredError2>()
        .and_then(|error| error.status_code)
        .is_some_and(|status| status == 401.0 || status == 403.0)
    {
        return true;
    }
    status_code(error).is_some_and(|status| status == 401 || status == 403)
}

fn status_code(error: &(dyn Error + Send + Sync + 'static)) -> Option<i64> {
    if let Some(error) = error.downcast_ref::<kimi_core::primitives::errors::Error2>() {
        return error.status_code();
    }
    if let Some(error) = error.downcast_ref::<APIStatusError>() {
        return error.status_code;
    }
    if let Some(error) = error.downcast_ref::<APIContextOverflowError>() {
        return error.0.status_code;
    }
    if let Some(error) = error.downcast_ref::<APIRequestTooLargeError>() {
        return error.0.status_code;
    }
    if let Some(error) = error.downcast_ref::<APIProviderRateLimitError>() {
        return error.0.status_code;
    }
    if let Some(error) = error.downcast_ref::<APIProviderQuotaExhaustedError>() {
        return error.0.status_code;
    }
    error
        .downcast_ref::<APIProviderOverloadedError>()
        .and_then(|error| error.0.status_code)
}

pub fn is_video_upload_unsupported_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> bool {
    error.downcast_ref::<VideoUploadUnsupportedError>().is_some()
}

pub fn inline_video_supported_for_protocol(protocol: Protocol) -> bool {
    !matches!(protocol, Protocol::OpenAI | Protocol::OpenAIResponses)
}

pub fn inline_video_part(data: &[u8], mime_type: &str) -> ContentPart {
    ContentPart::VideoUrl {
        video_url: VideoUrl {
            url: format!(
                "data:{mime_type};base64,{}",
                base64::engine::general_purpose::STANDARD.encode(data)
            ),
            id: None,
            name: None,
        },
    }
}
