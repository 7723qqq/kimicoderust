use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde_json::{Map, Value, json};

use crate::human::llm::protocol::format::ProtocolFormat;
use crate::human::llm::errors::{headers_to_record, to_llm_syntax_error_message};
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::base::{ProtocolBase, ProtocolRequesterOptions, TraitContext};
use crate::human::llm::protocol::connection::{ProviderConnection, resolve_model_connection};
use crate::human::llm::protocol::format::{
    FormatRequestInput, StreamParseSink, StreamParserOptions, resolve_max_completion_cap,
};
use crate::human::llm::protocol::thinking::apply_thinking;
use crate::human::llm::requester::bases::openai::format::{
    OpenAIApiError, OpenAIErrorBody, OpenAISdkError, convert_open_ai_error,
};
use crate::human::llm::requester::bases::openai::requester::{
    OpenAIClientFactory, OpenAIStreamResponse,
};
use crate::human::llm::requester::bases::openai_responses::capability::get_open_ai_responses_model_capability;
use crate::human::llm::requester::bases::openai_responses::contract::*;
use crate::human::llm::requester::bases::openai_responses::format::*;
use crate::human::llm::requester::bases::openai_responses::lower::OpenAIResponsesLowerOptions;
use crate::human::llm::requester::bases::openai_responses::openai_responses_trait::OpenAIResponsesTrait;
use crate::human::llm::requester::bases::tool_call_id::{
    normalize_tool_call_ids_for_provider, sanitize_open_ai_responses_call_id,
};
use crate::human::llm::requester::requester::*;
use crate::human::llm::requester::timeout::{Dispatcher, get_llm_dispatcher};
use crate::human::llm::thinking::encode_reasoning_effort_fallback;
use crate::human::log::log::LlmLogger;
use crate::wire::record::WireAsyncError;

static OPENAI_RESPONSES_TOOL_CALL_ID_POLICY: LazyLock<ToolCallIdPolicy> = LazyLock::new(|| {
    ToolCallIdPolicy {
        normalize: Arc::new(|id| sanitize_open_ai_responses_call_id(&id, Some(64))),
        max_length: Some(64),
    }
});

pub type OpenAIResponsesClientFactory = Arc<
    dyn Fn(LlmClientContext) -> Result<Arc<dyn OpenAIResponsesClient>, WireAsyncError>
        + Send
        + Sync,
>;

pub struct OpenAIResponsesRequesterOptions {
    pub connection: Option<ProviderConnection>,
    pub trait_: Option<OpenAIResponsesTrait>,
    pub classify_error: Option<LlmErrorClassifier>,
    pub client_factory: Option<OpenAIResponsesClientFactory>,
    pub log: Option<LlmLogger>,
}

impl Default for OpenAIResponsesRequesterOptions {
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
pub struct OpenAIResponsesRequestPreparationOptions {
    pub trait_: Option<OpenAIResponsesTrait>,
}

pub fn prepare_open_ai_responses_request(
    input: &FormatRequestInput,
    options: &OpenAIResponsesRequestPreparationOptions,
) -> Result<OpenAIResponsesRequestParams, WireAsyncError> {
    let trait_ = options.trait_.as_ref();
    let ctx = TraitContext {
        model: input.model.clone(),
    };
    let mut kwargs = Map::new();
    if let Some(cache_key) = &input.cache_key {
        kwargs = trait_
            .and_then(|trait_| trait_.encode_cache_key.as_ref())
            .and_then(|encode| encode(cache_key, &ctx))
            .unwrap_or_else(|| encode_open_ai_responses_cache_key(cache_key));
    }
    if let Some(thinking) = &input.thinking {
        let strict = trait_
            .and_then(|trait_| trait_.strict_thinking_validation)
            .unwrap_or(false);
        let fallback: crate::human::llm::protocol::thinking::ThinkingFallback = Arc::new(move |thinking: &crate::human::llm::thinking::ThinkingRequestOptions, ctx: &TraitContext| {
            encode_reasoning_effort_fallback(thinking, &ctx.model, strict)
                .unwrap_or_else(|error| std::panic::panic_any(error))
        });
        kwargs = apply_thinking(
            kwargs,
            thinking,
            trait_.and_then(|trait_| trait_.thinking.as_ref()),
            &ctx,
            Some(&fallback),
        )
        .kwargs;
    }
    let requested = input.max_completion_tokens;
    let cap = if requested.is_some_and(|requested| requested == 0) {
        None
    } else {
        resolve_max_completion_cap(input).or_else(|| {
            trait_
                .and_then(|trait_| trait_.completion_tokens_when_unset.as_ref())
                .and_then(|resolve| {
                    resolve(&crate::human::llm::requester::bases::openai_responses::openai_responses_trait::ResponsesCompletionTokensWhenUnsetInput {
                        model: input.model.clone(),
                        used_context_tokens: input.used_context_tokens,
                    })
                })
        })
    };
    if let Some(cap) = cap {
        kwargs.extend(
            trait_
                .and_then(|trait_| trait_.encode_max_completion_tokens.as_ref())
                .and_then(|encode| encode(cap, &ctx))
                .unwrap_or_else(|| encode_open_ai_responses_max_completion_tokens(cap)),
        );
    }
    if let Some(format) = &input.response_format {
        kwargs = apply_open_ai_responses_response_format(&kwargs, format);
    }
    kwargs = normalize_open_ai_responses_reasoning(&kwargs);
    if let Some(extra) = input
        .extra_params
        .as_ref()
        .and_then(|extra| extra.responses.as_ref())
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
    let lowered = lower_open_ai_responses_messages(
        input,
        &OpenAIResponsesLowerOptions {
            model_name: input.model.model.clone(),
            extract_text: input
                .tool_message_conversion
                .or_else(|| trait_.and_then(|trait_| trait_.tool_message_conversion))
                == Some(crate::human::llm::requester::requester::ToolMessageConversion::ExtractText),
        },
    );
    let merged = trait_
        .and_then(|trait_| trait_.merge_history.as_ref())
        .and_then(|merge| merge(&lowered, &ctx))
        .unwrap_or(lowered);
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            trait_
                .and_then(|trait_| trait_.convert_tool.as_ref())
                .and_then(|convert| convert(tool, &ctx))
                .unwrap_or_else(|| default_open_ai_responses_tool(tool))
        })
        .collect();
    let params = assemble_open_ai_responses_request(
        input,
        &OpenAIResponsesRequestParts {
            input: merged,
            tools,
            kwargs,
        },
    );
    let final_params = trait_
        .and_then(|trait_| trait_.build_params.as_ref())
        .and_then(|build| build(params.clone(), &ctx))
        .unwrap_or(params);
    Ok(encode_open_ai_responses_request(final_params))
}

pub type OpenAIResponsesChunkStream =
    Pin<Box<dyn Stream<Item = Result<OpenAIResponsesRawChunk, WireAsyncError>> + Send>>;

pub struct OpenAIResponsesStreamResponse {
    pub headers: reqwest::header::HeaderMap,
    pub chunks: OpenAIResponsesChunkStream,
}

#[async_trait]
pub trait OpenAIResponsesClient: Send + Sync {
    async fn create_stream(
        &self,
        params: Map<String, Value>,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<OpenAIResponsesStreamResponse, WireAsyncError>;
}

struct ReqwestOpenAIResponsesClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    headers: HashMap<String, String>,
}

#[async_trait]
impl OpenAIResponsesClient for ReqwestOpenAIResponsesClient {
    async fn create_stream(
        &self,
        params: Map<String, Value>,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<OpenAIResponsesStreamResponse, WireAsyncError> {
        let mut request = self
            .client
            .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
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
        Ok(OpenAIResponsesStreamResponse {
            headers,
            chunks: parse_responses_sse_stream(response),
        })
    }
}

fn parse_responses_sse_stream(response: reqwest::Response) -> OpenAIResponsesChunkStream {
    Box::pin(futures_util::stream::try_unfold(
        (response.bytes_stream(), String::new(), false),
        |(mut stream, mut pending, mut done)| async move {
            loop {
                if done {
                    return Ok(None);
                }
                if let Some(event) = take_responses_sse_event(&mut pending) {
                    if event.trim() == "[DONE]" {
                        done = true;
                        return Ok(None);
                    }
                    if event.trim().is_empty() {
                        continue;
                    }
                    let chunk = serde_json::from_str::<OpenAIResponsesRawChunk>(&event)?;
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
                        let chunk = serde_json::from_str::<OpenAIResponsesRawChunk>(&event)?;
                        return Ok(Some((chunk, (stream, pending, done))));
                    }
                }
            }
        }),
    )
}

fn take_responses_sse_event(pending: &mut String) -> Option<String> {
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

pub struct OpenAIResponsesRequester {
    connection: Option<ProviderConnection>,
    trait_: Option<OpenAIResponsesTrait>,
    classify_error: Option<LlmErrorClassifier>,
    format: OpenAIResponsesProtocolFormat,
    resolve_client: OpenAIResponsesClientFactory,
    log: Option<LlmLogger>,
}

impl OpenAIResponsesRequester {
    async fn execute(
        &self,
        request: OpenAIResponsesRequestParams,
        ctx: TraitContext,
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
        let parse = self.format.create_stream_parser(StreamParserOptions {
            resolve_usage: self.trait_.as_ref().and_then(|trait_| {
                let extract = trait_.extract_usage.clone()?;
                Some(Arc::new(move |chunk: &OpenAIResponsesRawChunk, default_usage: Option<crate::human::llm::usage::TokenUsagePatch>| {
                    extract(chunk).map_or(default_usage, |usage| {
                        let value = serde_json::to_value(&usage).ok()?;
                        let object = value.as_object()?;
                        parse_open_ai_responses_usage(Some(object)).map(|usage| {
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
impl LlmRequester for OpenAIResponsesRequester {
    async fn generate(
        &self,
        config: LlmRequestConfig,
        content: LlmRequestContent,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let model = resolve_model_connection(&config.model, self.connection.as_ref());
        let tools = config.tools.clone().unwrap_or_default();
        let ctx = TraitContext { model: model.clone() };
        let request = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let policy = self
                .trait_
                .as_ref()
                .and_then(|trait_| trait_.tool_call_id_policy.clone())
                .unwrap_or_else(|| OPENAI_RESPONSES_TOOL_CALL_ID_POLICY.clone());
            let messages = normalize_tool_call_ids_for_provider(&content.messages, &policy)?;
            prepare_open_ai_responses_request(
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
                    messages,
                    tools,
                    used_context_tokens: content.used_context_tokens,
                },
                &OpenAIResponsesRequestPreparationOptions {
                    trait_: self.trait_.clone(),
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
        if let Err(error) = self.execute(request, ctx, control.clone()).await {
            (control.on_event)(LlmRequestEvent::FailedRemote {
                error: convert_open_ai_error(&error, self.classify_error.as_ref(), false),
                raw_error: Some(Value::String(error.to_string())),
            });
        }
        Ok(())
    }
}

pub fn create_open_ai_responses_requester(
    options: OpenAIResponsesRequesterOptions,
) -> Arc<OpenAIResponsesRequester> {
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
            Ok(Arc::new(ReqwestOpenAIResponsesClient {
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
            }) as Arc<dyn OpenAIResponsesClient>)
        })
    });
    Arc::new(OpenAIResponsesRequester {
        connection: options.connection,
        trait_: options.trait_,
        classify_error: options.classify_error,
        format: OpenAIResponsesProtocolFormat::new(),
        resolve_client,
        log: options.log,
    })
}

pub struct OpenAIResponsesBase;

impl ProtocolBase<OpenAIResponsesTrait> for OpenAIResponsesBase {
    fn capability(&self, model_name: &str) -> Option<crate::human::llm::capability::ModelCapability> {
        get_open_ai_responses_model_capability(model_name)
    }

    fn create_requester(
        &self,
        options: ProtocolRequesterOptions<OpenAIResponsesTrait>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        Ok(create_open_ai_responses_requester(OpenAIResponsesRequesterOptions {
            connection: options.connection,
            trait_: options.trait_,
            classify_error: options.classify_error,
            client_factory: None,
            log: options.log,
        }))
    }
}
