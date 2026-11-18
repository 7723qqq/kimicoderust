use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::human::llm::errors::{
    LlmErrorKind, LlmErrorMessage, LlmRemoteErrorMessage, headers_to_record, is_abort_error,
    is_transient_http_status, parse_retry_after_ms, to_llm_error_message,
    to_llm_status_error_message, to_llm_stream_error_message, to_llm_transport_error_message,
};
use crate::human::llm::finish_reason::{FinishInfo, FinishReason, NO_FINISH};
use crate::human::llm::message::{ContentPart, Message, StreamedMessagePart, ToolCall, ToolCallPart, ToolDescription};
use crate::human::llm::protocol::format::{
    FormatRequestInput, ProtocolFormat, StreamParser, StreamParserOptions,
};
use crate::human::llm::protocol::patterns::{MergeUsersPolicy, merge_consecutive_users};
use crate::human::llm::protocol::rewrite::apply_patterns;
use crate::human::llm::requester::bases::anthropic::contract::*;
use crate::human::llm::requester::bases::anthropic::lower::{lower_message, message_content};
use crate::human::llm::requester::bases::anthropic::patterns::{
    AUDIO_TO_PLACEHOLDER, StripUnsignedThinking,
};
use crate::human::llm::requester::bases::anthropic::profile::{
    resolve_default_max_tokens, should_preserve_unsigned_thinking,
};
use crate::human::llm::response_format::ResponseFormat;
use crate::human::llm::syntax_errors::SyntaxRequestFormatError;
use crate::human::llm::usage::TokenUsagePatch;
use crate::wire::record::WireAsyncError;

const CLEAR_THINKING_EDIT: &str = "clear_thinking_20251015";

fn is_tool_result_only(message: &AnthropicWireMessage) -> bool {
    if message.role != "user" {
        return false;
    }
    let content = message_content(message);
    !content.is_empty()
        && content
            .iter()
            .all(|block| matches!(block, AnthropicWireContentBlock::ToolResult { .. }))
}

fn normalize_stop_reason(raw: Option<&str>) -> FinishInfo {
    let Some(raw) = raw else {
        return NO_FINISH;
    };
    let finish_reason = match raw {
        "end_turn" | "stop_sequence" => FinishReason::Completed,
        "max_tokens" => FinishReason::Truncated,
        "tool_use" => FinishReason::ToolCalls,
        "pause_turn" => FinishReason::Paused,
        "refusal" => FinishReason::Filtered,
        _ => FinishReason::Other,
    };
    FinishInfo {
        finish_reason: Some(finish_reason),
        raw_finish_reason: Some(raw.to_string()),
    }
}

fn parse_raw_usage(usage: Option<&AnthropicRawUsage>) -> Option<TokenUsagePatch> {
    let usage = usage?;
    let mut raw = Map::new();
    if let Ok(value) = serde_json::to_value(usage)
        && let Some(object) = value.as_object()
    {
        raw = object.clone();
    }
    Some(TokenUsagePatch {
        input_other: usage.input_tokens,
        output: usage.output_tokens,
        input_cache_read: usage.cache_read_input_tokens,
        input_cache_creation: usage.cache_creation_input_tokens,
        raw: Some(raw),
    })
}

pub fn apply_anthropic_response_format(
    kwargs: &Map<String, Value>,
    format: &ResponseFormat,
) -> Result<Map<String, Value>, WireAsyncError> {
    let ResponseFormat::JsonSchema { json_schema } = format else {
        return Err(Box::new(SyntaxRequestFormatError::new(
            "Anthropic requires a JSON schema for structured response output.",
        )));
    };
    let mut output_config = kwargs
        .get("output_config")
        .and_then(|value| value.as_object())
        .cloned()
        .unwrap_or_default();
    output_config.insert(
        "format".to_string(),
        json!({ "type": "json_schema", "schema": json_schema.schema }),
    );
    let mut output = kwargs.clone();
    output.insert("output_config".to_string(), Value::Object(output_config));
    Ok(output)
}

pub fn apply_anthropic_thinking_keep(
    kwargs: &Map<String, Value>,
    keep: &str,
) -> Map<String, Value> {
    let beta_features = kwargs
        .get("betaFeatures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut beta_features = if beta_features.is_empty() {
        vec![Value::String(CONTEXT_MANAGEMENT_BETA.to_string())]
    } else {
        beta_features
    };
    if !beta_features.contains(&Value::String(CONTEXT_MANAGEMENT_BETA.to_string())) {
        beta_features.push(Value::String(CONTEXT_MANAGEMENT_BETA.to_string()));
    }
    let existing_edits = kwargs
        .get("context_management")
        .and_then(|value| value.get("edits"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut edits = vec![json!({ "type": CLEAR_THINKING_EDIT, "keep": keep })];
    edits.extend(
        existing_edits
            .into_iter()
            .filter(|edit| edit.get("type").and_then(Value::as_str) != Some(CLEAR_THINKING_EDIT)),
    );
    let mut output = kwargs.clone();
    output.insert("betaFeatures".to_string(), Value::Array(beta_features));
    output.insert(
        "context_management".to_string(),
        json!({ "edits": edits }),
    );
    output
}

pub fn encode_anthropic_max_tokens(cap: u64) -> Map<String, Value> {
    Map::from_iter([("max_tokens".to_string(), Value::from(cap))])
}

pub fn default_anthropic_tool(tool: &ToolDescription) -> Map<String, Value> {
    Map::from_iter([
        ("name".to_string(), Value::String(tool.name.clone())),
        (
            "description".to_string(),
            Value::String(tool.description.clone()),
        ),
        ("input_schema".to_string(), Value::Object(tool.parameters.clone())),
    ])
}

pub fn default_anthropic_merge_history(
    messages: &[AnthropicWireMessage],
) -> Vec<AnthropicWireMessage> {
    let merge = merge_consecutive_users(MergeUsersPolicy {
        is_user: |message: &crate::human::llm::requester::bases::anthropic::contract::AnthropicWireMessage| message.role == "user",
        is_tool_result_only,
        merge: |last, next| AnthropicWireMessage {
            role: last.role.clone(),
            content: message_content(&last)
                .into_iter()
                .chain(message_content(&next))
                .collect(),
        },
    });
    apply_patterns(messages, &[&merge])
}

#[derive(Debug, Clone)]
pub struct AnthropicLoweredMessage {
    pub source: Message,
    pub message: AnthropicWireMessage,
}

pub fn lower_anthropic_messages(
    input: &FormatRequestInput,
    accepted_mimes: &HashSet<String>,
) -> Vec<AnthropicLoweredMessage> {
    let strip = StripUnsignedThinking {
        preserve: should_preserve_unsigned_thinking(&input.model.model),
    };
    let normalized = apply_patterns(&input.messages, &[&strip, &AUDIO_TO_PLACEHOLDER]);
    normalized
        .into_iter()
        .flat_map(|message| {
            lower_message(&message, accepted_mimes)
                .into_iter()
                .map(move |wire| AnthropicLoweredMessage {
                    source: message.clone(),
                    message: wire,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct AnthropicRequestParams {
    pub params: Map<String, Value>,
    pub betas: Vec<String>,
    pub use_beta_api: bool,
}

#[derive(Debug, Clone)]
pub struct AnthropicRequestParts {
    pub messages: Vec<AnthropicWireMessage>,
    pub tools: Vec<Map<String, Value>>,
    pub kwargs: Map<String, Value>,
    pub beta_api: bool,
}

#[derive(Debug, Clone)]
pub struct AnthropicRequestAssembly {
    pub params: Map<String, Value>,
    pub betas: Vec<String>,
    pub use_beta_api: bool,
}

pub fn assemble_anthropic_request(
    input: &FormatRequestInput,
    parts: &AnthropicRequestParts,
) -> AnthropicRequestAssembly {
    let mut messages = parts.messages.clone();
    apply_cache_breakpoints(
        &mut messages
            .iter_mut()
            .map(|message| message.content.as_mut_slice())
            .collect::<Vec<_>>(),
        2,
    );
    let mut tools = parts.tools.clone();
    if let Some(last_tool) = tools.last_mut() {
        last_tool.insert("cache_control".to_string(), cache_control());
    }
    let beta_features = parts
        .kwargs
        .get("betaFeatures")
        .and_then(Value::as_array)
        .map(|features| {
            features
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let use_beta_api = parts.beta_api
        || input.model.connection.beta_api == Some(true)
        || input
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.keep.is_some());
    let mut params = Map::new();
    params.insert("model".to_string(), Value::String(input.model.model.clone()));
    params.insert(
        "max_tokens".to_string(),
        Value::from(resolve_default_max_tokens(&input.model.model)),
    );
    if let Some(cache_key) = &input.cache_key {
        params.insert(
            "metadata".to_string(),
            json!({ "user_id": cache_key }),
        );
    }
    for (key, value) in &parts.kwargs {
        if key != "betaFeatures" {
            params.insert(key.clone(), value.clone());
        }
    }
    if let Some(system_prompt) = input.system_prompt.as_ref().filter(|prompt| !prompt.is_empty()) {
        params.insert(
            "system".to_string(),
            json!([{ "type": "text", "text": system_prompt, "cache_control": cache_control() }]),
        );
    } else {
        params.remove("system");
    }
    params.insert("messages".to_string(), json!(messages));
    if !tools.is_empty() {
        params.insert("tools".to_string(), json!(tools));
    } else {
        params.remove("tools");
    }
    if use_beta_api && !beta_features.is_empty() {
        params.insert("betas".to_string(), json!(beta_features));
    } else {
        params.remove("betas");
    }
    params.insert("stream".to_string(), Value::Bool(true));
    AnthropicRequestAssembly {
        params,
        betas: beta_features,
        use_beta_api,
    }
}

pub fn encode_anthropic_request(assembly: AnthropicRequestAssembly) -> AnthropicRequestParams {
    AnthropicRequestParams {
        params: assembly.params,
        betas: assembly.betas,
        use_beta_api: assembly.use_beta_api,
    }
}

pub struct AnthropicProtocolFormat;

impl AnthropicProtocolFormat {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AnthropicProtocolFormat {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolFormat<AnthropicRawStreamEvent> for AnthropicProtocolFormat {
    fn create_stream_parser(
        &self,
        _options: StreamParserOptions<AnthropicRawStreamEvent>,
    ) -> StreamParser<AnthropicRawStreamEvent> {
        Arc::new(move |chunk, sink| {
            match chunk.type_.as_str() {
                "message_start" => {
                    if let Some(message) = &chunk.message {
                        if let Some(id) = message.id.as_ref().filter(|id| !id.is_empty())
                            && let Some(on_message_id) = &sink.on_message_id
                        {
                            on_message_id(id.clone());
                        }
                        if let Some(usage) = parse_raw_usage(message.usage.as_ref()) {
                            let mut input_usage = usage;
                            input_usage.output = None;
                            if let Some(on_usage) = &sink.on_usage {
                                on_usage(input_usage);
                            }
                        }
                    }
                }
                "message_delta" => {
                    if let Some(usage) = parse_raw_usage(chunk.usage.as_ref())
                        && let Some(on_usage) = &sink.on_usage
                    {
                        on_usage(usage);
                    }
                    if let Some(stop_reason) = chunk
                        .delta
                        .as_ref()
                        .and_then(|delta| delta.stop_reason.as_deref())
                    {
                        (sink.on_finish)(normalize_stop_reason(Some(stop_reason)));
                    }
                }
                "content_block_start" => {
                    if let Some(block) = &chunk.content_block {
                        let index = chunk.index.unwrap_or(0);
                        match block.type_.as_str() {
                            "tool_use" => {
                                (sink.on_delta)(StreamedMessagePart::ToolCall(ToolCall {
                                    id: block.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                                    name: block.name.clone().unwrap_or_default(),
                                    arguments: Some(String::new()),
                                    extras: None,
                                    raw_id: None,
                                    _stream_index: Some(crate::human::llm::message::StreamIndex::Number(index as u64)),
                                }));
                            }
                            "thinking" if block.thinking.as_ref().is_some_and(|thinking| !thinking.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                                    think: block.thinking.clone().unwrap(),
                                    encrypted: None,
                                    details_index: None,
                                    hidden: None,
                                    reasoning_key: None,
                                }));
                            }
                            "redacted_thinking" if block.data.as_ref().is_some_and(|data| !data.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                                    think: String::new(),
                                    encrypted: block.data.clone(),
                                    details_index: None,
                                    hidden: None,
                                    reasoning_key: None,
                                }));
                            }
                            "text" if block.text.as_ref().is_some_and(|text| !text.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Text {
                                    text: block.text.clone().unwrap(),
                                    meta: None,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = &chunk.delta {
                        let index = chunk.index.unwrap_or(0);
                        match delta.type_.as_deref() {
                            Some("text_delta") if delta.text.as_ref().is_some_and(|text| !text.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Text {
                                    text: delta.text.clone().unwrap(),
                                    meta: None,
                                }));
                            }
                            Some("thinking_delta") if delta.thinking.as_ref().is_some_and(|thinking| !thinking.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                                    think: delta.thinking.clone().unwrap(),
                                    encrypted: None,
                                    details_index: None,
                                    hidden: None,
                                    reasoning_key: None,
                                }));
                            }
                            Some("input_json_delta") if delta.partial_json.as_ref().is_some_and(|json| !json.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::ToolCallPart(ToolCallPart {
                                    arguments_part: delta.partial_json.clone(),
                                    index: Some(crate::human::llm::message::StreamIndex::Number(index as u64)),
                                }));
                            }
                            Some("signature_delta") if delta.signature.as_ref().is_some_and(|signature| !signature.is_empty()) => {
                                (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                                    think: String::new(),
                                    encrypted: delta.signature.clone(),
                                    details_index: None,
                                    hidden: None,
                                    reasoning_key: None,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }
}

pub static ANTHROPIC_FORMAT: AnthropicProtocolFormat = AnthropicProtocolFormat;

#[derive(Debug, Clone)]
pub struct AnthropicApiError {
    pub status: Option<u16>,
    pub request_id: Option<String>,
    pub headers: Option<reqwest::header::HeaderMap>,
    pub type_: Option<String>,
    pub body_type: Option<String>,
    pub body_message: Option<String>,
    pub message: String,
    pub has_body: bool,
}

#[derive(Debug)]
pub enum AnthropicSdkError {
    ConnectionTimeout(String),
    Connection(String),
    Api(AnthropicApiError),
    Other(String),
}

impl std::fmt::Display for AnthropicSdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionTimeout(message) | Self::Connection(message) | Self::Other(message) => f.write_str(message),
            Self::Api(error) => f.write_str(&error.message),
        }
    }
}

impl std::error::Error for AnthropicSdkError {}

pub fn convert_anthropic_error(
    error: &WireAsyncError,
    classify_error_hook: Option<&crate::human::llm::requester::requester::LlmErrorClassifier>,
    source_stream: bool,
) -> LlmRemoteErrorMessage {
    if is_abort_error(error) {
        return to_llm_error_message(error);
    }
    if let Some(hooked) = classify_error_hook.and_then(|classify| classify(error)) {
        return hooked;
    }
    let Some(sdk) = error.downcast_ref::<AnthropicSdkError>() else {
        return to_llm_transport_error_message(&error.to_string());
    };
    match sdk {
        AnthropicSdkError::ConnectionTimeout(message) => LlmErrorMessage::new(
            LlmErrorKind::Timeout,
            message.clone(),
            true,
        ),
        AnthropicSdkError::Connection(message) => LlmErrorMessage::new(
            LlmErrorKind::Connection,
            message.clone(),
            true,
        ),
        AnthropicSdkError::Other(message) => to_llm_transport_error_message(message),
        AnthropicSdkError::Api(api) => {
            let body_type = api.body_type.clone().or_else(|| api.type_.clone());
            let status_code = if source_stream { None } else { api.status };
            let request_id = api.request_id.clone();
            let retry_after_ms = parse_retry_after_ms(api.headers.as_ref());
            let headers = headers_to_record(api.headers.as_ref());
            let body_message = api.body_message.clone().unwrap_or_default();
            let mut result = if body_type.as_deref() == Some("insufficient_quota")
                || body_type.as_deref() == Some("exceeded_current_quota_error")
            {
                LlmErrorMessage::new(LlmErrorKind::QuotaExhausted, body_message.clone(), false)
            } else if body_type.as_deref() == Some("overloaded_error")
                || body_type.as_deref() == Some("engine_overloaded_error")
            {
                LlmErrorMessage::new(LlmErrorKind::Overloaded, body_message.clone(), true)
            } else if matches!(body_type.as_deref(), Some("rate_limit_error" | "rate_limit_reached_error" | "rate_limit_exceeded")) {
                LlmErrorMessage::new(LlmErrorKind::RateLimit, body_message.clone(), true)
            } else if body_type.as_deref() == Some("context_length_exceeded") {
                LlmErrorMessage::new(LlmErrorKind::ContextOverflow, body_message.clone(), false)
            } else if let Some(status) = status_code {
                return to_llm_status_error_message(crate::human::llm::errors::LlmStatusErrorInput {
                    status_code: Some(status),
                    message: if body_message.is_empty() { api.message.clone() } else { body_message.clone() },
                    request_id,
                    retry_after_ms,
                    headers,
                    retryable: is_transient_http_status(Some(status)),
                });
            } else if api.has_body {
                return to_llm_stream_error_message(
                    &body_message,
                    api.status.map(i64::from),
                    request_id,
                    retry_after_ms,
                    headers,
                    api.status.is_none() || is_transient_http_status(api.status),
                );
            } else if source_stream {
                return to_llm_stream_error_message(
                    "",
                    api.status.map(i64::from),
                    request_id,
                    retry_after_ms,
                    headers,
                    api.status.is_none() || is_transient_http_status(api.status),
                );
            } else {
                return to_llm_transport_error_message(&api.message);
            };
            result.status_code = status_code;
            result.request_id = request_id;
            result.retry_after_ms = retry_after_ms;
            result.headers = headers;
            result
        }
    }
}

pub fn cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

fn apply_cache_breakpoints(contents: &mut [&mut [AnthropicWireContentBlock]], history_gap: usize) {
    if let Some(tail) = contents.last_mut() {
        mark_last_block(tail, false);
    }
    if contents.len() < history_gap + 2 {
        return;
    }
    let stable_index = contents.len() - history_gap - 1;
    mark_last_block(&mut contents[stable_index], true);
}

fn mark_last_block(content: &mut [AnthropicWireContentBlock], keep_existing: bool) {
    let Some(block) = content.last_mut() else {
        return;
    };
    if !matches!(
        block,
        AnthropicWireContentBlock::Text { .. }
            | AnthropicWireContentBlock::Image { .. }
            | AnthropicWireContentBlock::Document { .. }
            | AnthropicWireContentBlock::ToolUse { .. }
            | AnthropicWireContentBlock::ToolResult { .. }
    ) {
        return;
    }
    if keep_existing && block.cache_control_mut().is_some() {
        return;
    }
    *block.cache_control_mut() = Some(AnthropicCacheControl {
        type_: "ephemeral".to_string(),
    });
}
