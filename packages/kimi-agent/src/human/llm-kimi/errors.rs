use serde_json::Value;

use crate::human::llm::errors::{
    LlmErrorKind, LlmRemoteErrorMessage, headers_to_record, parse_retry_after_ms,
};
use crate::human::llm::requester::bases::anthropic::format::AnthropicSdkError;
use crate::human::llm::requester::bases::openai::format::OpenAISdkError;
use crate::wire::record::WireAsyncError;

const KIMI_QUOTA_EXHAUSTED_ERROR_CODES: [&str; 1] = ["exceeded_current_quota_error"];

fn read_string_prop<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn read_error_object_prop(value: &Value) -> Option<&Value> {
    value
        .get("error")
        .filter(|value| value.is_object() || value.is_array())
}

fn contains_quota_code(value: &Value) -> bool {
    let mut current = Some(value);
    for _ in 0..3 {
        let Some(value) = current else {
            break;
        };
        if read_string_prop(value, "code")
            .into_iter()
            .chain(read_string_prop(value, "type"))
            .any(|code| KIMI_QUOTA_EXHAUSTED_ERROR_CODES.contains(&code))
        {
            return true;
        }
        current = read_error_object_prop(value);
    }
    false
}

fn quota_message(raw: &str, fallback: Option<String>) -> String {
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return fallback.unwrap_or_default();
    };
    let Some(nested_error) = read_error_object_prop(&parsed) else {
        return String::new();
    };
    if let Some(message) = read_string_prop(nested_error, "message") {
        return message.to_string();
    }
    read_error_object_prop(nested_error)
        .and_then(|error| read_string_prop(error, "message"))
        .unwrap_or_default()
        .to_string()
}

fn quota_error(
    status: Option<u16>,
    request_id: Option<String>,
    headers: Option<&reqwest::header::HeaderMap>,
    message: String,
) -> LlmRemoteErrorMessage {
    LlmRemoteErrorMessage {
        kind: LlmErrorKind::QuotaExhausted,
        message,
        code: None,
        status_code: status,
        request_id,
        retry_after_ms: parse_retry_after_ms(headers),
        headers: headers_to_record(headers),
        retryable: Some(false),
        finish_reason: None,
        raw_finish_reason: None,
    }
}

pub fn classify_kimi_quota_error(error: &WireAsyncError) -> Option<LlmRemoteErrorMessage> {
    if let Some(OpenAISdkError::Api(api)) = error.downcast_ref::<OpenAISdkError>() {
        if api.status.is_some_and(|status| status != 429) {
            return None;
        }
        let body = api.error.as_ref();
        let parsed = serde_json::from_str::<Value>(&api.message).ok();
        let structured_hit = [
            api.code.as_deref(),
            api.type_.as_deref(),
            body.and_then(|body| body.code.as_deref()),
            body.and_then(|body| body.type_.as_deref()),
        ]
        .into_iter()
        .flatten()
        .any(|code| KIMI_QUOTA_EXHAUSTED_ERROR_CODES.contains(&code))
            || parsed.as_ref().is_some_and(contains_quota_code);
        if !structured_hit {
            return None;
        }
        return Some(quota_error(
            api.status,
            api.request_id.clone(),
            api.headers.as_ref(),
            quota_message(
                &api.message,
                body.and_then(|body| body.message.clone()),
            ),
        ));
    }
    if let Some(AnthropicSdkError::Api(api)) = error.downcast_ref::<AnthropicSdkError>() {
        if api.status.is_some_and(|status| status != 429) {
            return None;
        }
        let parsed = serde_json::from_str::<Value>(&api.message).ok();
        let structured_hit = [api.type_.as_deref(), api.body_type.as_deref()]
            .into_iter()
            .flatten()
            .any(|code| KIMI_QUOTA_EXHAUSTED_ERROR_CODES.contains(&code))
            || parsed.as_ref().is_some_and(contains_quota_code);
        if !structured_hit {
            return None;
        }
        return Some(quota_error(
            api.status,
            api.request_id.clone(),
            api.headers.as_ref(),
            quota_message(&api.message, api.body_message.clone()),
        ));
    }
    None
}
