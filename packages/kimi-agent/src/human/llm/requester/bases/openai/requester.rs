use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::human::llm::errors::{
    LlmRemoteErrorMessage, headers_to_record, to_llm_syntax_error_message,
};
use crate::human::llm::message::{Message, StreamedMessagePart};
use crate::human::llm::model::{LlmModel, model_key};
use crate::human::llm::model_family::{
    FamilyMaxOutputDefaults, full_reasoning_echo_required, ignored_sampling_params_for_model,
    max_output_defaults_for_model, resolve_model_family,
};
use crate::human::llm::protocol::base::{ProtocolBase, ProtocolRequesterOptions, TraitContext};
use crate::human::llm::protocol::connection::{ProviderConnection, resolve_model_connection};
use crate::human::llm::protocol::format::{
    FormatRequestInput, StreamParseSink, remaining_context_tokens, resolve_max_completion_cap,
};
use crate::human::llm::protocol::thinking::apply_thinking;
use crate::human::llm::requester::bases::openai::capability::get_open_ai_legacy_model_capability;
use crate::human::llm::requester::bases::openai::contract::*;
use crate::human::llm::requester::bases::openai::format::*;
use crate::human::llm::requester::bases::openai::lower::OpenAILowerOptions;
use crate::human::llm::requester::bases::openai::reasoning_key::{
    DEFAULT_REASONING_KEY, ReasoningKeyDialect,
};
use crate::human::llm::requester::bases::openai::usage::parse_open_ai_usage;
use crate::human::llm::requester::bases::openai::openai_trait::OpenAITrait;
use crate::human::llm::requester::bases::tool_call_id::{
    normalize_tool_call_ids_for_provider, sanitize_tool_call_id,
};
use crate::human::llm::requester::requester::*;
use crate::human::llm::requester::timeout::{Dispatcher, get_llm_dispatcher};
use crate::human::llm::thinking::{
    ThinkingEffort, encode_reasoning_effort_fallback, resolve_thinking_effort_for_model,
};
use crate::human::log::log::LlmLogger;
use crate::wire::record::WireAsyncError;

static OPENAI_CHAT_TOOL_CALL_ID_POLICY: LazyLock<ToolCallIdPolicy> = LazyLock::new(|| {
    ToolCallIdPolicy {
        normalize: Arc::new(|id| sanitize_tool_call_id(&id, Some(64))),
        max_length: Some(64),
    }
});

use std::sync::LazyLock;

pub type OpenAIClientFactory = Arc<
    dyn Fn(LlmClientContext) -> Result<Arc<dyn OpenAIClient>, WireAsyncError> + Send + Sync,
>;

pub struct OpenAIRequesterOptions {
    pub connection: Option<ProviderConnection>,
    pub trait_: Option<OpenAITrait>,
    pub classify_error: Option<LlmErrorClassifier>,
    pub client_factory: Option<OpenAIClientFactory>,
    pub log: Option<LlmLogger>,
}

impl Default for OpenAIRequesterOptions {
    fn default() -> Self {
        Self {
            connection: None,
            trait_: None,
            classify_error: None,
            client_factory: None,
            log: None,
        }
    }
}

#[derive(Default)]
pub struct OpenAIRequestPreparationOptions {
    pub trait_: Option<OpenAITrait>,
    pub reasoning_key: Option<String>,
}

fn output_tier_for_effort(defaults: &FamilyMaxOutputDefaults, effort: &str) -> u64 {
    match effort {
        "off" => defaults.non_thinking as u64,
        "max" => defaults.thinking_max_effort as u64,
        _ => defaults.thinking as u64,
    }
}

fn family_default_cap(input: &FormatRequestInput, strict: bool) -> Option<u64> {
    let defaults = max_output_defaults_for_model(Some(&input.model.model))?;
    let effort = resolve_thinking_effort_for_model(
        input.thinking.as_ref().map(|thinking| thinking.effort.as_str()),
        None,
        &input.model,
        strict,
    );
    let tier = output_tier_for_effort(defaults, &effort);
    match remaining_context_tokens(input) {
        Some(remaining) if remaining > 0 => Some(tier.min(remaining as u64)),
        Some(_) => None,
        None => Some(tier),
    }
}

fn first_user_text(messages: &[Message]) -> String {
    for message in messages {
        let Message::User(message) = message else {
            continue;
        };
        let text = message
            .content
            .iter()
            .filter_map(|part| match part {
                crate::human::llm::message::ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

fn derive_determinism_seed(model: &str, messages: &[Message]) -> u32 {
    let source = format!("{model}\0{}", first_user_text(messages));
    let mut hash = 2166136261u32;
    for code_unit in source.encode_utf16() {
        hash ^= code_unit as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

pub fn prepare_open_ai_request(
    input: &FormatRequestInput,
    options: &OpenAIRequestPreparationOptions,
) -> Result<OpenAIRequestParams, WireAsyncError> {
    let trait_ = options.trait_.as_ref();
    let ctx = TraitContext {
        model: input.model.clone(),
    };
    let mut kwargs = Map::new();
    if let Some(cache_key) = &input.cache_key {
        kwargs = trait_
            .and_then(|trait_| trait_.encode_cache_key.as_ref())
            .and_then(|encode| encode(cache_key, &ctx))
            .unwrap_or_else(|| encode_open_ai_cache_key(cache_key));
    }
    let mut preserve_thinking = false;
    if let Some(thinking) = &input.thinking {
        let strict = trait_
            .and_then(|trait_| trait_.strict_thinking_validation)
            .unwrap_or(false);
        let fallback: crate::human::llm::protocol::thinking::ThinkingFallback = Arc::new(move |thinking: &crate::human::llm::thinking::ThinkingRequestOptions, ctx: &TraitContext| {
            encode_reasoning_effort_fallback(thinking, &ctx.model, strict)
                .unwrap_or_else(|error| std::panic::panic_any(error))
        });
        let applied = apply_thinking(
            kwargs,
            thinking,
            trait_.and_then(|trait_| trait_.thinking.as_ref()),
            &ctx,
            Some(&fallback),
        );
        kwargs = applied.kwargs;
        preserve_thinking = applied.preserve_thinking;
    }
    if trait_.and_then(|trait_| trait_.thinking.as_ref()).is_none()
        && input.thinking.as_ref().is_some_and(|thinking| thinking.effort != "off")
        && !kwargs.contains_key("reasoning_effort")
        && input.messages.iter().any(|message| {
            let content = match message {
                Message::System(message) => &message.content,
                Message::User(message) => &message.content,
                Message::Assistant(message) => &message.content,
                Message::Tool(message) => &message.content,
            };
            content
                .iter()
                .any(|part| matches!(part, crate::human::llm::message::ContentPart::Think { .. }))
        })
    {
        kwargs.extend(encode_open_ai_think_history_kwargs(&ctx.model.model));
    }
    if let Some(determinism) = &input.determinism {
        let seed = determinism
            .seed
            .map(u64::from)
            .unwrap_or_else(|| derive_determinism_seed(&ctx.model.model, &input.messages) as u64);
        kwargs.insert("seed".to_string(), Value::from(seed));
    }
    let echo_full_reasoning = full_reasoning_echo_required(
        Some(&ctx.model.model),
        !input.tools.is_empty(),
    );
    if let Some(format) = &input.response_format {
        kwargs.insert(
            "response_format".to_string(),
            Value::Object(response_format_to_open_ai(format)),
        );
    }
    let requested = input.max_completion_tokens;
    let sentinel = requested.is_some_and(|requested| requested == 0);
    let cap = if sentinel {
        None
    } else {
        resolve_max_completion_cap(input)
            .or_else(|| {
                trait_
                    .and_then(|trait_| trait_.completion_tokens_when_unset.as_ref())
                    .and_then(|resolve| resolve(&crate::human::llm::requester::bases::openai::openai_trait::CompletionTokensWhenUnsetInput {
                        model: input.model.clone(),
                        used_context_tokens: input.used_context_tokens,
                    }))
            })
            .or_else(|| {
                family_default_cap(
                    input,
                    trait_
                        .and_then(|trait_| trait_.strict_thinking_validation)
                        .unwrap_or(false),
                )
            })
    };
    if let Some(cap) = cap {
        kwargs.extend(
            trait_
                .and_then(|trait_| trait_.encode_max_completion_tokens.as_ref())
                .and_then(|encode| encode(cap, &ctx))
                .unwrap_or_else(|| encode_open_ai_max_completion_tokens(&ctx.model.model, cap)),
        );
    }
    if let Some(extra) = input
        .extra_params
        .as_ref()
        .and_then(|extra| extra.openai.as_ref())
    {
        let extra = serde_json::to_value(extra)?;
        if let Some(extra) = extra.as_object() {
            kwargs.extend(
                extra
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
        }
    }
    kwargs = strip_ignored_sampling_params(
        &kwargs,
        ignored_sampling_params_for_model(Some(&ctx.model.model)),
    );
    if !input.tools.is_empty()
        && resolve_model_family(&ctx.model.model).is_some_and(|family| family.id == "deepseek")
    {
        kwargs.remove("response_format");
    }
    let lowered = lower_open_ai_messages(
        input,
        &OpenAILowerOptions {
            reasoning_key: options
                .reasoning_key
                .clone()
                .unwrap_or_else(|| DEFAULT_REASONING_KEY.to_string()),
            preserve_thinking,
            echo_full_reasoning,
            tool_message_conversion: input
                .tool_message_conversion
                .or_else(|| trait_.and_then(|trait_| trait_.tool_message_conversion)),
        },
    );
    let mut converted = Vec::new();
    for lowered in lowered {
        let hooked = trait_
            .and_then(|trait_| trait_.convert_message.as_ref())
            .map(|convert| convert(&lowered.source, &lowered.message, &ctx));
        if matches!(hooked, Some(Some(None))) {
            continue;
        }
        let converted_message = hooked.flatten().flatten().unwrap_or(lowered.message);
        if let Message::System(source) = &lowered.source
            && source.content.is_empty()
            && source.tools.as_ref().is_some_and(|tools| !tools.is_empty())
            && matches!(&converted_message, OpenAIWireMessage::System { content: OpenAIContent::Text(text), extra, .. } if text.is_empty() && !extra.contains_key("tools"))
        {
            continue;
        }
        converted.push(converted_message);
    }
    let mut history = Vec::new();
    if let Some(system_prompt) = input.system_prompt.as_ref().filter(|prompt| !prompt.is_empty()) {
        history.push(OpenAIWireMessage::System {
            content: OpenAIContent::Text(system_prompt.clone()),
            extra: Map::new(),
        });
    }
    history.extend(converted);
    let merged = trait_
        .and_then(|trait_| trait_.merge_history.as_ref())
        .and_then(|merge| merge(&history, &ctx))
        .unwrap_or(history);
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            trait_
                .and_then(|trait_| trait_.convert_tool.as_ref())
                .and_then(|convert| convert(tool, &ctx))
                .unwrap_or_else(|| default_open_ai_tool(tool))
        })
        .collect();
    let params = assemble_open_ai_request(
        input,
        &OpenAIRequestParts {
            messages: merged,
            tools,
            kwargs,
        },
    );
    let final_params = trait_
        .and_then(|trait_| trait_.build_params.as_ref())
        .and_then(|build| build(params.clone(), &ctx))
        .unwrap_or(params);
    Ok(encode_open_ai_request(final_params))
}

pub type OpenAIChunkStream = Pin<Box<dyn Stream<Item = Result<OpenAIRawChunk, WireAsyncError>> + Send>>;

pub struct OpenAIStreamResponse {
    pub headers: reqwest::header::HeaderMap,
    pub chunks: OpenAIChunkStream,
}

#[async_trait]
pub trait OpenAIClient: Send + Sync {
    async fn create_stream(
        &self,
        params: Map<String, Value>,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<OpenAIStreamResponse, WireAsyncError>;
}

struct ReqwestOpenAIClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    headers: HashMap<String, String>,
}

#[async_trait]
impl OpenAIClient for ReqwestOpenAIClient {
    async fn create_stream(
        &self,
        params: Map<String, Value>,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<OpenAIStreamResponse, WireAsyncError> {
        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.api_key)
            .json(&params);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|error| {
            Box::new(if error.is_timeout() {
                OpenAISdkError::ConnectionTimeout(error.to_string())
            } else {
                OpenAISdkError::Connection(error.to_string())
            }) as WireAsyncError
        })?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
            let error_body = parsed.get("error").and_then(|body| {
                Some(OpenAIErrorBody {
                    code: body.get("code").and_then(Value::as_str).map(str::to_string),
                    message: body.get("message").and_then(Value::as_str).map(str::to_string),
                    type_: body.get("type").and_then(Value::as_str).map(str::to_string),
                })
            });
            return Err(Box::new(OpenAISdkError::Api(OpenAIApiError {
                status: Some(status.as_u16()),
                request_id: headers
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                headers: Some(headers),
                code: parsed.get("code").and_then(Value::as_str).map(str::to_string),
                type_: parsed.get("type").and_then(Value::as_str).map(str::to_string),
                error: error_body,
                message: body,
            })));
        }
        if signal.is_cancelled() {
            return Err("The operation was aborted.".into());
        }
        let chunks = parse_sse_stream(response);
        Ok(OpenAIStreamResponse { headers, chunks })
    }
}

fn parse_sse_stream(response: reqwest::Response) -> OpenAIChunkStream {
    Box::pin(futures_util::stream::try_unfold(
        (response.bytes_stream(), String::new(), false),
        |(mut stream, mut pending, mut done)| async move {
            loop {
                if done {
                    return Ok(None);
                }
                if let Some(event) = take_sse_event(&mut pending) {
                    if event.trim() == "[DONE]" {
                        done = true;
                        return Ok(None);
                    }
                    if event.trim().is_empty() {
                        continue;
                    }
                    let chunk = serde_json::from_str::<OpenAIRawChunk>(&event)?;
                    return Ok(Some((chunk, (stream, pending, done))));
                }
                match stream.next().await {
                    Some(chunk) => {
                        let chunk = chunk.map_err(|error| {
                            Box::new(OpenAISdkError::Connection(error.to_string())) as WireAsyncError
                        })?;
                        pending.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
                    }
                    None => {
                        done = true;
                        if pending.trim().is_empty() {
                            return Ok(None);
                        }
                        let event = std::mem::take(&mut pending);
                        if event.trim() == "[DONE]" {
                            return Ok(None);
                        }
                        let chunk = serde_json::from_str::<OpenAIRawChunk>(&event)?;
                        return Ok(Some((chunk, (stream, pending, done))));
                    }
                }
            }
        }),
    )
}

fn take_sse_event(pending: &mut String) -> Option<String> {
    let boundary = pending.find("\n\n")?;
    let raw = pending[..boundary].to_string();
    pending.drain(..boundary + 2);
    let data = raw
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(|data| data.trim_start()))
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() { Some(String::new()) } else { Some(data) }
}

pub struct OpenAIRequester {
    connection: Option<ProviderConnection>,
    trait_: Option<OpenAITrait>,
    classify_error: Option<LlmErrorClassifier>,
    format: OpenAIProtocolFormat,
    resolve_client: OpenAIClientFactory,
    reasoning_by_model: Mutex<HashMap<String, Arc<ReasoningKeyDialect>>>,
    log: Option<LlmLogger>,
}

impl OpenAIRequester {
    fn reasoning_for(&self, ctx: &TraitContext) -> Arc<ReasoningKeyDialect> {
        let key = model_key(&ctx.model);
        self.reasoning_by_model
            .lock()
            .entry(key)
            .or_insert_with(|| {
                Arc::new(ReasoningKeyDialect::new(
                    self.trait_
                        .as_ref()
                        .and_then(|trait_| trait_.reasoning_key.clone()),
                ))
            })
            .clone()
    }

    async fn execute(
        &self,
        request: OpenAIRequestParams,
        ctx: TraitContext,
        reasoning: &ReasoningKeyDialect,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let headers = merge_request_headers(
            merge_request_headers(
                self.connection
                    .as_ref()
                    .and_then(|connection| connection.default_headers.as_ref())
                    .and_then(|headers| headers(crate::human::llm::protocol::connection::ConnectionContext { model: ctx.model.clone() }))
                    .as_ref(),
                ctx.model.connection.default_headers.as_ref(),
            )
            .as_ref(),
            request.headers.as_ref(),
        );
        let client = (self.resolve_client)(LlmClientContext {
            model: ctx.model.clone(),
            headers,
        })?;
        (control.on_event)(LlmRequestEvent::Sent);
        let response = client.create_stream(request.params, control.signal.clone()).await?;
        (control.on_event)(LlmRequestEvent::StreamingHeaders {
            headers: headers_to_record(Some(&response.headers)).unwrap_or_default(),
        });
        let parse = self.format.create_stream_parser(OpenAIStreamParserOptions {
            reasoning_key: self
                .trait_
                .as_ref()
                .and_then(|trait_| trait_.reasoning_key.clone()),
            resolve_usage: self.trait_.as_ref().and_then(|trait_| {
                let extract = trait_.extract_usage.clone()?;
                let log = self.log.clone();
                Some(Arc::new(move |chunk: &OpenAIRawChunk, default_usage: Option<crate::human::llm::usage::TokenUsagePatch>| {
                    extract(chunk).map_or(default_usage, |usage| {
                        parse_open_ai_usage(Some(&usage), log.as_ref()).map(|usage| {
                            crate::human::llm::usage::TokenUsagePatch {
                                input_other: Some(usage.input_other),
                                output: Some(usage.output),
                                input_cache_read: Some(usage.input_cache_read),
                                input_cache_creation: Some(usage.input_cache_creation),
                                raw: usage.raw,
                            }
                        })
                    })
                }) as _)
            }),
        });
        let message_id = std::sync::Arc::new(parking_lot::Mutex::new(None::<String>));
        let mut chunks = response.chunks;
        while let Some(chunk) = chunks.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    (control.on_event)(LlmRequestEvent::FailedRemote {
                        error: convert_open_ai_error(&error, self.classify_error.as_ref(), true),
                        raw_error: None,
                    });
                    return Ok(());
                }
            };
            if let Some(delta) = chunk.choices.first().and_then(|choice| choice.delta.as_ref()) {
                reasoning.observe(&serde_json::to_value(delta)?);
            }
            let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let parse_result = parse(chunk, &StreamParseSink {
                on_delta: {
                    let on_event = control.on_event.clone();
                    Arc::new(move |part| on_event(LlmRequestEvent::StreamingPart { part }))
                },
                on_finish: {
                    let on_event = control.on_event.clone();
                    Arc::new(move |finish| on_event(LlmRequestEvent::StreamingFinish { finish }))
                },
                on_message_id: Some({
                    let on_event = control.on_event.clone();
                    let message_id = message_id.clone();
                    Arc::new(move |id| {
                        {
                            let mut guard = message_id.lock();
                            if guard.as_ref() == Some(&id) {
                                return;
                            }
                            *guard = Some(id.clone());
                        }
                        on_event(LlmRequestEvent::StreamingMessageId { message_id: id });
                    })
                }),
                on_usage: Some({
                    let on_event = control.on_event.clone();
                    Arc::new(move |usage| on_event(LlmRequestEvent::StreamingUsage { usage }))
                }),
                on_error: Some({
                    let on_event = control.on_event.clone();
                    let failed = failed.clone();
                    Arc::new(move |error| {
                        failed.store(true, std::sync::atomic::Ordering::SeqCst);
                        on_event(LlmRequestEvent::FailedRemote {
                            error,
                            raw_error: None,
                        });
                    })
                }),
            });
            if let Err(error) = parse_result {
                (control.on_event)(LlmRequestEvent::FailedRemote {
                    error: convert_open_ai_error(&error, self.classify_error.as_ref(), true),
                    raw_error: None,
                });
                return Ok(());
            }
            if failed.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(());
            }
        }
        (control.on_event)(LlmRequestEvent::Done);
        Ok(())
    }
}

#[async_trait]
impl LlmRequester for OpenAIRequester {
    async fn generate(
        &self,
        config: LlmRequestConfig,
        content: LlmRequestContent,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let model = resolve_model_connection(&config.model, self.connection.as_ref());
        let tools = config.tools.clone().unwrap_or_default();
        let ctx = TraitContext { model: model.clone() };
        let reasoning = self.reasoning_for(&ctx);
        let request = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let policy = self
                .trait_
                .as_ref()
                .and_then(|trait_| trait_.tool_call_id_policy.clone())
                .unwrap_or_else(|| OPENAI_CHAT_TOOL_CALL_ID_POLICY.clone());
            let messages = normalize_tool_call_ids_for_provider(&content.messages, &policy)?;
            prepare_open_ai_request(
                &FormatRequestInput {
                    model,
                    system_prompt: config.system_prompt.clone(),
                    cache_key: config.cache_key.clone(),
                    thinking: config.thinking.clone(),
                    response_format: config.response_format.clone(),
                    determinism: config.determinism.clone(),
                    extra_params: config.extra_params.clone(),
                    tool_message_conversion: config.tool_message_conversion,
                    max_completion_tokens: config.max_completion_tokens,
                    max_context_tokens: config.max_context_tokens,
                    messages: messages.clone(),
                    tools,
                    used_context_tokens: content.used_context_tokens,
                },
                &OpenAIRequestPreparationOptions {
                    trait_: self.trait_.clone(),
                    reasoning_key: Some(reasoning.outbound_key(&content.messages)),
                },
            )
        }))
        .unwrap_or_else(|payload| {
            let error: WireAsyncError = match payload.downcast::<WireAsyncError>() {
                Ok(error) => *error,
                Err(payload) => {
                    if let Some(message) = payload.downcast_ref::<String>() {
                        message.clone().into()
                    } else if let Some(message) = payload.downcast_ref::<&str>() {
                        (*message).into()
                    } else {
                        std::panic::resume_unwind(payload)
                    }
                }
            };
            Err(error)
        });
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                (control.on_event)(LlmRequestEvent::FailedSyntax {
                    error: to_llm_syntax_error_message(&error),
                });
                return Ok(());
            }
        };
        if let Err(error) = self.execute(request, ctx, &reasoning, control.clone()).await {
            (control.on_event)(LlmRequestEvent::FailedRemote {
                error: convert_open_ai_error(&error, self.classify_error.as_ref(), false),
                raw_error: None,
            });
        }
        Ok(())
    }
}

pub fn create_open_ai_requester(options: OpenAIRequesterOptions) -> Arc<OpenAIRequester> {
    let resolve_client = options.client_factory.unwrap_or_else(|| {
        Arc::new(|request: LlmClientContext| {
            let dispatcher = get_llm_dispatcher(&std::env::vars().collect())?;
            let (headers_timeout, body_timeout) = match dispatcher {
                Some(Dispatcher::Agent(agent)) => (agent.headers_timeout_ms, agent.body_timeout_ms),
                Some(Dispatcher::EnvHttpProxyAgent(agent)) => {
                    (agent.headers_timeout_ms, agent.body_timeout_ms)
                }
                None => (None, None),
            };
            let mut builder = reqwest::Client::builder();
            if let Some(timeout) = body_timeout.or(headers_timeout) {
                builder = builder.timeout(std::time::Duration::from_millis(timeout));
            }
            Ok(Arc::new(ReqwestOpenAIClient {
                client: builder.build()?,
                base_url: request
                    .model
                    .connection
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                api_key: request
                    .model
                    .connection
                    .api_key
                    .clone()
                    .unwrap_or_else(|| "unused".to_string()),
                headers: request.headers.unwrap_or_default(),
            }) as Arc<dyn OpenAIClient>)
        })
    });
    Arc::new(OpenAIRequester {
        connection: options.connection,
        trait_: options.trait_.clone(),
        classify_error: options.classify_error,
        format: OpenAIProtocolFormat::new(options.log.clone()),
        resolve_client,
        reasoning_by_model: Mutex::new(HashMap::new()),
        log: options.log,
    })
}

pub struct OpenAIBase;

impl ProtocolBase<OpenAITrait> for OpenAIBase {
    fn capability(&self, model_name: &str) -> Option<crate::human::llm::capability::ModelCapability> {
        get_open_ai_legacy_model_capability(model_name)
    }

    fn create_requester(
        &self,
        options: ProtocolRequesterOptions<OpenAITrait>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        Ok(create_open_ai_requester(OpenAIRequesterOptions {
            connection: options.connection,
            trait_: options.trait_,
            classify_error: options.classify_error,
            client_factory: None,
            log: options.log,
        }))
    }
}
