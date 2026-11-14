use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::ops::Deref;

use kimi_core::primitives::errors::Error2;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::human::llm::errors::{
    LlmErrorKind, LlmErrorMessage, LlmRemoteErrorMessage, append_thinking_effort_config_hint,
    is_context_overflow_status_error, is_image_format_status_error,
    is_provider_overload_status_error, is_request_structure_status_error,
    is_request_too_large_status_error, is_tool_exchange_adjacency_status_error,
    is_transient_http_status,
};
use crate::human::llm::finish_reason::FinishReason;
use crate::wire::record::WireAsyncError;

pub use crate::human::llm::errors::{
    error_status_code, parse_retry_after_ms, sanitize_status_error_message,
};

pub const CONFIG_INVALID_ERROR_CODE: &str = "config.invalid";
pub const PROVIDER_API_ERROR_CODE: &str = "provider.api_error";
pub const PROVIDER_FILTERED_ERROR_CODE: &str = "provider.filtered";
pub const PROVIDER_RATE_LIMIT_ERROR_CODE: &str = "provider.rate_limit";
pub const PROVIDER_AUTH_ERROR_CODE: &str = "provider.auth_error";
pub const PROVIDER_CONNECTION_ERROR_CODE: &str = "provider.connection_error";
pub const PROVIDER_OVERLOADED_ERROR_CODE: &str = "provider.overloaded";
pub const CONTEXT_OVERFLOW_ERROR_CODE: &str = "context.overflow";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderErrorCode {
    ApiError,
    Filtered,
    RateLimit,
    AuthError,
    ConnectionError,
    Overloaded,
    ContextOverflow,
}

impl ProviderErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiError => PROVIDER_API_ERROR_CODE,
            Self::Filtered => PROVIDER_FILTERED_ERROR_CODE,
            Self::RateLimit => PROVIDER_RATE_LIMIT_ERROR_CODE,
            Self::AuthError => PROVIDER_AUTH_ERROR_CODE,
            Self::ConnectionError => PROVIDER_CONNECTION_ERROR_CODE,
            Self::Overloaded => PROVIDER_OVERLOADED_ERROR_CODE,
            Self::ContextOverflow => CONTEXT_OVERFLOW_ERROR_CODE,
        }
    }
}

impl fmt::Display for ProviderErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn code_for_status_error(status_code: Option<i64>) -> ProviderErrorCode {
    match status_code {
        Some(429) => ProviderErrorCode::RateLimit,
        Some(401 | 403) => ProviderErrorCode::AuthError,
        Some(529) => ProviderErrorCode::Overloaded,
        _ => ProviderErrorCode::ApiError,
    }
}

#[derive(Debug, Clone)]
pub struct ChatProviderError {
    pub message: String,
    pub code: ProviderErrorCode,
    pub retryable: bool,
    pub details: Option<Value>,
    pub name: String,
}

impl ChatProviderError {
    pub fn new(message: impl Into<String>, code: ProviderErrorCode, retryable: bool) -> Self {
        Self::with_options(message, code, None, retryable, "ChatProviderError")
    }

    pub fn with_options(
        message: impl Into<String>,
        code: ProviderErrorCode,
        details: Option<Value>,
        retryable: bool,
        name: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            code,
            retryable,
            details,
            name: name.into(),
        }
    }
}

impl fmt::Display for ChatProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ChatProviderError {}

macro_rules! impl_chat_error {
    ($type:ident) => {
        impl Deref for $type {
            type Target = ChatProviderError;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0.message)
            }
        }

        impl Error for $type {}
    };
}

macro_rules! simple_chat_error {
    ($type:ident, $name:literal, $code:expr, $retryable:expr) => {
        #[derive(Debug, Clone)]
        pub struct $type(pub ChatProviderError);

        impl $type {
            pub fn new(message: impl Into<String>) -> Self {
                Self(ChatProviderError::with_options(
                    message,
                    $code,
                    None,
                    $retryable,
                    $name,
                ))
            }
        }

        impl_chat_error!($type);
    };
}

simple_chat_error!(
    APIConnectionError,
    "APIConnectionError",
    ProviderErrorCode::ConnectionError,
    true
);
simple_chat_error!(
    VideoUploadUnsupportedError,
    "VideoUploadUnsupportedError",
    ProviderErrorCode::ApiError,
    false
);
simple_chat_error!(
    ImageUploadUnsupportedError,
    "ImageUploadUnsupportedError",
    ProviderErrorCode::ApiError,
    false
);
simple_chat_error!(
    APITimeoutError,
    "APITimeoutError",
    ProviderErrorCode::ConnectionError,
    true
);

#[derive(Debug, Clone)]
pub struct APIStatusError {
    pub base: ChatProviderError,
    pub status_code: Option<i64>,
    pub request_id: Option<String>,
    pub retry_after_ms: Option<i64>,
    pub trace_id: Option<String>,
}

impl APIStatusError {
    pub fn new(
        status_code: Option<i64>,
        message: impl Into<String>,
        retryable: bool,
        request_id: Option<String>,
        retry_after_ms: Option<i64>,
        trace_id: Option<String>,
        code: ProviderErrorCode,
    ) -> Self {
        let message = sanitize_status_error_message(&message.into());
        let details = json!({
            "statusCode": status_code,
            "requestId": request_id,
            "traceId": trace_id,
        });
        Self {
            base: ChatProviderError::with_options(
                message,
                code,
                Some(details),
                retryable,
                "APIStatusError",
            ),
            status_code,
            request_id,
            retry_after_ms,
            trace_id,
        }
    }

    pub fn for_status(
        status_code: Option<i64>,
        message: impl Into<String>,
        retryable: bool,
        request_id: Option<String>,
        retry_after_ms: Option<i64>,
        trace_id: Option<String>,
    ) -> Self {
        Self::new(
            status_code,
            message,
            retryable,
            request_id,
            retry_after_ms,
            trace_id,
            code_for_status_error(status_code),
        )
    }
}

impl Deref for APIStatusError {
    type Target = ChatProviderError;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl fmt::Display for APIStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.base.message)
    }
}

impl Error for APIStatusError {}

macro_rules! status_error {
    ($type:ident, $name:literal, $code:expr, $retryable:expr) => {
        #[derive(Debug, Clone)]
        pub struct $type(pub APIStatusError);

        impl $type {
            pub fn new(
                status_code: Option<i64>,
                message: impl Into<String>,
                request_id: Option<String>,
                retry_after_ms: Option<i64>,
                trace_id: Option<String>,
            ) -> Self {
                let mut base = APIStatusError::new(
                    status_code,
                    message,
                    $retryable,
                    request_id,
                    retry_after_ms,
                    trace_id,
                    $code,
                );
                base.base.name = $name.to_string();
                Self(base)
            }
        }

        impl Deref for $type {
            type Target = APIStatusError;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0.base.message)
            }
        }

        impl Error for $type {}
    };
}

status_error!(
    APIContextOverflowError,
    "APIContextOverflowError",
    ProviderErrorCode::ContextOverflow,
    false
);
status_error!(
    APIRequestTooLargeError,
    "APIRequestTooLargeError",
    ProviderErrorCode::ApiError,
    false
);
status_error!(
    APIProviderOverloadedError,
    "APIProviderOverloadedError",
    ProviderErrorCode::Overloaded,
    true
);

#[derive(Debug, Clone)]
pub struct APIProviderRateLimitError(pub APIStatusError);

impl APIProviderRateLimitError {
    pub fn new(
        message: impl Into<String>,
        request_id: Option<String>,
        retry_after_ms: Option<i64>,
        trace_id: Option<String>,
        status_code: Option<i64>,
    ) -> Self {
        Self::from_optional_status(
            message,
            request_id,
            retry_after_ms,
            trace_id,
            Some(status_code.unwrap_or(429)),
        )
    }

    pub fn from_optional_status(
        message: impl Into<String>,
        request_id: Option<String>,
        retry_after_ms: Option<i64>,
        trace_id: Option<String>,
        status_code: Option<i64>,
    ) -> Self {
        let mut base = APIStatusError::new(
            status_code,
            message,
            true,
            request_id,
            retry_after_ms,
            trace_id,
            ProviderErrorCode::RateLimit,
        );
        base.base.name = "APIProviderRateLimitError".to_string();
        Self(base)
    }
}

impl Deref for APIProviderRateLimitError {
    type Target = APIStatusError;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for APIProviderRateLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.base.message)
    }
}

impl Error for APIProviderRateLimitError {}

#[derive(Debug, Clone)]
pub struct APIProviderQuotaExhaustedError(pub APIStatusError);

impl APIProviderQuotaExhaustedError {
    pub fn new(
        message: impl Into<String>,
        request_id: Option<String>,
        retry_after_ms: Option<i64>,
        trace_id: Option<String>,
        status_code: Option<i64>,
    ) -> Self {
        Self::from_optional_status(
            message,
            request_id,
            retry_after_ms,
            trace_id,
            Some(status_code.unwrap_or(429)),
        )
    }

    pub fn from_optional_status(
        message: impl Into<String>,
        request_id: Option<String>,
        retry_after_ms: Option<i64>,
        trace_id: Option<String>,
        status_code: Option<i64>,
    ) -> Self {
        let mut base = APIStatusError::new(
            status_code,
            message,
            false,
            request_id,
            retry_after_ms,
            trace_id,
            ProviderErrorCode::ApiError,
        );
        base.base.name = "APIProviderQuotaExhaustedError".to_string();
        Self(base)
    }
}

impl Deref for APIProviderQuotaExhaustedError {
    type Target = APIStatusError;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for APIProviderQuotaExhaustedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.base.message)
    }
}

impl Error for APIProviderQuotaExhaustedError {}

#[derive(Debug, Clone)]
pub struct APIEmptyResponseError(pub ChatProviderError);

impl APIEmptyResponseError {
    pub fn new(
        message: impl Into<String>,
        finish_reason: Option<FinishReason>,
        raw_finish_reason: Option<String>,
    ) -> Self {
        let filtered = finish_reason == Some(FinishReason::Filtered);
        let details = json!({
            "finishReason": finish_reason,
            "rawFinishReason": raw_finish_reason,
        });
        Self(ChatProviderError::with_options(
            message,
            if filtered {
                ProviderErrorCode::Filtered
            } else {
                ProviderErrorCode::ApiError
            },
            Some(details),
            !filtered,
            "APIEmptyResponseError",
        ))
    }

    pub fn finish_reason(&self) -> Option<FinishReason> {
        self.0
            .details
            .as_ref()
            .and_then(|details| details.get("finishReason"))
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .flatten()
    }

    pub fn raw_finish_reason(&self) -> Option<&str> {
        self.0
            .details
            .as_ref()
            .and_then(|details| details.get("rawFinishReason"))
            .and_then(Value::as_str)
    }
}

impl_chat_error!(APIEmptyResponseError);

pub fn create_abort_error() -> WireAsyncError {
    Box::new(
        Error2::new("abort", "The operation was aborted.").with_name("AbortError"),
    )
}

pub fn is_abort_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    if error
        .downcast_ref::<Error2>()
        .is_some_and(|error| error.name() == "AbortError" || error.name() == "APIUserAbortError")
    {
        return true;
    }
    let debug = format!("{error:?}");
    debug.contains("AbortError") || debug.contains("APIUserAbortError")
}

pub fn throw_if_abort_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> Result<(), WireAsyncError> {
    if is_abort_error(error) {
        return Err(create_abort_error());
    }
    Ok(())
}

fn as_u16(status_code: Option<i64>) -> Option<u16> {
    status_code.and_then(|status| u16::try_from(status).ok())
}

fn status_error_of<'a>(
    error: &'a (dyn Error + Send + Sync + 'static),
) -> Option<&'a APIStatusError> {
    if let Some(error) = error.downcast_ref::<APIStatusError>() {
        return Some(error);
    }
    if let Some(error) = error.downcast_ref::<APIContextOverflowError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIRequestTooLargeError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIProviderRateLimitError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIProviderQuotaExhaustedError>() {
        return Some(&error.0);
    }
    error
        .downcast_ref::<APIProviderOverloadedError>()
        .map(|error| &error.0)
}

pub fn chat_error_of<'a>(
    error: &'a (dyn Error + Send + Sync + 'static),
) -> Option<&'a ChatProviderError> {
    if let Some(error) = error.downcast_ref::<ChatProviderError>() {
        return Some(error);
    }
    if let Some(error) = error.downcast_ref::<APIConnectionError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<VideoUploadUnsupportedError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<ImageUploadUnsupportedError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APITimeoutError>() {
        return Some(&error.0);
    }
    if let Some(error) = error.downcast_ref::<APIEmptyResponseError>() {
        return Some(&error.0);
    }
    status_error_of(error).map(|error| &error.base)
}

pub fn is_image_format_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    if error.downcast_ref::<APIContextOverflowError>().is_some()
        || error.downcast_ref::<APIRequestTooLargeError>().is_some()
    {
        return false;
    }
    if let Some(error) = status_error_of(error) {
        return as_u16(error.status_code)
            .is_some_and(|status| is_image_format_status_error(status, &error.base.message));
    }
    chat_error_of(error).is_some_and(|error| {
        let message = error.message.to_lowercase();
        [
            r"unsupported media type for base64 image",
            r"invalid data url for image",
        ]
        .iter()
        .any(|pattern| Regex::new(pattern).unwrap().is_match(&message))
    })
}

pub fn is_retryable_generate_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    !is_abort_error(error) && chat_error_of(error).is_some_and(|error| error.retryable)
}

pub fn classify_base_api_error(message: &str) -> WireAsyncError {
    if RegexBuilder::new(r"timed?\s*out|timeout|deadline")
        .case_insensitive(true)
        .build()
        .unwrap()
        .is_match(message)
    {
        return Box::new(APITimeoutError::new(message));
    }
    if RegexBuilder::new(r"network|connection|connect|disconnect|terminated")
        .case_insensitive(true)
        .build()
        .unwrap()
        .is_match(message)
    {
        return Box::new(APIConnectionError::new(message));
    }
    Box::new(ChatProviderError::new(
        format!("Error: {message}"),
        ProviderErrorCode::ApiError,
        true,
    ))
}

pub fn is_context_overflow_error_code(code: Option<&str>) -> bool {
    code == Some("context_length_exceeded")
}

pub fn normalize_api_status_error(
    status_code: i64,
    message: &str,
    request_id: Option<String>,
    retry_after_ms: Option<i64>,
    trace_id: Option<String>,
) -> WireAsyncError {
    if status_code == 429 {
        return Box::new(APIProviderRateLimitError::new(
            message,
            request_id,
            retry_after_ms,
            trace_id,
            Some(status_code),
        ));
    }
    let status = as_u16(Some(status_code));
    if status.is_some_and(|status| is_context_overflow_status_error(status, message)) {
        return Box::new(APIContextOverflowError::new(
            Some(status_code),
            message,
            request_id,
            retry_after_ms,
            trace_id,
        ));
    }
    if status.is_some_and(|status| is_request_too_large_status_error(status, message)) {
        return Box::new(APIRequestTooLargeError::new(
            Some(status_code),
            message,
            request_id,
            retry_after_ms,
            trace_id,
        ));
    }
    if status.is_some_and(|status| is_provider_overload_status_error(status, message)) {
        return Box::new(APIProviderOverloadedError::new(
            Some(status_code),
            message,
            request_id,
            retry_after_ms,
            trace_id,
        ));
    }
    Box::new(APIStatusError::for_status(
        Some(status_code),
        append_thinking_effort_config_hint(status.unwrap_or_default(), message),
        is_transient_http_status(status),
        request_id,
        retry_after_ms,
        trace_id,
    ))
}

pub fn parse_trace_id(headers: Option<&reqwest::header::HeaderMap>) -> Option<String> {
    headers?
        .get("x-trace-id")?
        .to_str()
        .ok()
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub fn trace_id_from_headers_record(headers: Option<&HashMap<String, String>>) -> Option<String> {
    headers?.iter().find_map(|(key, value)| {
        (key.eq_ignore_ascii_case("x-trace-id") && !value.is_empty()).then(|| value.clone())
    })
}

pub fn is_tool_exchange_adjacency_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> bool {
    if error.downcast_ref::<APIContextOverflowError>().is_some() {
        return false;
    }
    status_error_of(error).is_some_and(|error| {
        as_u16(error.status_code).is_some_and(|status| {
            is_tool_exchange_adjacency_status_error(status, &error.base.message)
        })
    })
}

pub fn is_recoverable_request_structure_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> bool {
    if error.downcast_ref::<APIContextOverflowError>().is_some() {
        return false;
    }
    status_error_of(error).is_some_and(|error| {
        as_u16(error.status_code).is_some_and(|status| {
            is_request_structure_status_error(status, &error.base.message)
        })
    })
}

pub fn is_unknown_cache_key_field_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> bool {
    if error.downcast_ref::<APIContextOverflowError>().is_some() {
        return false;
    }
    let Some(error) = status_error_of(error) else {
        return false;
    };
    if error.status_code != Some(400) && error.status_code != Some(422) {
        return false;
    }
    let message = error.base.message.to_lowercase();
    message.contains("prompt_cache_key")
        && [
            r"未知请求字段",
            r"unknown (?:request )?(?:field|parameter|argument)",
            r"unrecognized (?:request )?(?:field|parameter|keyword argument)",
        ]
        .iter()
        .any(|pattern| Regex::new(pattern).unwrap().is_match(&message))
}

fn get_status_code(error: &(dyn Error + Send + Sync + 'static)) -> Option<i64> {
    if let Some(status) = status_error_of(error).and_then(|error| error.status_code) {
        return Some(status);
    }
    if let Some(error) = error.downcast_ref::<Error2>() {
        if let Some(status) = error.status_code() {
            return Some(status);
        }
        if let Some(details) = error.details() {
            if let Some(status) = details
                .get("statusCode")
                .or_else(|| details.get("status"))
                .and_then(Value::as_i64)
            {
                return Some(status);
            }
            if let Some(status) = details
                .get("response")
                .and_then(|response| response.get("statusCode").or_else(|| response.get("status")))
                .and_then(Value::as_i64)
            {
                return Some(status);
            }
        }
    }
    error
        .downcast_ref::<reqwest::Error>()
        .and_then(|error| error.status())
        .map(|status| i64::from(status.as_u16()))
}

pub fn is_provider_rate_limit_error(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    if error
        .downcast_ref::<APIProviderQuotaExhaustedError>()
        .is_some()
    {
        return false;
    }
    if error.downcast_ref::<APIProviderRateLimitError>().is_some() {
        return true;
    }
    if let Some(status_code) = get_status_code(error) {
        return status_code == 429;
    }
    let message = error.to_string().to_lowercase();
    [
        r"(?:apistatuserror.*429|429.*apistatuserror)",
        r"429.*too many requests",
        r"too many requests",
        r"provider\.rate_limit",
        r"reached .*max rpm",
        r"rate[ _-]?limit(?:ed)?",
        r"rate-limited",
    ]
    .iter()
    .any(|pattern| Regex::new(pattern).unwrap().is_match(&message))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorKind {
    ContextOverflow,
    Overloaded,
    RateLimit,
    QuotaExhausted,
    Auth,
    #[serde(rename = "5xx_server")]
    Server5xx,
    #[serde(rename = "4xx_client")]
    Client4xx,
    Network,
    Timeout,
    EmptyResponse,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiErrorClassification {
    pub kind: ApiErrorKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i64>,
}

pub fn classify_api_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> ApiErrorClassification {
    let status_code = get_status_code(error);
    let kind = if error.downcast_ref::<APIContextOverflowError>().is_some() {
        ApiErrorKind::ContextOverflow
    } else if error.downcast_ref::<APIProviderOverloadedError>().is_some() {
        ApiErrorKind::Overloaded
    } else if error
        .downcast_ref::<APIProviderQuotaExhaustedError>()
        .is_some()
    {
        ApiErrorKind::QuotaExhausted
    } else if let Some(status) = status_error_of(error) {
        if as_u16(status.status_code).is_some_and(|status_code| {
            is_context_overflow_status_error(status_code, &status.base.message)
        }) {
            ApiErrorKind::ContextOverflow
        } else {
            match status.status_code {
                Some(429) => ApiErrorKind::RateLimit,
                Some(529) => ApiErrorKind::Overloaded,
                Some(401 | 403) => ApiErrorKind::Auth,
                Some(code) if code >= 500 => ApiErrorKind::Server5xx,
                Some(code) if code >= 400 => ApiErrorKind::Client4xx,
                _ if error.downcast_ref::<APIConnectionError>().is_some() => ApiErrorKind::Network,
                _ if error.downcast_ref::<APITimeoutError>().is_some() => ApiErrorKind::Timeout,
                _ if error.downcast_ref::<APIEmptyResponseError>().is_some() => {
                    ApiErrorKind::EmptyResponse
                }
                _ => ApiErrorKind::Other,
            }
        }
    } else if error.downcast_ref::<APIConnectionError>().is_some() {
        ApiErrorKind::Network
    } else if error.downcast_ref::<APITimeoutError>().is_some() {
        ApiErrorKind::Timeout
    } else if error.downcast_ref::<APIEmptyResponseError>().is_some() {
        ApiErrorKind::EmptyResponse
    } else {
        ApiErrorKind::Other
    };
    ApiErrorClassification { kind, status_code }
}

pub fn is_llm_error_message(error: &Value) -> bool {
    error.get("kind").and_then(Value::as_str).is_some()
        && error.get("message").and_then(Value::as_str).is_some()
}

pub fn is_unauthorized_llm_error(error: &Value) -> bool {
    if !is_llm_error_message(error) {
        return false;
    }
    let status_kind = error.get("kind").and_then(Value::as_str).is_some_and(|kind| {
        matches!(
            kind,
            "status"
                | "rate_limit"
                | "quota_exhausted"
                | "overloaded"
                | "context_overflow"
                | "request_too_large"
                | "request_structure"
                | "image_format"
        )
    });
    status_kind && error.get("statusCode").and_then(Value::as_f64) == Some(401.0)
}

fn status_code_from_llm(error: &LlmErrorMessage) -> Option<i64> {
    error.status_code.map(i64::from)
}

pub fn error_from_llm_message(error: &LlmErrorMessage) -> WireAsyncError {
    let status_code = status_code_from_llm(error);
    let trace_id = trace_id_from_headers_record(error.headers.as_ref());
    match error.kind {
        LlmErrorKind::Abort => create_abort_error(),
        LlmErrorKind::Connection => Box::new(APIConnectionError::new(error.message.clone())),
        LlmErrorKind::Timeout => Box::new(APITimeoutError::new(error.message.clone())),
        LlmErrorKind::RateLimit => Box::new(APIProviderRateLimitError::from_optional_status(
            error.message.clone(),
            error.request_id.clone(),
            error.retry_after_ms,
            trace_id,
            status_code,
        )),
        LlmErrorKind::QuotaExhausted => {
            Box::new(APIProviderQuotaExhaustedError::from_optional_status(
                error.message.clone(),
                error.request_id.clone(),
                error.retry_after_ms,
                trace_id,
                status_code,
            ))
        }
        LlmErrorKind::Overloaded => Box::new(APIProviderOverloadedError::new(
            status_code,
            error.message.clone(),
            error.request_id.clone(),
            error.retry_after_ms,
            trace_id,
        )),
        LlmErrorKind::ContextOverflow => Box::new(APIContextOverflowError::new(
            status_code,
            error.message.clone(),
            error.request_id.clone(),
            error.retry_after_ms,
            trace_id,
        )),
        LlmErrorKind::RequestTooLarge => Box::new(APIRequestTooLargeError::new(
            status_code,
            error.message.clone(),
            error.request_id.clone(),
            error.retry_after_ms,
            trace_id,
        )),
        LlmErrorKind::RequestStructure | LlmErrorKind::ImageFormat | LlmErrorKind::Status => {
            Box::new(APIStatusError::for_status(
                status_code,
                error.message.clone(),
                error.retryable.unwrap_or(false),
                error.request_id.clone(),
                error.retry_after_ms,
                trace_id,
            ))
        }
        LlmErrorKind::EmptyResponse => Box::new(APIEmptyResponseError::new(
            error.message.clone(),
            error.finish_reason,
            error.raw_finish_reason.clone(),
        )),
        LlmErrorKind::Syntax | LlmErrorKind::Unknown => Box::new(ChatProviderError::new(
            error.message.clone(),
            ProviderErrorCode::ApiError,
            false,
        )),
        LlmErrorKind::Provider => Box::new(ChatProviderError::new(
            error.message.clone(),
            ProviderErrorCode::ApiError,
            error.retryable.unwrap_or(false),
        )),
    }
}

fn trace_headers_of(error: &APIStatusError) -> Option<HashMap<String, String>> {
    error.trace_id.as_ref().map(|trace_id| {
        HashMap::from([("x-trace-id".to_string(), trace_id.clone())])
    })
}

fn status_message(
    kind: LlmErrorKind,
    error: &APIStatusError,
) -> LlmRemoteErrorMessage {
    LlmErrorMessage {
        kind,
        message: error.base.message.clone(),
        code: None,
        status_code: as_u16(error.status_code),
        request_id: error.request_id.clone(),
        retry_after_ms: error.retry_after_ms,
        headers: trace_headers_of(error),
        retryable: Some(error.base.retryable),
        finish_reason: None,
        raw_finish_reason: None,
    }
}

pub fn llm_message_from_error(
    error: &(dyn Error + Send + Sync + 'static),
) -> Option<LlmRemoteErrorMessage> {
    if let Some(error) = error.downcast_ref::<APIProviderRateLimitError>() {
        return Some(status_message(LlmErrorKind::RateLimit, &error.0));
    }
    if let Some(error) = error.downcast_ref::<APIProviderQuotaExhaustedError>() {
        return Some(status_message(LlmErrorKind::QuotaExhausted, &error.0));
    }
    if let Some(error) = error.downcast_ref::<APIContextOverflowError>() {
        return Some(status_message(LlmErrorKind::ContextOverflow, &error.0));
    }
    if let Some(error) = error.downcast_ref::<APIRequestTooLargeError>() {
        return Some(status_message(LlmErrorKind::RequestTooLarge, &error.0));
    }
    if let Some(error) = error.downcast_ref::<APIProviderOverloadedError>() {
        return Some(status_message(LlmErrorKind::Overloaded, &error.0));
    }
    if let Some(error) = error.downcast_ref::<APIConnectionError>() {
        return Some(LlmErrorMessage::new(
            LlmErrorKind::Connection,
            error.0.message.clone(),
            error.0.retryable,
        ));
    }
    if let Some(error) = error.downcast_ref::<APITimeoutError>() {
        return Some(LlmErrorMessage::new(
            LlmErrorKind::Timeout,
            error.0.message.clone(),
            error.0.retryable,
        ));
    }
    if let Some(error) = error.downcast_ref::<APIEmptyResponseError>() {
        return Some(LlmErrorMessage {
            kind: LlmErrorKind::EmptyResponse,
            message: error.0.message.clone(),
            code: None,
            status_code: None,
            request_id: None,
            retry_after_ms: None,
            headers: None,
            retryable: Some(error.0.retryable),
            finish_reason: error.finish_reason(),
            raw_finish_reason: error.raw_finish_reason().map(ToString::to_string),
        });
    }
    if let Some(error) = error.downcast_ref::<APIStatusError>() {
        return Some(status_message(LlmErrorKind::Status, error));
    }
    chat_error_of(error).map(|error| {
        LlmErrorMessage::new(
            LlmErrorKind::Provider,
            error.message.clone(),
            error.retryable,
        )
    })
}
