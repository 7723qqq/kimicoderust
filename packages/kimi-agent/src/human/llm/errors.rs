use std::collections::HashMap;

use regex::{Regex, RegexBuilder};
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::human::llm::finish_reason::FinishReason;
use crate::wire::record::WireAsyncError;
pub use crate::human::llm::syntax_errors::to_llm_syntax_error_message;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    Syntax,
    Abort,
    Connection,
    Timeout,
    Status,
    RateLimit,
    QuotaExhausted,
    Overloaded,
    ContextOverflow,
    RequestTooLarge,
    RequestStructure,
    ImageFormat,
    EmptyResponse,
    Provider,
    Unknown,
}

pub type LlmRemoteErrorKind = LlmErrorKind;

impl LlmErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Abort => "abort",
            Self::Connection => "connection",
            Self::Timeout => "timeout",
            Self::Status => "status",
            Self::RateLimit => "rate_limit",
            Self::QuotaExhausted => "quota_exhausted",
            Self::Overloaded => "overloaded",
            Self::ContextOverflow => "context_overflow",
            Self::RequestTooLarge => "request_too_large",
            Self::RequestStructure => "request_structure",
            Self::ImageFormat => "image_format",
            Self::EmptyResponse => "empty_response",
            Self::Provider => "provider",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmSyntaxErrorCode {
    RequestFormat,
    ThinkingConfig,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmErrorMessage {
    pub kind: LlmErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<LlmSyntaxErrorCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_finish_reason: Option<String>,
}

impl Serialize for LlmErrorMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("kind", &self.kind)?;
        map.serialize_entry("message", &self.message)?;
        match self.kind {
            LlmErrorKind::Syntax => {
                if let Some(code) = self.code {
                    map.serialize_entry("code", &code)?;
                }
            }
            LlmErrorKind::EmptyResponse => {
                map.serialize_entry("finishReason", &self.finish_reason)?;
                map.serialize_entry("rawFinishReason", &self.raw_finish_reason)?;
                if let Some(retryable) = self.retryable {
                    map.serialize_entry("retryable", &retryable)?;
                }
            }
            LlmErrorKind::Status
            | LlmErrorKind::RateLimit
            | LlmErrorKind::QuotaExhausted
            | LlmErrorKind::Overloaded
            | LlmErrorKind::ContextOverflow
            | LlmErrorKind::RequestTooLarge
            | LlmErrorKind::RequestStructure
            | LlmErrorKind::ImageFormat => {
                if let Some(status_code) = self.status_code {
                    map.serialize_entry("statusCode", &status_code)?;
                }
                map.serialize_entry("requestId", &self.request_id)?;
                map.serialize_entry("retryAfterMs", &self.retry_after_ms)?;
                map.serialize_entry("headers", &self.headers)?;
                if let Some(retryable) = self.retryable {
                    map.serialize_entry("retryable", &retryable)?;
                }
            }
            _ => {
                if let Some(retryable) = self.retryable {
                    map.serialize_entry("retryable", &retryable)?;
                }
            }
        }
        map.end()
    }
}

pub type LlmRemoteErrorMessage = LlmErrorMessage;
pub type LlmStatusErrorInfo = LlmErrorMessage;

impl LlmErrorMessage {
    pub fn new(kind: LlmErrorKind, message: String, retryable: bool) -> Self {
        Self {
            kind,
            message,
            code: None,
            status_code: None,
            request_id: None,
            retry_after_ms: None,
            headers: None,
            retryable: Some(retryable),
            finish_reason: None,
            raw_finish_reason: None,
        }
    }
}

pub fn sanitize_status_error_message(message: &str) -> String {
    let title = RegexBuilder::new(r"<title[^>]*>([\s\S]*?)</title>")
        .case_insensitive(true)
        .build()
        .unwrap()
        .captures(message)
        .and_then(|captures| captures.get(1).map(|value| value.as_str().trim()))
        .filter(|title| !title.is_empty());
    title.unwrap_or(message).replace('\r', "")
}

pub fn is_abort_error(error: &WireAsyncError) -> bool {
    let debug = format!("{error:?}");
    debug.contains("AbortError") || debug.contains("APIUserAbortError")
}

pub fn error_status_code(error: &Value) -> Option<i64> {
    let status = match error.get("status") {
        None | Some(Value::Null) => error.get("statusCode"),
        value => value,
    };
    status.and_then(Value::as_i64)
}

pub fn llm_status_error_message(error: &LlmErrorMessage) -> Option<LlmErrorMessage> {
    matches!(
        error.kind,
        LlmErrorKind::Status
            | LlmErrorKind::RateLimit
            | LlmErrorKind::QuotaExhausted
            | LlmErrorKind::Overloaded
            | LlmErrorKind::ContextOverflow
            | LlmErrorKind::RequestTooLarge
            | LlmErrorKind::RequestStructure
            | LlmErrorKind::ImageFormat
    )
    .then(|| error.clone())
}

pub fn to_llm_error_message(error: &WireAsyncError) -> LlmRemoteErrorMessage {
    if is_abort_error(error) {
        return LlmErrorMessage::new(LlmErrorKind::Abort, error.to_string(), false);
    }
    LlmErrorMessage::new(LlmErrorKind::Unknown, error.to_string(), false)
}

pub fn to_llm_transport_error_message(message: &str) -> LlmRemoteErrorMessage {
    if RegexBuilder::new(r"timed?\s*out|timeout|deadline")
        .case_insensitive(true)
        .build()
        .unwrap()
        .is_match(message)
    {
        return LlmErrorMessage::new(LlmErrorKind::Timeout, message.to_string(), true);
    }
    if RegexBuilder::new(r"network|connection|connect|disconnect|terminated")
        .case_insensitive(true)
        .build()
        .unwrap()
        .is_match(message)
    {
        return LlmErrorMessage::new(LlmErrorKind::Connection, message.to_string(), true);
    }
    LlmErrorMessage::new(LlmErrorKind::Provider, format!("Error: {message}"), true)
}

const THINKING_EFFORT_CONFIG_DOCS_URL: &str =
    "https://moonshotai.github.io/kimi-code/en/configuration/config-files.html#thinking";

pub fn append_thinking_effort_config_hint(status_code: u16, message: &str) -> String {
    if status_code != 400 && status_code != 422 {
        return message.to_string();
    }
    let lower = message.to_lowercase();
    let patterns = [
        r"reasoning[_ .-]?effort",
        r"thinking[_ .-]?effort",
        r"output_config[\s\S]*effort",
        r"unsupported[\s\S]*effort",
        r"invalid[\s\S]*effort",
    ];
    if !patterns.iter().any(|pattern| Regex::new(pattern).unwrap().is_match(&lower))
        || message.contains(THINKING_EFFORT_CONFIG_DOCS_URL)
    {
        return message.to_string();
    }
    format!(
        "{message}\n\nThe provider rejected the configured thinking effort. Non-Kimi providers receive effort strings without client-side mapping; choose an effort supported by the selected model. For Kimi models, check support_efforts and default_effort. See {THINKING_EFFORT_CONFIG_DOCS_URL}"
    )
}

#[derive(Debug, Clone)]
pub struct LlmStatusErrorInput {
    pub status_code: Option<u16>,
    pub message: String,
    pub request_id: Option<String>,
    pub retry_after_ms: Option<i64>,
    pub headers: Option<HashMap<String, String>>,
    pub retryable: bool,
}

pub fn is_transient_http_status(status_code: Option<u16>) -> bool {
    matches!(
        status_code,
        Some(408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
    )
}

pub fn to_llm_status_error_message(input: LlmStatusErrorInput) -> LlmRemoteErrorMessage {
    let kind = if input.status_code == Some(429) {
        LlmErrorKind::RateLimit
    } else if input.status_code.is_some_and(|status| {
        is_context_overflow_status_error(status, &input.message)
    }) {
        LlmErrorKind::ContextOverflow
    } else if input.status_code.is_some_and(|status| {
        is_request_too_large_status_error(status, &input.message)
    }) {
        LlmErrorKind::RequestTooLarge
    } else if input.status_code.is_some_and(|status| {
        is_provider_overload_status_error(status, &input.message)
    }) {
        LlmErrorKind::Overloaded
    } else if input.status_code.is_some_and(|status| {
        is_request_structure_status_error(status, &input.message)
    }) {
        LlmErrorKind::RequestStructure
    } else if input.status_code.is_some_and(|status| {
        is_image_format_status_error(status, &input.message)
    }) {
        LlmErrorKind::ImageFormat
    } else {
        LlmErrorKind::Status
    };
    LlmErrorMessage {
        kind,
        status_code: input.status_code,
        ..LlmErrorMessage::new(
            kind,
            input.message.clone(),
            input.retryable,
        )
    }
    .with_status_fields(input)
}

impl LlmErrorMessage {
    fn with_status_fields(mut self, input: LlmStatusErrorInput) -> Self {
        self.status_code = input.status_code;
        self.request_id = input.request_id;
        self.retry_after_ms = input.retry_after_ms;
        self.headers = input.headers;
        self
    }
}

pub fn to_llm_stream_error_message(
    message: &str,
    business_code: Option<i64>,
    request_id: Option<String>,
    retry_after_ms: Option<i64>,
    headers: Option<HashMap<String, String>>,
    retryable: bool,
) -> LlmRemoteErrorMessage {
    let kind = match business_code {
        Some(429) => LlmErrorKind::RateLimit,
        Some(529) => LlmErrorKind::Overloaded,
        Some(413) => LlmErrorKind::RequestTooLarge,
        _ => LlmErrorKind::Status,
    };
    LlmErrorMessage {
        kind,
        message: message.to_string(),
        code: None,
        status_code: None,
        request_id,
        retry_after_ms,
        headers,
        retryable: Some(retryable),
        finish_reason: None,
        raw_finish_reason: None,
    }
}

pub fn parse_retry_after_ms(headers: Option<&reqwest::header::HeaderMap>) -> Option<i64> {
    let raw = headers?.get("retry-after")?.to_str().ok()?.trim_start();
    let (negative, digits) = match raw.strip_prefix('-') {
        Some(raw) => (true, raw),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    };
    let digits = digits
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let seconds = digits.parse::<i64>().ok()?;
    if negative && seconds != 0 {
        return None;
    }
    seconds.checked_mul(1000)
}

pub fn headers_to_record(
    headers: Option<&reqwest::header::HeaderMap>,
) -> Option<HashMap<String, String>> {
    let headers = headers?;
    Some(
        headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect(),
    )
}

pub fn is_context_overflow_status_error(status_code: u16, message: &str) -> bool {
    if status_code != 400 && status_code != 413 && status_code != 422 {
        return false;
    }
    let lower = message.to_lowercase();
    [
        r"context[ _-]?length",
        r"(?:context[ _-]?window.*exceed|exceed.*context[ _-]?window)",
        r"maximum context",
        r"exceed(?:ed|s|ing)?\s+(?:the\s+)?max(?:imum)?\s+tokens?",
        r"(?:too many tokens.*(?:prompt|input|context)|(?:prompt|input|context).*too many tokens)",
        r"prompt is too long.*maximum",
        r"input token count.*exceeds?.*maximum number of tokens",
        r"request.*exceed(?:ed|s|ing)?.*model token limit",
    ]
    .iter()
    .any(|pattern| Regex::new(pattern).unwrap().is_match(&lower))
}

pub fn is_provider_overload_status_error(status_code: u16, message: &str) -> bool {
    if status_code == 529 {
        return true;
    }
    if status_code != 500 && status_code != 503 {
        return false;
    }
    Regex::new("overload")
        .unwrap()
        .is_match(&message.to_lowercase())
}

pub fn is_request_too_large_status_error(status_code: u16, message: &str) -> bool {
    if status_code != 413 {
        return false;
    }
    let lower = message.to_lowercase();
    [
        r"request exceeds the maximum size",
        r"request entity too large",
        r"request_too_large",
        r"exceeds? the maximum allowed number of bytes",
        r"payload too large",
        r"content too large",
        r"request (?:body )?too large",
    ]
    .iter()
    .any(|pattern| Regex::new(pattern).unwrap().is_match(&lower))
}

pub fn is_tool_exchange_adjacency_status_error(status_code: u16, message: &str) -> bool {
    if status_code != 400 && status_code != 422 {
        return false;
    }
    let lower = message.to_lowercase();
    [
        r"tool_use[\s\S]*tool_result",
        r"tool_result[\s\S]*tool_use",
        r"unexpected\s+`?tool_result",
        r"tool_call_id[\s\S]*not found",
        r#"role\s+['"`]?tool['"`]?\s+must be a response to a preceding message"#,
        r#"assistant message with\s+['"`]?tool_calls['"`]?\s+must be followed by tool messages"#,
        r"tool_call_ids? did not have response messages",
        r"insufficient tool messages following",
    ]
    .iter()
    .any(|pattern| Regex::new(pattern).unwrap().is_match(&lower))
}

pub fn is_request_structure_status_error(status_code: u16, message: &str) -> bool {
    if status_code != 400 && status_code != 422 {
        return false;
    }
    if is_tool_exchange_adjacency_status_error(status_code, message) {
        return true;
    }
    let lower = message.to_lowercase();
    [
        r"text content blocks must be non-empty",
        r"text content blocks must contain non-whitespace",
        r"first message must use the .*user.* role",
        r"roles must alternate",
        r"multiple .*(?:user|assistant).* roles in a row",
        r"tool_use[\s\S]*ids must be unique",
        r#"message at position \d+ with role ['"`]?[a-z]+['"`]? must not be empty"#,
    ]
    .iter()
    .any(|pattern| Regex::new(pattern).unwrap().is_match(&lower))
}

pub fn is_image_format_status_error(status_code: u16, message: &str) -> bool {
    if status_code != 400 {
        return false;
    }
    let lower = message.to_lowercase();
    [
        r"unsupported image (?:url|format|type)",
        r"does not represent a valid image",
        r"could not (?:process|decode) (?:the |input )?image",
        r"unable to process (?:the |input )?image",
        r"failed to decode (?:the )?image",
        r"invalid image(?: data| type| format)?",
    ]
    .iter()
    .any(|pattern| Regex::new(pattern).unwrap().is_match(&lower))
        || (Regex::new(r"(?:media|mime)_?type")
            .unwrap()
            .is_match(&lower)
            && lower.contains("image"))
}
