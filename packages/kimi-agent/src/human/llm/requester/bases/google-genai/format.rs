use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};

use crate::human::llm::errors::{
    LlmErrorKind, LlmErrorMessage, LlmRemoteErrorMessage, LlmStatusErrorInput, is_abort_error,
    is_transient_http_status, to_llm_error_message, to_llm_status_error_message,
    to_llm_stream_error_message,
};
use crate::human::llm::finish_reason::{FinishInfo, FinishReason, NO_FINISH};
use crate::human::llm::message::{
    ContentPart, Message, StreamedMessagePart, ToolCall, ToolDescription,
};
use crate::human::llm::protocol::format::{
    ProtocolFormat, StreamParseSink, StreamParser, StreamParserOptions,
};
use crate::human::llm::protocol::patterns::{MergeUsersPolicy, merge_consecutive_users};
use crate::human::llm::protocol::rewrite::apply_patterns;
use crate::human::llm::response_format::ResponseFormat;
use crate::human::llm::requester::bases::google_genai::contract::GoogleContent;
use crate::human::llm::requester::bases::google_genai::lower::{
    GoogleGenAILowerContext, build_tool_name_by_id, build_tool_raw_id_by_id,
    is_modern_gemini_model, lower_message,
};
use crate::human::llm::requester::bases::google_genai::patterns::SORT_TOOL_RUN_BY_CALL_ORDER;
use crate::human::llm::requester::requester::LlmErrorClassifier;
use crate::human::llm::thinking::{
    ThinkingConfigError, ThinkingConfigErrorCode, ThinkingEffort,
};
use crate::human::llm::usage::TokenUsagePatch;
use crate::wire::record::WireAsyncError;

pub fn default_google_gen_ai_tool(tool: &ToolDescription) -> Map<String, Value> {
    Map::from_iter([(
        "functionDeclarations".to_string(),
        json!([{
            "name": tool.name,
            "description": tool.description,
            "parametersJsonSchema": tool.parameters,
        }]),
    )])
}

pub fn lower_google_gen_ai_messages(
    messages: &[Message],
    model_name: &str,
) -> Result<Vec<GoogleContent>, WireAsyncError> {
    let normalized = apply_patterns(messages, &[&SORT_TOOL_RUN_BY_CALL_ORDER]);
    let tool_name_by_id = build_tool_name_by_id(&normalized);
    let tool_raw_id_by_id = build_tool_raw_id_by_id(&normalized);
    let lower = GoogleGenAILowerContext {
        tool_name_by_id,
        tool_raw_id_by_id,
        modern_function_responses: is_modern_gemini_model(model_name),
    };
    let mut lowered = Vec::new();
    for message in &normalized {
        lowered.extend(lower_message(message, &lower)?);
    }
    let merge = merge_consecutive_users(MergeUsersPolicy {
        is_user: |content: &GoogleContent| content.role == "user",
        is_tool_result_only: |content: &GoogleContent| {
            content
                .parts
                .first()
                .is_some_and(|part| part.function_response.is_some())
        },
        merge: |last: GoogleContent, next: GoogleContent| {
            let last_starts_with_function_response = last
                .parts
                .first()
                .is_some_and(|part| part.function_response.is_some());
            let next_has_function_response = next
                .parts
                .iter()
                .any(|part| part.function_response.is_some());
            if last_starts_with_function_response && !next_has_function_response {
                let mut parts = next.parts.clone();
                parts.extend(last.parts);
                GoogleContent { parts, ..next }
            } else {
                let mut parts = last.parts.clone();
                parts.extend(next.parts);
                GoogleContent { parts, ..last }
            }
        },
    });
    Ok(apply_patterns(&lowered, &[&merge]))
}

fn nullish_get<'a>(object: &'a Map<String, Value>, key: &str, fallback: &str) -> Option<&'a Value> {
    match object.get(key) {
        None | Some(Value::Null) => object.get(fallback),
        value => value,
    }
}

fn extract_chunk_finish_reason(response: &Map<String, Value>) -> Option<Value> {
    response
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(Value::as_object)
        .and_then(|first| nullish_get(first, "finishReason", "finish_reason"))
        .cloned()
}

fn normalize_finish_reason(raw: Option<&Value>) -> FinishInfo {
    let Some(raw) = raw else {
        return NO_FINISH;
    };
    if raw.is_null() {
        return NO_FINISH;
    }
    let raw_string = match raw {
        Value::String(raw) => raw.to_uppercase(),
        Value::Number(_) | Value::Bool(_) => raw.to_string().to_uppercase(),
        _ => return NO_FINISH,
    };
    if raw_string == "FINISH_REASON_UNSPECIFIED" || raw_string.is_empty() {
        return NO_FINISH;
    }
    let finish_reason = match raw_string.as_str() {
        "STOP" => FinishReason::Completed,
        "MAX_TOKENS" => FinishReason::Truncated,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII"
        | "IMAGE_SAFETY" => FinishReason::Filtered,
        _ => FinishReason::Other,
    };
    FinishInfo {
        finish_reason: Some(finish_reason),
        raw_finish_reason: Some(raw_string),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn extract_chunk_parts(response: &Map<String, Value>) -> Vec<StreamedMessagePart> {
    let mut parts = Vec::new();
    let Some(candidates) = response.get("candidates").and_then(Value::as_array) else {
        return parts;
    };
    for candidate in candidates {
        let Some(content_parts) = candidate
            .get("content")
            .and_then(Value::as_object)
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for part in content_parts {
            let Some(part) = part.as_object() else {
                continue;
            };
            let thought_signature = nullish_get(part, "thoughtSignature", "thought_signature")
                .and_then(Value::as_str)
                .filter(|signature| !signature.is_empty());
            let signature_meta = thought_signature.map(|signature| {
                Map::from_iter([(
                    "thought_signature_b64".to_string(),
                    Value::String(signature.to_string()),
                )])
            });
            if part.get("thought").and_then(Value::as_bool) == Some(true)
                && part.get("text").and_then(Value::as_str).is_some()
            {
                parts.push(StreamedMessagePart::Content(ContentPart::Think {
                    think: part
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_string(),
                    encrypted: thought_signature.map(str::to_string),
                    details_index: None,
                    hidden: None,
                    reasoning_key: None,
                }));
            } else if part.get("text").is_some_and(truthy) {
                let Some(text) = part.get("text").and_then(Value::as_str) else {
                    continue;
                };
                parts.push(StreamedMessagePart::Content(ContentPart::Text {
                    text: text.to_string(),
                    meta: signature_meta.clone(),
                }));
            } else if let Some(function_call) = match part.get("functionCall") {
                Some(value) if truthy(value) => Some(value),
                _ => part.get("function_call").filter(|value| truthy(value)),
            }
            .and_then(Value::as_object)
            {
                let Some(name) = function_call.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                let raw_id = function_call
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                let internal_id = raw_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let entropy = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
                let arguments = function_call
                    .get("args")
                    .filter(|args| truthy(args))
                    .map(|args| serde_json::to_string(args).unwrap())
                    .unwrap_or_else(|| "{}".to_string());
                let extras = signature_meta.clone();
                parts.push(StreamedMessagePart::ToolCall(ToolCall {
                    id: format!("{name}_{internal_id}_{entropy}"),
                    name: name.to_string(),
                    arguments: Some(arguments),
                    extras,
                    raw_id,
                    _stream_index: None,
                }));
            }
        }
    }
    parts
}

pub fn encode_google_gen_ai_thinking(model: &str, effort: &ThinkingEffort) -> Map<String, Value> {
    if model.contains("gemini-3") || model.contains("gemini-4") {
        if effort == "off" && model.contains("gemini-4") {
            let error = ThinkingConfigError::new(
                ThinkingConfigErrorCode::ThinkingCannotDisable,
                format!(
                    "Model '{model}' always reasons and thinking cannot be turned off. Choose a concrete thinking effort (low, medium, high) instead of 'off'."
                ),
            );
            std::panic::panic_any(error);
        }
        let mut result = Map::new();
        match effort.as_str() {
            "off" => {
                result.insert("includeThoughts".to_string(), Value::Bool(false));
                result.insert("thinkingLevel".to_string(), json!("MINIMAL"));
            }
            "low" | "medium" | "high" | "xhigh" | "max" => {
                result.insert("includeThoughts".to_string(), Value::Bool(true));
                let level = match effort.as_str() {
                    "low" => "LOW",
                    "medium" => "MEDIUM",
                    _ => "HIGH",
                };
                result.insert("thinkingLevel".to_string(), json!(level));
            }
            _ => {
                result.insert("includeThoughts".to_string(), Value::Bool(true));
            }
        }
        return result;
    }
    let mut result = Map::new();
    match effort.as_str() {
        "off" => {
            result.insert("includeThoughts".to_string(), Value::Bool(false));
            result.insert("thinkingBudget".to_string(), Value::from(0));
        }
        "low" | "medium" | "high" | "xhigh" | "max" => {
            result.insert("includeThoughts".to_string(), Value::Bool(true));
            let budget = match effort.as_str() {
                "low" => 1024,
                "medium" => 4096,
                _ => 32000,
            };
            result.insert("thinkingBudget".to_string(), Value::from(budget));
        }
        _ => {
            result.insert("includeThoughts".to_string(), Value::Bool(true));
        }
    }
    result
}

pub fn encode_google_gen_ai_max_output_tokens(cap: u64) -> Map<String, Value> {
    Map::from_iter([("maxOutputTokens".to_string(), Value::from(cap))])
}

pub fn apply_google_gen_ai_response_format(
    kwargs: &Map<String, Value>,
    format: &ResponseFormat,
) -> Map<String, Value> {
    let mut result = kwargs.clone();
    result.remove("responseSchema");
    result.remove("responseJsonSchema");
    result.insert(
        "responseMimeType".to_string(),
        Value::String("application/json".to_string()),
    );
    if let ResponseFormat::JsonSchema { json_schema } = format {
        result.insert(
            "responseJsonSchema".to_string(),
            Value::Object(json_schema.schema.clone()),
        );
    }
    result
}

#[derive(Debug, Clone)]
pub struct GoogleGenAIRequestParams {
    pub params: Map<String, Value>,
    pub headers: Option<HashMap<String, String>>,
}

pub struct GoogleGenAIRequestParts {
    pub contents: Vec<GoogleContent>,
    pub tools: Vec<Map<String, Value>>,
    pub kwargs: Map<String, Value>,
}

pub fn assemble_google_gen_ai_request(
    input: &crate::human::llm::protocol::format::FormatRequestInput,
    parts: &GoogleGenAIRequestParts,
) -> Map<String, Value> {
    let mut config = Map::new();
    if let Some(system_prompt) = &input.system_prompt
        && !system_prompt.is_empty()
    {
        config.insert(
            "systemInstruction".to_string(),
            Value::String(system_prompt.clone()),
        );
    }
    if !parts.tools.is_empty() {
        config.insert("tools".to_string(), json!(parts.tools));
    }
    config.extend(parts.kwargs.clone());
    Map::from_iter([
        ("model".to_string(), Value::String(input.model.model.clone())),
        ("contents".to_string(), json!(parts.contents)),
        ("config".to_string(), Value::Object(config)),
    ])
}

pub fn encode_google_gen_ai_request(params: Map<String, Value>) -> GoogleGenAIRequestParams {
    GoogleGenAIRequestParams {
        params,
        headers: None,
    }
}

pub struct GoogleGenAIProtocolFormat;

impl GoogleGenAIProtocolFormat {
    pub fn new() -> Self {
        Self
    }
}

impl Default for GoogleGenAIProtocolFormat {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolFormat<Value> for GoogleGenAIProtocolFormat {
    fn create_stream_parser(
        &self,
        _options: StreamParserOptions<Value>,
    ) -> StreamParser<Value> {
        Arc::new(move |chunk, sink: &StreamParseSink| {
            let Value::Object(response) = chunk else {
                return Ok(());
            };
            let raw_finish = extract_chunk_finish_reason(&response);
            if let Some(response_id) = response
                .get("responseId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                && let Some(on_message_id) = &sink.on_message_id
            {
                on_message_id(response_id.to_string());
            }
            if let Some(usage) = parse_usage_metadata(&response)
                && let Some(on_usage) = &sink.on_usage
            {
                on_usage(usage);
            }
            if raw_finish.as_ref().is_some_and(|raw| !raw.is_null()) {
                (sink.on_finish)(normalize_finish_reason(raw_finish.as_ref()));
            }
            for part in extract_chunk_parts(&response) {
                (sink.on_delta)(part);
            }
            Ok(())
        })
    }
}

fn parse_usage_metadata(response: &Map<String, Value>) -> Option<TokenUsagePatch> {
    let usage = response.get("usageMetadata")?.as_object()?;
    let number = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let prompt_token_count = number("promptTokenCount");
    let cached_content_token_count = number("cachedContentTokenCount");
    let candidates_token_count = number("candidatesTokenCount");
    Some(TokenUsagePatch {
        input_other: Some(prompt_token_count.saturating_sub(cached_content_token_count)),
        output: Some(candidates_token_count),
        input_cache_read: Some(cached_content_token_count),
        input_cache_creation: Some(0),
        raw: Some(usage.clone()),
    })
}

static NETWORK_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"network|connection|connect|disconnect|fetch failed")
        .case_insensitive(true)
        .build()
        .unwrap()
});
static TIMEOUT_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(r"timed?\s*out|timeout|deadline")
        .case_insensitive(true)
        .build()
        .unwrap()
});
static RETRY_DELAY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+(?:\.\d+)?)s$").unwrap());

#[derive(Debug, Clone)]
pub struct GoogleGenAIApiError {
    pub status: u16,
    pub message: String,
}

#[derive(Debug)]
pub enum GoogleGenAISdkError {
    Api(GoogleGenAIApiError),
    Other {
        message: String,
        code: Option<u16>,
    },
}

impl std::fmt::Display for GoogleGenAISdkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Api(error) => formatter.write_str(&error.message),
            Self::Other { message, .. } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for GoogleGenAISdkError {}

pub fn convert_google_gen_ai_error(
    error: &WireAsyncError,
    classify_error_hook: Option<&LlmErrorClassifier>,
    source_stream: bool,
) -> LlmRemoteErrorMessage {
    if is_abort_error(error) {
        return to_llm_error_message(error);
    }
    if let Some(hooked) = classify_error_hook.and_then(|classify| classify(error)) {
        return hooked;
    }
    if let Some(GoogleGenAISdkError::Api(api)) = error.downcast_ref::<GoogleGenAISdkError>() {
        let body = parse_google_gen_ai_error_body(&api.message);
        let error_body = body
            .as_ref()
            .and_then(|body| body.get("error"))
            .and_then(Value::as_object);
        let message = error_body
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let code = error_body
            .and_then(|error| error.get("code"))
            .and_then(Value::as_i64);
        let status = error_body
            .and_then(|error| error.get("status"))
            .and_then(Value::as_str);
        let status_code = if source_stream { None } else { Some(api.status) };
        let retry_after_ms = parse_retry_info_delay_ms(body.as_ref());
        if code == Some(429) || status == Some("RESOURCE_EXHAUSTED") {
            let mut result = LlmErrorMessage::new(LlmErrorKind::RateLimit, message, true);
            result.status_code = status_code;
            result.retry_after_ms = retry_after_ms;
            return result;
        }
        if code == Some(529) || status == Some("OVERLOADED") {
            let mut result = LlmErrorMessage::new(LlmErrorKind::Overloaded, message, true);
            result.status_code = status_code;
            result.retry_after_ms = retry_after_ms;
            return result;
        }
        if source_stream {
            let business_code = code.or_else(|| Some(i64::from(api.status)));
            return to_llm_stream_error_message(
                &message,
                business_code,
                None,
                retry_after_ms,
                None,
                business_code.is_none_or(|code| is_transient_http_status(u16::try_from(code).ok())),
            );
        }
        return to_llm_status_error_message(LlmStatusErrorInput {
            status_code: Some(api.status),
            message: if message.is_empty() {
                api.message.clone()
            } else {
                message
            },
            request_id: None,
            retry_after_ms,
            headers: None,
            retryable: is_transient_http_status(Some(api.status)),
        });
    }
    let message = error.to_string();
    if TIMEOUT_RE.is_match(&message) {
        return LlmErrorMessage::new(LlmErrorKind::Timeout, message, true);
    }
    if NETWORK_RE.is_match(&message) {
        return LlmErrorMessage::new(LlmErrorKind::Connection, message, true);
    }
    let code = match error.downcast_ref::<GoogleGenAISdkError>() {
        Some(GoogleGenAISdkError::Other { code, .. }) => *code,
        _ => None,
    };
    if let Some(code) = code {
        if source_stream {
            return to_llm_stream_error_message(
                &message,
                Some(i64::from(code)),
                None,
                None,
                None,
                is_transient_http_status(Some(code)),
            );
        }
        return to_llm_status_error_message(LlmStatusErrorInput {
            status_code: Some(code),
            message,
            request_id: None,
            retry_after_ms: None,
            headers: None,
            retryable: is_transient_http_status(Some(code)),
        });
    }
    LlmErrorMessage::new(
        LlmErrorKind::Provider,
        format!("GoogleGenAI error: {message}"),
        true,
    )
}

fn parse_google_gen_ai_error_body(message: &str) -> Option<Value> {
    let json_start = message.find('{')?;
    let body = serde_json::from_str::<Value>(&message[json_start..]).ok()?;
    body.is_object().then_some(body)
}

fn parse_retry_info_delay_ms(body: Option<&Value>) -> Option<i64> {
    let details = body?.get("error")?.get("details")?.as_array()?;
    for detail in details {
        let Some(detail) = detail.as_object() else {
            continue;
        };
        let Some(type_) = detail.get("@type").and_then(Value::as_str) else {
            continue;
        };
        if !type_.ends_with("google.rpc.RetryInfo") {
            continue;
        }
        let Some(retry_delay) = detail.get("retryDelay").and_then(Value::as_str) else {
            continue;
        };
        let Some(captures) = RETRY_DELAY_RE.captures(retry_delay.trim()) else {
            continue;
        };
        let Some(seconds) = captures
            .get(1)
            .and_then(|seconds| seconds.as_str().parse::<f64>().ok())
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        else {
            continue;
        };
        return Some((seconds * 1000.0).round() as i64);
    }
    None
}
