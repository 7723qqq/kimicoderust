use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::human::llm::errors::{
    LlmErrorKind, LlmRemoteErrorMessage, headers_to_record, is_abort_error,
    is_transient_http_status, parse_retry_after_ms, sanitize_status_error_message,
    to_llm_error_message, to_llm_status_error_message, to_llm_stream_error_message,
    to_llm_transport_error_message,
};
use crate::human::llm::finish_reason::{FinishInfo, FinishReason, NO_FINISH};
use crate::human::llm::message::{Message, StreamedMessagePart, ToolDescription};
use crate::human::llm::model_family::reasoning_effort_for_model;
use crate::human::llm::protocol::format::{
    FormatRequestInput, ProtocolFormat, StreamParseSink, StreamParser, StreamParserOptions,
};
use crate::human::llm::protocol::patterns::TOOL_RESULT_TO_PLAIN_TEXT;
use crate::human::llm::protocol::rewrite::{Pattern, apply_patterns};
use crate::human::llm::requester::bases::openai::contract::*;
use crate::human::llm::requester::bases::openai::lower::{OpenAILowerOptions, lower_message};
use crate::human::llm::requester::bases::openai::patterns::EXTRACT_TOOL_MEDIA;
use crate::human::llm::requester::bases::openai::reasoning_key::{
    convert_reasoning_details, extract_reasoning_details, extract_reasoning_strings,
};
use crate::human::llm::requester::bases::openai::usage::parse_open_ai_usage;
use crate::human::llm::requester::requester::{ToolMessageConversion};
use crate::human::llm::response_format::ResponseFormat;
use crate::human::llm::usage::{TokenUsage, TokenUsagePatch};
use crate::human::log::log::LlmLogger;
use crate::wire::record::WireAsyncError;

pub fn response_format_to_open_ai(format: &ResponseFormat) -> Map<String, Value> {
    match format {
        ResponseFormat::JsonObject => Map::from_iter([(
            "type".to_string(),
            Value::String("json_object".to_string()),
        )]),
        ResponseFormat::JsonSchema { json_schema } => {
            let mut schema = Map::from_iter([
                ("name".to_string(), Value::String(json_schema.name.clone())),
                ("schema".to_string(), Value::Object(json_schema.schema.clone())),
            ]);
            if let Some(strict) = json_schema.strict {
                schema.insert("strict".to_string(), Value::Bool(strict));
            }
            if let Some(description) = &json_schema.description {
                schema.insert("description".to_string(), Value::String(description.clone()));
            }
            Map::from_iter([
                ("type".to_string(), Value::String("json_schema".to_string())),
                ("json_schema".to_string(), Value::Object(schema)),
            ])
        }
    }
}

pub fn encode_open_ai_cache_key(cache_key: &str) -> Map<String, Value> {
    Map::from_iter([(
        "prompt_cache_key".to_string(),
        Value::String(cache_key.to_string()),
    )])
}

pub fn encode_open_ai_think_history_kwargs(model: &str) -> Map<String, Value> {
    Map::from_iter([(
        "reasoning_effort".to_string(),
        Value::String(
            reasoning_effort_for_model(Some(model))
                .map(|effort| effort.history_default.clone())
                .unwrap_or_else(|| "high".to_string()),
        ),
    )])
}

const CHAT_COMPLETIONS_MAX_OUTPUT_TOKENS_CEILING: u64 = 128 * 1024;

fn uses_max_completion_tokens(model: &str) -> bool {
    let normalized = model.to_lowercase();
    regex::Regex::new(r"^o\d+(?:$|[-.])")
        .unwrap()
        .is_match(&normalized)
        || regex::Regex::new(r"^gpt-(?:[5-9]|\d{2,})(?:$|[-.])")
            .unwrap()
            .is_match(&normalized)
}

pub fn encode_open_ai_max_completion_tokens(model: &str, cap: u64) -> Map<String, Value> {
    let capped = cap.clamp(1, CHAT_COMPLETIONS_MAX_OUTPUT_TOKENS_CEILING);
    Map::from_iter([(
        if uses_max_completion_tokens(model) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        }
        .to_string(),
        Value::from(capped),
    )])
}

pub fn default_open_ai_tool(tool: &ToolDescription) -> Map<String, Value> {
    Map::from_iter([
        ("type".to_string(), Value::String("function".to_string())),
        (
            "function".to_string(),
            json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
            }),
        ),
    ])
}

fn normalize_finish_reason(raw: Option<&str>) -> FinishInfo {
    let Some(raw) = raw else {
        return NO_FINISH;
    };
    let finish_reason = match raw {
        "stop" => FinishReason::Completed,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "length" => FinishReason::Truncated,
        "content_filter" => FinishReason::Filtered,
        _ => FinishReason::Other,
    };
    FinishInfo {
        finish_reason: Some(finish_reason),
        raw_finish_reason: Some(raw.to_string()),
    }
}

pub use crate::human::llm::requester::bases::openai::usage::parse_open_ai_usage as parse_usage;

#[derive(Debug, Clone)]
pub struct OpenAIRequestParams {
    pub params: Map<String, Value>,
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone)]
pub struct OpenAILoweredMessage {
    pub source: Message,
    pub message: OpenAIWireMessage,
}

pub fn lower_open_ai_messages(
    input: &FormatRequestInput,
    options: &OpenAILowerOptions,
) -> Vec<OpenAILoweredMessage> {
    let conversion = options.tool_message_conversion;
    let normalized = match conversion {
        Some(ToolMessageConversion::ExtractText) => {
            apply_patterns(&input.messages, &[&TOOL_RESULT_TO_PLAIN_TEXT])
        }
        Some(ToolMessageConversion::KeepParts) => input.messages.clone(),
        None => apply_patterns(&input.messages, &[&EXTRACT_TOOL_MEDIA]),
    };
    normalized
        .into_iter()
        .flat_map(|message| {
            lower_message(&message, options)
                .into_iter()
                .map(move |wire| OpenAILoweredMessage {
                    source: message.clone(),
                    message: wire,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct OpenAIRequestParts {
    pub messages: Vec<OpenAIWireMessage>,
    pub tools: Vec<Map<String, Value>>,
    pub kwargs: Map<String, Value>,
}

pub fn strip_ignored_sampling_params(
    kwargs: &Map<String, Value>,
    ignored: &[String],
) -> Map<String, Value> {
    if ignored.is_empty() || ignored.iter().all(|name| !kwargs.contains_key(name)) {
        return kwargs.clone();
    }
    kwargs
        .iter()
        .filter(|(key, _)| !ignored.contains(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn assemble_open_ai_request(
    input: &FormatRequestInput,
    parts: &OpenAIRequestParts,
) -> Map<String, Value> {
    let mut params = Map::from_iter([
        ("model".to_string(), Value::String(input.model.model.clone())),
        ("messages".to_string(), json!(parts.messages)),
        ("stream".to_string(), Value::Bool(true)),
        ("stream_options".to_string(), json!({ "include_usage": true })),
    ]);
    if !parts.tools.is_empty() {
        params.insert("tools".to_string(), json!(parts.tools));
    }
    params.extend(parts.kwargs.clone());
    params
}

pub fn encode_open_ai_request(params: Map<String, Value>) -> OpenAIRequestParams {
    OpenAIRequestParams {
        params,
        headers: None,
    }
}

#[derive(Default)]
pub struct OpenAIStreamParserOptions {
    pub resolve_usage: Option<Arc<dyn Fn(&OpenAIRawChunk, Option<TokenUsagePatch>) -> Option<TokenUsagePatch> + Send + Sync>>,
    pub reasoning_key: Option<String>,
}

pub struct OpenAIProtocolFormat {
    log: Option<LlmLogger>,
}

impl OpenAIProtocolFormat {
    pub fn new(log: Option<LlmLogger>) -> Self {
        Self { log }
    }

    pub fn create_stream_parser(
        &self,
        options: OpenAIStreamParserOptions,
    ) -> StreamParser<OpenAIRawChunk> {
        let buffered_tool_calls = Arc::new(Mutex::new(HashMap::<crate::human::llm::message::StreamIndex, BufferedStreamToolCall>::new()));
        let seen_reasoning_content = Arc::new(AtomicBool::new(false));
        let log = self.log.clone();
        Arc::new(move |chunk, sink| {
            if let Some(id) = chunk.id.as_ref().filter(|id| !id.is_empty())
                && let Some(on_message_id) = &sink.on_message_id
            {
                on_message_id(id.clone());
            }
            let default_usage = parse_open_ai_usage(chunk.usage.as_ref(), log.as_ref())
                .map(|usage| TokenUsagePatch {
                    input_other: Some(usage.input_other),
                    output: Some(usage.output),
                    input_cache_read: Some(usage.input_cache_read),
                    input_cache_creation: Some(usage.input_cache_creation),
                    raw: usage.raw,
                });
            let usage = match &options.resolve_usage {
                Some(resolve) => resolve(&chunk, default_usage),
                None => default_usage,
            };
            if let Some(usage) = usage
                && let Some(on_usage) = &sink.on_usage
            {
                on_usage(usage);
            }
            let choice = chunk.choices.first();
            if let Some(finish_reason) = choice
                .and_then(|choice| choice.finish_reason.as_deref())
            {
                (sink.on_finish)(normalize_finish_reason(Some(finish_reason)));
            }
            let Some(delta) = choice.and_then(|choice| choice.delta.as_ref()) else {
                return;
            };
            let delta_value = serde_json::to_value(delta).unwrap();
            let reasoning_details = if options.reasoning_key.is_none() {
                extract_reasoning_details(&delta_value)
            } else {
                None
            };
            for (key, value) in extract_reasoning_strings(&delta_value) {
                if key == "reasoning_content" {
                    seen_reasoning_content.store(true, Ordering::Relaxed);
                }
                (sink.on_delta)(StreamedMessagePart::Content(crate::human::llm::message::ContentPart::Think {
                    think: value,
                    encrypted: None,
                    details_index: None,
                    hidden: None,
                    reasoning_key: Some(key),
                }));
            }
            if let Some(details) = reasoning_details {
                for part in convert_reasoning_details(
                    &details,
                    seen_reasoning_content.load(Ordering::Relaxed),
                ) {
                    (sink.on_delta)(part);
                }
            }
            if let Some(content) = delta.content.as_ref().filter(|content| !content.is_empty()) {
                (sink.on_delta)(StreamedMessagePart::Content(crate::human::llm::message::ContentPart::Text {
                    text: content.clone(),
                    meta: None,
                }));
            }
            for tool_call in delta.tool_calls.clone().unwrap_or_default() {
                for part in convert_stream_tool_call(tool_call, &buffered_tool_calls) {
                    (sink.on_delta)(part);
                }
            }
            Ok(())
        })
    }
}

impl ProtocolFormat<OpenAIRawChunk> for OpenAIProtocolFormat {
    fn create_stream_parser(&self, options: StreamParserOptions<OpenAIRawChunk>) -> StreamParser<OpenAIRawChunk> {
        self.create_stream_parser(OpenAIStreamParserOptions {
            resolve_usage: options.resolve_usage,
            reasoning_key: None,
        })
    }
}

#[derive(Default)]
struct BufferedStreamToolCall {
    id: Option<String>,
    arguments: String,
    emitted: bool,
}

fn convert_stream_tool_call(
    tool_call: OpenAIRawStreamToolCallDelta,
    buffered_tool_calls: &Mutex<HashMap<crate::human::llm::message::StreamIndex, BufferedStreamToolCall>>,
) -> Vec<StreamedMessagePart> {
    let Some(function) = tool_call.function else {
        return Vec::new();
    };
    let has_concrete_name = function.name.as_ref().is_some_and(|name| !name.is_empty());
    let has_arguments = function.arguments.as_ref().is_some_and(|arguments| !arguments.is_empty());
    let Some(stream_index) = tool_call.index else {
        if has_concrete_name {
            return vec![StreamedMessagePart::ToolCall(crate::human::llm::message::ToolCall {
                id: tool_call.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: function.name.unwrap(),
                arguments: function.arguments,
                extras: None,
                raw_id: None,
                _stream_index: None,
            })];
        }
        if has_arguments {
            return vec![StreamedMessagePart::ToolCallPart(crate::human::llm::message::ToolCallPart {
                arguments_part: function.arguments,
                index: None,
            })];
        }
        return Vec::new();
    };
    let mut map = buffered_tool_calls.lock();
    let mut buffered = map.get(&stream_index).cloned().unwrap_or_default();
    if tool_call.id.is_some() {
        buffered.id = tool_call.id;
    }
    if !buffered.emitted {
        if !has_concrete_name {
            if has_arguments {
                buffered.arguments.push_str(&function.arguments.unwrap());
            }
            map.insert(stream_index, buffered);
            return Vec::new();
        }
        buffered.emitted = true;
        let initial_arguments = if buffered.arguments.is_empty() {
            function.arguments
        } else {
            let mut arguments = buffered.arguments;
            arguments.push_str(function.arguments.as_deref().unwrap_or(""));
            Some(arguments)
        };
        buffered.arguments.clear();
        let id = buffered
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        map.insert(stream_index.clone(), buffered);
        return vec![StreamedMessagePart::ToolCall(crate::human::llm::message::ToolCall {
            id,
            name: function.name.unwrap(),
            arguments: initial_arguments,
            extras: None,
            raw_id: None,
            _stream_index: Some(stream_index),
        })];
    }
    if !has_arguments {
        return Vec::new();
    }
    vec![StreamedMessagePart::ToolCallPart(crate::human::llm::message::ToolCallPart {
        arguments_part: function.arguments,
        index: Some(stream_index),
    })]
}

pub fn is_open_ai_insufficient_quota_code(code: Option<&str>) -> bool {
    matches!(code, Some("insufficient_quota" | "exceeded_current_quota_error"))
}

pub fn is_context_overflow_error_code(code: Option<&str>) -> bool {
    code == Some("context_length_exceeded")
}

pub fn is_open_ai_rate_limit_error_code(code: Option<&str>) -> bool {
    matches!(code, Some("rate_limit_exceeded" | "rate_limit_reached_error" | "rate_limit_error"))
}

pub fn is_open_ai_overloaded_error_code(code: Option<&str>) -> bool {
    matches!(code, Some("overloaded_error" | "engine_overloaded_error"))
}

#[derive(Debug, Clone, Default)]
pub struct OpenAIErrorBody {
    pub code: Option<String>,
    pub message: Option<String>,
    pub type_: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenAIApiError {
    pub status: Option<u16>,
    pub request_id: Option<String>,
    pub headers: Option<reqwest::header::HeaderMap>,
    pub code: Option<String>,
    pub type_: Option<String>,
    pub error: Option<OpenAIErrorBody>,
    pub message: String,
}

#[derive(Debug)]
pub enum OpenAISdkError {
    ConnectionTimeout(String),
    Connection(String),
    Api(OpenAIApiError),
    Sdk(String),
}

impl std::fmt::Display for OpenAISdkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionTimeout(message) | Self::Connection(message) | Self::Sdk(message) => f.write_str(message),
            Self::Api(error) => f.write_str(&error.message),
        }
    }
}

impl std::error::Error for OpenAISdkError {}

pub fn convert_open_ai_error(
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
    let Some(sdk) = error.downcast_ref::<OpenAISdkError>() else {
        return to_llm_transport_error_message(&error.to_string());
    };
    match sdk {
        OpenAISdkError::ConnectionTimeout(message) => crate::human::llm::errors::LlmErrorMessage::new(
            LlmErrorKind::Timeout,
            message.clone(),
            true,
        ),
        OpenAISdkError::Connection(message) => crate::human::llm::errors::LlmErrorMessage::new(
            LlmErrorKind::Connection,
            message.clone(),
            true,
        ),
        OpenAISdkError::Sdk(message) => crate::human::llm::errors::LlmErrorMessage::new(
            LlmErrorKind::Provider,
            format!("Error: {message}"),
            true,
        ),
        OpenAISdkError::Api(api) => convert_api_error(api, source_stream),
    }
}

fn convert_api_error(api: &OpenAIApiError, source_stream: bool) -> LlmRemoteErrorMessage {
    let body = api.error.as_ref();
    let body_message = body.and_then(|body| body.message.clone()).unwrap_or_default();
    let code = api.code.clone().or_else(|| body.and_then(|body| body.code.clone()));
    let type_ = api.type_.clone().or_else(|| body.and_then(|body| body.type_.clone()));
    let status_code = if source_stream { None } else { api.status };
    let request_id = api.request_id.clone();
    let retry_after_ms = parse_retry_after_ms(api.headers.as_ref());
    let headers = headers_to_record(api.headers.as_ref());
    let insufficient = (api.status.is_none() || api.status == Some(429))
        && [api.code.as_deref(), api.type_.as_deref(), body.and_then(|body| body.code.as_deref()), body.and_then(|body| body.type_.as_deref())]
            .into_iter()
            .flatten()
            .any(|code| is_open_ai_insufficient_quota_code(Some(code)));
    let mut result = if insufficient {
        crate::human::llm::errors::LlmErrorMessage::new(
            LlmErrorKind::QuotaExhausted,
            if body_message.is_empty() {
                sanitize_status_error_message(&api.message)
            } else {
                body_message.clone()
            },
            false,
        )
    } else if is_open_ai_overloaded_error_code(code.as_deref())
        || is_open_ai_overloaded_error_code(type_.as_deref())
    {
        crate::human::llm::errors::LlmErrorMessage::new(
            LlmErrorKind::Overloaded,
            if body_message.is_empty() { api.message.clone() } else { body_message.clone() },
            true,
        )
    } else if is_context_overflow_error_code(code.as_deref())
        || is_context_overflow_error_code(type_.as_deref())
    {
        crate::human::llm::errors::LlmErrorMessage::new(
            LlmErrorKind::ContextOverflow,
            if body_message.is_empty() { api.message.clone() } else { body_message.clone() },
            false,
        )
    } else if let Some(status) = status_code {
        return to_llm_status_error_message(crate::human::llm::errors::LlmStatusErrorInput {
            status_code: Some(status),
            message: if body_message.is_empty() { api.message.clone() } else { body_message.clone() },
            request_id,
            retry_after_ms,
            headers,
            retryable: is_transient_http_status(Some(status)),
        });
    } else if api.error.is_some() {
        if is_open_ai_rate_limit_error_code(code.as_deref())
            || is_open_ai_rate_limit_error_code(type_.as_deref())
        {
            crate::human::llm::errors::LlmErrorMessage::new(
                LlmErrorKind::RateLimit,
                body_message.clone(),
                true,
            )
        } else {
            return to_llm_stream_error_message(
                &body_message,
                api.status.map(i64::from),
                request_id,
                retry_after_ms,
                headers,
                api.status.is_none() || is_transient_http_status(api.status),
            );
        }
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
