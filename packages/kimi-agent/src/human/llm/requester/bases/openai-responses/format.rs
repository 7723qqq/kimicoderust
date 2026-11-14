use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::human::llm::errors::{LlmErrorKind, LlmErrorMessage, LlmRemoteErrorMessage};
use crate::human::llm::finish_reason::{FinishInfo, FinishReason, NO_FINISH};
use crate::human::llm::message::{ContentPart, StreamedMessagePart, ToolCall, ToolCallPart, ToolDescription};
use crate::human::llm::protocol::format::{
    FormatRequestInput, ProtocolFormat, StreamParser, StreamParserOptions,
};
use crate::human::llm::requester::bases::openai::format::{
    is_context_overflow_error_code, is_open_ai_insufficient_quota_code,
};
use crate::human::llm::requester::bases::openai::usage::classify_deep_seek_cache_split;
use crate::human::llm::requester::bases::openai_responses::contract::*;
use crate::human::llm::requester::bases::openai_responses::lower::{
    OpenAIResponsesLowerOptions, lower_message,
};
use crate::human::llm::response_format::ResponseFormat;
use crate::human::llm::usage::{TokenUsage, TokenUsagePatch};
use crate::wire::record::WireAsyncError;

pub fn response_format_to_responses_text(format: &ResponseFormat) -> Map<String, Value> {
    match format {
        ResponseFormat::JsonObject => Map::from_iter([(
            "type".to_string(),
            Value::String("json_object".to_string()),
        )]),
        ResponseFormat::JsonSchema { json_schema } => {
            let mut output = Map::from_iter([
                ("type".to_string(), Value::String("json_schema".to_string())),
                ("name".to_string(), Value::String(json_schema.name.clone())),
                ("schema".to_string(), Value::Object(json_schema.schema.clone())),
            ]);
            if let Some(strict) = json_schema.strict {
                output.insert("strict".to_string(), Value::Bool(strict));
            }
            if let Some(description) = &json_schema.description {
                output.insert("description".to_string(), Value::String(description.clone()));
            }
            output
        }
    }
}

#[derive(Debug, Clone)]
enum ResponseOutputItemView {
    Message {
        #[allow(dead_code)]
        content: Vec<Map<String, Value>>,
    },
    FunctionCall {
        item_id: Option<String>,
        call_id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    Reasoning {
        encrypted_content: Option<String>,
        #[allow(dead_code)]
        summary: Vec<Map<String, Value>>,
    },
    Other,
}

fn as_raw_object(value: &Value) -> Option<Map<String, Value>> {
    value.as_object().cloned()
}

fn read_string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_string)
}

fn has_own(object: &Map<String, Value>, key: &str) -> bool {
    object.contains_key(key)
}

fn read_nullable_string_field(object: &Map<String, Value>, key: &str) -> Option<Option<String>> {
    let value = object.get(key)?;
    if value.is_null() {
        return Some(None);
    }
    value.as_str().map(|value| Some(value.to_string()))
}

fn read_number_field(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64)
}

fn read_object_field(object: &Map<String, Value>, key: &str) -> Option<Map<String, Value>> {
    object.get(key).and_then(as_raw_object)
}

fn read_object_array_field(object: &Map<String, Value>, key: &str) -> Option<Vec<Map<String, Value>>> {
    object.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(as_raw_object)
            .collect::<Vec<_>>()
    })
}

fn fail_responses_decode<T>(context: &str, detail: &str) -> Result<T, WireAsyncError> {
    Err(format!("OpenAI Responses decode error: {context} {detail}").into())
}

fn require_string_field(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<String, WireAsyncError> {
    match read_string_field(object, key) {
        Some(value) => Ok(value),
        None => fail_responses_decode(&format!("{context}.{key}"), "must be a string."),
    }
}

fn require_object_field(
    object: &Map<String, Value>,
    key: &str,
    context: &str,
) -> Result<Map<String, Value>, WireAsyncError> {
    match read_object_field(object, key) {
        Some(value) => Ok(value),
        None => fail_responses_decode(&format!("{context}.{key}"), "must be an object."),
    }
}

fn read_response_output_item(value: &Value, context: &str) -> Result<ResponseOutputItemView, WireAsyncError> {
    let Some(item) = as_raw_object(value) else {
        return fail_responses_decode(context, "must be an object.");
    };
    let type_ = require_string_field(&item, "type", context)?;
    match type_.as_str() {
        "message" => Ok(ResponseOutputItemView::Message {
            content: read_object_array_field(&item, "content").unwrap_or_default(),
        }),
        "function_call" => Ok(ResponseOutputItemView::FunctionCall {
            item_id: read_string_field(&item, "id"),
            call_id: read_string_field(&item, "call_id"),
            name: read_string_field(&item, "name"),
            arguments: read_nullable_string_field(&item, "arguments").flatten(),
        }),
        "reasoning" => Ok(ResponseOutputItemView::Reasoning {
            encrypted_content: read_string_field(&item, "encrypted_content"),
            summary: read_object_array_field(&item, "summary").unwrap_or_default(),
        }),
        _ => Ok(ResponseOutputItemView::Other),
    }
}

fn response_stream_index(item_id: Option<String>, output_index: Option<u64>) -> Option<crate::human::llm::message::StreamIndex> {
    item_id
        .map(crate::human::llm::message::StreamIndex::String)
        .or_else(|| output_index.map(crate::human::llm::message::StreamIndex::Number))
}

fn format_response_stream_index(stream_index: Option<&crate::human::llm::message::StreamIndex>) -> String {
    match stream_index {
        Some(crate::human::llm::message::StreamIndex::String(index)) => index.clone(),
        Some(crate::human::llm::message::StreamIndex::Number(index)) => index.to_string(),
        None => "<unindexed>".to_string(),
    }
}

fn require_function_call_name(name: Option<String>) -> Result<String, WireAsyncError> {
    name.ok_or_else(|| "OpenAI Responses function_call item is missing a name.".into())
}

fn function_call_id(call_id: Option<String>) -> String {
    call_id
        .filter(|call_id| !call_id.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn format_responses_error_event(code: Option<&str>, message: &str, param: Option<&str>) -> String {
    let code = code.unwrap_or("unknown");
    let param = param.map_or(String::new(), |param| format!(" (param: {param})"));
    format!("{code}: {message}{param}")
}

fn read_embedded_status_code(message: &str) -> Option<u16> {
    regex::Regex::new(r"\bstatus_code\s*[:=]\s*(\d{3})\b")
        .unwrap()
        .captures(message)
        .and_then(|captures| captures.get(1))
        .and_then(|status| status.as_str().parse().ok())
}

fn error_from_open_ai_responses_event(
    prefix: &str,
    code: Option<String>,
    message: &str,
    param: Option<String>,
) -> LlmRemoteErrorMessage {
    let formatted = format_responses_error_event(code.as_deref(), message, param.as_deref());
    let full_message = format!("{prefix}: {formatted}");
    let mut result = if code.as_deref().is_some_and(|code| is_context_overflow_error_code(Some(code))) {
        LlmErrorMessage::new(LlmErrorKind::ContextOverflow, full_message, false)
    } else if code.as_deref().is_some_and(|code| is_open_ai_insufficient_quota_code(Some(code))) {
        LlmErrorMessage::new(LlmErrorKind::QuotaExhausted, full_message, false)
    } else if code.as_deref() == Some("rate_limit_exceeded")
        || read_embedded_status_code(message) == Some(429)
    {
        LlmErrorMessage::new(LlmErrorKind::RateLimit, full_message, true)
    } else {
        LlmErrorMessage::new(LlmErrorKind::Provider, full_message, true)
    };
    result.request_id = None;
    result.retry_after_ms = None;
    result.headers = None;
    result
}

fn parse_nested_gateway_stream_error(message: &str) -> Option<(Option<String>, String, Option<String>)> {
    let marker = "received error while streaming:";
    let index = message.find(marker)?;
    let json_text = message[index + marker.len()..].trim();
    if json_text.is_empty() {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(json_text).ok()?;
    let error = as_raw_object(&parsed)?;
    let nested_message = read_string_field(&error, "message")?;
    Some((
        read_nullable_string_field(&error, "code").flatten(),
        nested_message,
        read_nullable_string_field(&error, "param").flatten(),
    ))
}

fn malformed_stream_error_event(message: &str) -> LlmRemoteErrorMessage {
    if let Some((code, message, param)) = parse_nested_gateway_stream_error(message) {
        return error_from_open_ai_responses_event(
            "OpenAI Responses malformed stream error",
            code,
            &message,
            param,
        );
    }
    error_from_open_ai_responses_event(
        "OpenAI Responses malformed stream error",
        None,
        message,
        None,
    )
}

fn read_responses_failed_response_error(response: &Map<String, Value>) -> Option<(Option<String>, String)> {
    let error = read_object_field(response, "error")?;
    let code = read_nullable_string_field(&error, "code")
        .flatten()
        .or_else(|| Some("unknown".to_string()));
    let message = read_string_field(&error, "message").unwrap_or_else(|| "no message".to_string());
    Some((code, message))
}

fn format_responses_failed_response(response: &Map<String, Value>) -> String {
    if let Some((code, message)) = read_responses_failed_response_error(response) {
        return format_responses_error_event(code.as_deref(), &message, None);
    }
    let reason = read_object_field(response, "incomplete_details")
        .and_then(|details| read_string_field(&details, "reason"));
    reason.map_or_else(
        || "Unknown error (no error details in response)".to_string(),
        |reason| format!("incomplete: {reason}"),
    )
}

fn normalize_responses_finish(status: Option<&str>, incomplete_reason: Option<&str>) -> FinishInfo {
    match status {
        Some("completed") => FinishInfo {
            finish_reason: Some(FinishReason::Completed),
            raw_finish_reason: Some("completed".to_string()),
        },
        Some("incomplete") => match incomplete_reason {
            Some("max_output_tokens") => FinishInfo {
                finish_reason: Some(FinishReason::Truncated),
                raw_finish_reason: Some("max_output_tokens".to_string()),
            },
            Some("content_filter") => FinishInfo {
                finish_reason: Some(FinishReason::Filtered),
                raw_finish_reason: Some("content_filter".to_string()),
            },
            _ => FinishInfo {
                finish_reason: Some(FinishReason::Other),
                raw_finish_reason: Some(incomplete_reason.unwrap_or("incomplete").to_string()),
            },
        },
        Some("failed") => FinishInfo {
            finish_reason: Some(FinishReason::Other),
            raw_finish_reason: Some("failed".to_string()),
        },
        _ => NO_FINISH,
    }
}

pub fn default_open_ai_responses_tool(tool: &ToolDescription) -> Map<String, Value> {
    Map::from_iter([
        ("type".to_string(), Value::String("function".to_string())),
        ("name".to_string(), Value::String(tool.name.clone())),
        (
            "description".to_string(),
            Value::String(tool.description.clone()),
        ),
        ("parameters".to_string(), Value::Object(tool.parameters.clone())),
        ("strict".to_string(), Value::Bool(false)),
    ])
}

pub fn encode_open_ai_responses_cache_key(cache_key: &str) -> Map<String, Value> {
    Map::from_iter([(
        "prompt_cache_key".to_string(),
        Value::String(cache_key.to_string()),
    )])
}

pub fn encode_open_ai_responses_max_completion_tokens(cap: u64) -> Map<String, Value> {
    Map::from_iter([("max_output_tokens".to_string(), Value::from(cap))])
}

pub fn apply_open_ai_responses_response_format(
    kwargs: &Map<String, Value>,
    format: &ResponseFormat,
) -> Map<String, Value> {
    let mut output = kwargs.clone();
    let mut text = kwargs
        .get("text")
        .and_then(as_raw_object)
        .unwrap_or_default();
    text.insert(
        "format".to_string(),
        Value::Object(response_format_to_responses_text(format)),
    );
    output.insert("text".to_string(), Value::Object(text));
    output
}

pub fn normalize_open_ai_responses_reasoning(
    kwargs: &Map<String, Value>,
) -> Map<String, Value> {
    let Some(effort) = kwargs.get("reasoning_effort").and_then(Value::as_str) else {
        return kwargs.clone();
    };
    let mut output = kwargs.clone();
    output.remove("reasoning_effort");
    output.insert(
        "reasoning".to_string(),
        json!({ "effort": effort, "summary": "auto" }),
    );
    output.insert(
        "include".to_string(),
        json!(["reasoning.encrypted_content"]),
    );
    output
}

pub fn parse_open_ai_responses_usage(usage: Option<&Map<String, Value>>) -> Option<TokenUsage> {
    let usage = usage?;
    let input_tokens = read_number_field(usage, "input_tokens").unwrap_or(0);
    let output_tokens = read_number_field(usage, "output_tokens").unwrap_or(0);
    let cached = read_object_field(usage, "input_tokens_details")
        .and_then(|details| read_number_field(&details, "cached_tokens"))
        .unwrap_or(0);
    let split = classify_deep_seek_cache_split(
        usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64),
        usage.get("prompt_cache_miss_tokens").and_then(Value::as_u64),
        input_tokens,
        cached,
    );
    let (cached, miss) = match split {
        crate::human::llm::requester::bases::openai::usage::OpenAICacheSplit::Split { cached, miss } => (cached, miss),
        crate::human::llm::requester::bases::openai::usage::OpenAICacheSplit::Contradiction { cached, .. } => (cached, None),
        crate::human::llm::requester::bases::openai::usage::OpenAICacheSplit::Absent { cached } => (cached, None),
    };
    Some(TokenUsage {
        input_other: miss.unwrap_or_else(|| input_tokens.saturating_sub(cached)),
        output: output_tokens,
        input_cache_read: cached,
        input_cache_creation: 0,
        raw: Some(usage.clone()),
    })
}

fn extract_event_usage(event: &Map<String, Value>) -> Option<Map<String, Value>> {
    let type_ = read_string_field(event, "type");
    let response = read_object_field(event, "response");
    let response_usage = response.and_then(|response| read_object_field(&response, "usage"));
    if type_.as_deref() == Some("response.completed")
        || type_.as_deref() == Some("response.incomplete")
    {
        return response_usage;
    }
    response_usage.or_else(|| read_object_field(event, "usage"))
}

#[derive(Debug, Clone)]
pub struct OpenAIResponsesRequestParams {
    pub params: Map<String, Value>,
    pub headers: Option<HashMap<String, String>>,
}

pub fn lower_open_ai_responses_messages(
    input: &FormatRequestInput,
    options: &OpenAIResponsesLowerOptions,
) -> Vec<ResponsesInputItem> {
    input
        .messages
        .iter()
        .flat_map(|message| {
            lower_message(
                message,
                &OpenAIResponsesLowerOptions {
                    model_name: input.model.model.clone(),
                    extract_text: options.extract_text,
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct OpenAIResponsesRequestParts {
    pub input: Vec<ResponsesInputItem>,
    pub tools: Vec<Map<String, Value>>,
    pub kwargs: Map<String, Value>,
}

pub fn assemble_open_ai_responses_request(
    input: &FormatRequestInput,
    parts: &OpenAIResponsesRequestParts,
) -> Map<String, Value> {
    let mut params = Map::from_iter([
        ("model".to_string(), Value::String(input.model.model.clone())),
        ("input".to_string(), json!(parts.input)),
        ("store".to_string(), Value::Bool(false)),
        ("stream".to_string(), Value::Bool(true)),
    ]);
    if let Some(system_prompt) = input.system_prompt.as_ref().filter(|prompt| !prompt.is_empty()) {
        params.insert(
            "instructions".to_string(),
            Value::String(system_prompt.clone()),
        );
    }
    if !parts.tools.is_empty() {
        params.insert("tools".to_string(), json!(parts.tools));
    }
    params.extend(parts.kwargs.clone());
    params
}

pub fn encode_open_ai_responses_request(params: Map<String, Value>) -> OpenAIResponsesRequestParams {
    OpenAIResponsesRequestParams {
        params,
        headers: None,
    }
}

pub struct OpenAIResponsesProtocolFormat;

impl OpenAIResponsesProtocolFormat {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OpenAIResponsesProtocolFormat {
    fn default() -> Self {
        Self::new()
    }
}

impl ProtocolFormat<OpenAIResponsesRawChunk> for OpenAIResponsesProtocolFormat {
    fn create_stream_parser(
        &self,
        options: StreamParserOptions<OpenAIResponsesRawChunk>,
    ) -> StreamParser<OpenAIResponsesRawChunk> {
        let state = Arc::new(Mutex::new(FunctionCallState::default()));
        Arc::new(move |chunk, sink| {
            let event = chunk;
            let default_usage = parse_open_ai_responses_usage(extract_event_usage(&event).as_ref())
                .map(|usage| TokenUsagePatch {
                    input_other: Some(usage.input_other),
                    output: Some(usage.output),
                    input_cache_read: Some(usage.input_cache_read),
                    input_cache_creation: Some(usage.input_cache_creation),
                    raw: usage.raw,
                });
            let usage = match &options.resolve_usage {
                Some(resolve) => resolve(&event, default_usage),
                None => default_usage,
            };
            if let Some(usage) = usage
                && let Some(on_usage) = &sink.on_usage
            {
                on_usage(usage);
            }
            let Some(type_) = read_string_field(&event, "type") else {
                if !has_own(&event, "type")
                    && let Some(message) = read_string_field(&event, "message")
                {
                    if let Some(on_error) = &sink.on_error {
                        on_error(malformed_stream_error_event(&message));
                    }
                    return Ok(());
                }
                return fail_responses_decode("stream event.type", "must be a string.");
            };
            match type_.as_str() {
                "response.output_text.delta" => {
                    let text = require_string_field(&event, "delta", &type_)?;
                    (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Text {
                        text,
                        meta: None,
                    }));
                }
                "response.output_item.added" => {
                    let item = read_response_output_item(
                        event.get("item").unwrap_or(&Value::Null),
                        &format!("{type_}.item"),
                    )?;
                    let output_index = read_number_field(&event, "output_index");
                    if let ResponseOutputItemView::FunctionCall {
                        item_id,
                        call_id,
                        name,
                        arguments,
                    } = item
                    {
                        let stream_index = response_stream_index(item_id, output_index);
                        state.lock().set(stream_index.clone(), arguments.clone().unwrap_or_default());
                        (sink.on_delta)(StreamedMessagePart::ToolCall(ToolCall {
                            id: function_call_id(call_id),
                            name: require_function_call_name(name)?,
                            arguments,
                            extras: None,
                            raw_id: None,
                            _stream_index: stream_index,
                        }));
                    }
                }
                "response.output_item.done" => {
                    let item = read_response_output_item(
                        event.get("item").unwrap_or(&Value::Null),
                        &format!("{type_}.item"),
                    )?;
                    let output_index = read_number_field(&event, "output_index");
                    match item {
                        ResponseOutputItemView::Reasoning {
                            encrypted_content,
                            ..
                        } => {
                            (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                                think: String::new(),
                                encrypted: encrypted_content,
                                details_index: None,
                                hidden: None,
                                reasoning_key: None,
                            }));
                        }
                        ResponseOutputItemView::FunctionCall {
                            item_id,
                            arguments: Some(arguments),
                            ..
                        } => {
                            let stream_index = response_stream_index(item_id, output_index);
                            for part in final_arguments_suffix(&mut state.lock(), stream_index, &arguments, &type_)? {
                                (sink.on_delta)(part);
                            }
                        }
                        _ => {}
                    }
                }
                "response.function_call_arguments.delta" => {
                    let stream_index = response_stream_index(
                        read_string_field(&event, "item_id"),
                        read_number_field(&event, "output_index"),
                    );
                    let arguments_part = require_string_field(&event, "delta", &type_)?;
                    append_function_call_arguments(&mut state.lock(), stream_index.clone(), &arguments_part, &type_)?;
                    (sink.on_delta)(StreamedMessagePart::ToolCallPart(ToolCallPart {
                        arguments_part: Some(arguments_part),
                        index: stream_index,
                    }));
                }
                "response.function_call_arguments.done" => {
                    let arguments = require_string_field(&event, "arguments", &type_)?;
                    let stream_index = response_stream_index(
                        read_string_field(&event, "item_id"),
                        read_number_field(&event, "output_index"),
                    );
                    for part in final_arguments_suffix(&mut state.lock(), stream_index, &arguments, &type_)? {
                        (sink.on_delta)(part);
                    }
                }
                "response.reasoning_summary_part.added" => {
                    (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                        think: String::new(),
                        encrypted: None,
                        details_index: None,
                        hidden: None,
                        reasoning_key: None,
                    }));
                }
                "response.reasoning_summary_text.delta" => {
                    let think = require_string_field(&event, "delta", &type_)?;
                    (sink.on_delta)(StreamedMessagePart::Content(ContentPart::Think {
                        think,
                        encrypted: None,
                        details_index: None,
                        hidden: None,
                        reasoning_key: None,
                    }));
                }
                "response.completed" | "response.incomplete" => {
                    let response = read_object_field(&event, "response");
                    if let Some(message_id) = response
                        .as_ref()
                        .and_then(|response| read_string_field(response, "id"))
                        && let Some(on_message_id) = &sink.on_message_id
                    {
                        on_message_id(message_id);
                    }
                    let status = response
                        .as_ref()
                        .and_then(|response| read_string_field(response, "status"))
                        .unwrap_or_else(|| type_["response.".len()..].to_string());
                    let reason = response
                        .as_ref()
                        .and_then(|response| read_object_field(response, "incomplete_details"))
                        .and_then(|details| read_string_field(&details, "reason"));
                    (sink.on_finish)(normalize_responses_finish(Some(&status), reason.as_deref()));
                }
                "error" => {
                    let message = require_string_field(&event, "message", &type_)?;
                    if let Some(on_error) = &sink.on_error {
                        on_error(error_from_open_ai_responses_event(
                            "OpenAI Responses stream error",
                            read_nullable_string_field(&event, "code").flatten(),
                            &message,
                            read_nullable_string_field(&event, "param").flatten(),
                        ));
                    }
                }
                "response.failed" => {
                    let response = require_object_field(&event, "response", &type_)?;
                    if let Some((code, message)) = read_responses_failed_response_error(&response) {
                        if let Some(on_error) = &sink.on_error {
                            on_error(error_from_open_ai_responses_event(
                                "OpenAI Responses response.failed",
                                code,
                                &message,
                                None,
                            ));
                        }
                        return Ok(());
                    }
                    if let Some(on_error) = &sink.on_error {
                        on_error(LlmErrorMessage::new(
                            LlmErrorKind::Provider,
                            format!(
                                "OpenAI Responses response.failed: {}",
                                format_responses_failed_response(&response)
                            ),
                            true,
                        ));
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }
}

#[derive(Default)]
struct FunctionCallState {
    by_index: HashMap<crate::human::llm::message::StreamIndex, String>,
    unindexed: Option<String>,
}

impl FunctionCallState {
    fn has(&self, index: &Option<crate::human::llm::message::StreamIndex>) -> bool {
        match index {
            Some(index) => self.by_index.contains_key(index),
            None => self.unindexed.is_some(),
        }
    }

    fn get(&self, index: &Option<crate::human::llm::message::StreamIndex>) -> String {
        match index {
            Some(index) => self.by_index.get(index).cloned().unwrap_or_default(),
            None => self.unindexed.clone().unwrap_or_default(),
        }
    }

    fn set(&mut self, index: Option<crate::human::llm::message::StreamIndex>, arguments: String) {
        if let Some(index) = index {
            self.by_index.insert(index, arguments);
        } else {
            self.unindexed = Some(arguments);
        }
    }
}

fn append_function_call_arguments(
    state: &mut FunctionCallState,
    stream_index: Option<crate::human::llm::message::StreamIndex>,
    arguments_part: &str,
    context: &str,
) -> Result<(), WireAsyncError> {
    if !state.has(&stream_index) {
        return fail_responses_decode(
            context,
            &format!(
                "received function-call arguments for unknown stream index {}.",
                format_response_stream_index(stream_index.as_ref())
            ),
        );
    }
    let arguments = format!("{}{arguments_part}", state.get(&stream_index));
    state.set(stream_index, arguments);
    Ok(())
}

fn final_arguments_suffix(
    state: &mut FunctionCallState,
    stream_index: Option<crate::human::llm::message::StreamIndex>,
    final_arguments: &str,
    context: &str,
) -> Result<Vec<StreamedMessagePart>, WireAsyncError> {
    if !state.has(&stream_index) {
        return fail_responses_decode(
            context,
            &format!(
                "received final function-call arguments for unknown stream index {}.",
                format_response_stream_index(stream_index.as_ref())
            ),
        );
    }
    let accumulated = state.get(&stream_index);
    if final_arguments == accumulated {
        return Ok(Vec::new());
    }
    if !final_arguments.starts_with(&accumulated) {
        return Err(format!(
            "OpenAI Responses final function-call arguments for stream index {} do not match the streamed argument deltas.",
            format_response_stream_index(stream_index.as_ref())
        )
        .into());
    }
    let suffix = final_arguments[accumulated.len()..].to_string();
    state.set(stream_index.clone(), final_arguments.to_string());
    if suffix.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![StreamedMessagePart::ToolCallPart(ToolCallPart {
        arguments_part: Some(suffix),
        index: stream_index,
    })])
}
