use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde_json::{Map, Value, json};

use crate::human::llm::errors::headers_to_record;
use crate::human::llm::media::image_formats::provider_image_policy;
use crate::human::llm::model::LlmModel;
use crate::human::llm::protocol::base::{ProtocolBase, ProtocolRequesterOptions, TraitContext};
use crate::human::llm::protocol::connection::{
    ConnectionContext, ProviderConnection, resolve_model_connection,
};
use crate::human::llm::protocol::format::{
    FormatRequestInput, ProtocolFormat, StreamParseSink, StreamParserOptions,
    resolve_max_completion_cap,
};
use crate::human::llm::protocol::thinking::apply_thinking;
use crate::human::llm::requester::bases::anthropic::capability::get_anthropic_model_capability;
use crate::human::llm::requester::bases::anthropic::contract::AnthropicRawStreamEvent;
use crate::human::llm::requester::bases::anthropic::format::*;
use crate::human::llm::requester::bases::anthropic::lower::is_anthropic_wire_message_empty;
use crate::human::llm::requester::bases::anthropic::profile::{
    INTERLEAVED_THINKING_BETA, encode_thinking, resolve_default_max_tokens_with_override,
};
use crate::human::llm::requester::bases::anthropic::r#trait::AnthropicTrait;
use crate::human::llm::requester::bases::tool_call_id::{
    normalize_tool_call_ids_for_provider, sanitize_tool_call_id,
};
use crate::human::llm::requester::requester::*;
use crate::human::llm::requester::timeout::{Dispatcher, get_llm_dispatcher};
use crate::human::llm::syntax_errors::{SyntaxRequestFormatError, to_llm_syntax_error_message};
use crate::human::llm::thinking::{ThinkingConfigError, ThinkingConfigErrorCode};
use crate::human::log::log::LlmLogger;
use crate::wire::record::WireAsyncError;

static ANTHROPIC_TOOL_CALL_ID_POLICY: LazyLock<ToolCallIdPolicy> = LazyLock::new(|| {
    ToolCallIdPolicy {
        normalize: Arc::new(|id| sanitize_tool_call_id(&id, Some(64))),
        max_length: Some(64),
    }
});

const MIN_ANTHROPIC_THINKING_BUDGET: u64 = 1024;

pub type AnthropicClientFactory = Arc<
    dyn Fn(LlmClientContext) -> Result<Arc<dyn AnthropicClient>, WireAsyncError> + Send + Sync,
>;

#[derive(Clone, Default)]
pub struct AnthropicRequesterOptions {
    pub connection: Option<ProviderConnection>,
    pub trait_: Option<AnthropicTrait>,
    pub classify_error: Option<LlmErrorClassifier>,
    pub client_factory: Option<AnthropicClientFactory>,
    pub log: Option<LlmLogger>,
    pub beta_api: Option<bool>,
}

#[derive(Clone, Default)]
pub struct AnthropicRequestPreparationOptions {
    pub trait_: Option<AnthropicTrait>,
    pub beta_api: bool,
}

fn anthropic_custom_header_env_names() -> Vec<String> {
    let Ok(custom_headers) = std::env::var("ANTHROPIC_CUSTOM_HEADERS") else {
        return Vec::new();
    };
    if custom_headers.is_empty() {
        return Vec::new();
    }
    custom_headers
        .split('\n')
        .filter_map(|line| {
            let colon = line.find(':')?;
            let name = line[..colon].trim().to_lowercase();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

fn is_anthropic_oauth_api_key(api_key: &str) -> bool {
    api_key.starts_with("sk-ant-oat01-")
}

fn build_default_headers(
    api_key: Option<&str>,
    headers: Option<&HashMap<String, String>>,
) -> HashMap<String, Option<String>> {
    let mut default_headers = HashMap::new();
    default_headers.insert(
        "authorization".to_string(),
        api_key
            .filter(|api_key| is_anthropic_oauth_api_key(api_key))
            .map(|api_key| format!("Bearer {api_key}")),
    );
    for name in anthropic_custom_header_env_names() {
        default_headers.insert(name, None);
    }
    for (name, value) in headers.into_iter().flatten() {
        default_headers.insert(name.to_lowercase(), Some(value.clone()));
    }
    default_headers
}

fn clamp_thinking_budget(
    kwargs: &mut Map<String, Value>,
    final_cap: u64,
    model: &str,
) -> Result<(), WireAsyncError> {
    let Some(thinking) = kwargs.get_mut("thinking").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(budget) = thinking.get("budget_tokens").and_then(Value::as_u64) else {
        return Ok(());
    };
    if budget < final_cap {
        return Ok(());
    }
    if final_cap <= MIN_ANTHROPIC_THINKING_BUDGET {
        return Err(Box::new(ThinkingConfigError::new(
            ThinkingConfigErrorCode::BudgetExceedsCap,
            format!(
                "Thinking budget for '{model}' needs at least {MIN_ANTHROPIC_THINKING_BUDGET} tokens but max_tokens is {final_cap}. Raise maxCompletionTokens or lower the thinking effort."
            ),
        )));
    }
    let clamped = MIN_ANTHROPIC_THINKING_BUDGET.max(final_cap - 1);
    thinking.insert("budget_tokens".to_string(), Value::from(clamped));
    Ok(())
}

pub fn prepare_anthropic_request(
    input: &FormatRequestInput,
    options: &AnthropicRequestPreparationOptions,
) -> Result<AnthropicRequestParams, WireAsyncError> {
    let trait_ = options.trait_.as_ref();
    let ctx = TraitContext {
        model: input.model.clone(),
    };
    let mut kwargs = Map::from_iter([(
        "betaFeatures".to_string(),
        json!([INTERLEAVED_THINKING_BETA]),
    )]);
    if let Some(thinking) = &input.thinking {
        let fallback = Arc::new(
            |thinking: &crate::human::llm::thinking::ThinkingRequestOptions, ctx: &TraitContext| {
                encode_thinking(thinking, &ctx.model)
            },
        );
        kwargs = apply_thinking(
            kwargs,
            thinking,
            trait_.and_then(|trait_| trait_.thinking.as_ref()),
            &ctx,
            Some(&fallback),
        )
        .kwargs;
    }
    if let Some(format) = &input.response_format {
        kwargs = apply_anthropic_response_format(&kwargs, format)?;
    }
    let cap = resolve_max_completion_cap(input);
    let final_cap = resolve_default_max_tokens_with_override(&ctx.model.model, cap);
    clamp_thinking_budget(&mut kwargs, final_cap, &ctx.model.model)?;
    if cap.is_some() {
        kwargs.extend(
            trait_
                .and_then(|trait_| trait_.encode_max_completion_tokens.as_ref())
                .and_then(|encode| encode(final_cap, &ctx))
                .unwrap_or_else(|| encode_anthropic_max_tokens(final_cap)),
        );
    }
    if let Some(extra) = input
        .extra_params
        .as_ref()
        .and_then(|extra| extra.anthropic.as_ref())
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
    if let Some(keep) = input.thinking.as_ref().and_then(|thinking| thinking.keep.as_deref()) {
        kwargs = apply_anthropic_thinking_keep(&kwargs, keep);
    }
    let accepted_mimes = trait_
        .and_then(|trait_| trait_.accepted_image_mimes.as_ref())
        .and_then(|accepted| accepted(&ctx))
        .unwrap_or_else(|| provider_image_policy().accepted_mimes);
    let lowered = lower_anthropic_messages(input, &accepted_mimes);
    let mut converted = Vec::new();
    for lowered in lowered {
        let message = if let Some(convert) = trait_.and_then(|trait_| trait_.convert_message.as_ref()) {
            match convert(&lowered.source, &lowered.message, &ctx) {
                Some(None) => continue,
                Some(Some(message)) => message,
                None => lowered.message,
            }
        } else {
            lowered.message
        };
        if !is_anthropic_wire_message_empty(&message) {
            converted.push(message);
        }
    }
    let merged = trait_
        .and_then(|trait_| trait_.merge_history.as_ref())
        .and_then(|merge| merge(&converted, &ctx))
        .unwrap_or_else(|| default_anthropic_merge_history(&converted));
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            trait_
                .and_then(|trait_| trait_.convert_tool.as_ref())
                .and_then(|convert| convert(tool, &ctx))
                .unwrap_or_else(|| default_anthropic_tool(tool))
        })
        .collect();
    let assembly = assemble_anthropic_request(
        input,
        &AnthropicRequestParts {
            messages: merged,
            tools,
            kwargs,
            beta_api: options.beta_api,
        },
    );
    let final_params = trait_
        .and_then(|trait_| trait_.build_params.as_ref())
        .and_then(|build| build(assembly.params.clone(), &ctx))
        .unwrap_or(assembly.params.clone());
    Ok(encode_anthropic_request(AnthropicRequestAssembly {
        params: final_params,
        betas: assembly.betas,
        use_beta_api: assembly.use_beta_api,
    }))
}

pub type AnthropicChunkStream =
    Pin<Box<dyn Stream<Item = Result<AnthropicRawStreamEvent, WireAsyncError>> + Send>>;

pub struct AnthropicStreamResponse {
    pub headers: reqwest::header::HeaderMap,
    pub chunks: AnthropicChunkStream,
}

#[async_trait]
pub trait AnthropicClient: Send + Sync {
    async fn create_stream(
        &self,
        request: AnthropicRequestParams,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<AnthropicStreamResponse, WireAsyncError>;
}

struct ReqwestAnthropicClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    headers: HashMap<String, Option<String>>,
}

#[async_trait]
impl AnthropicClient for ReqwestAnthropicClient {
    async fn create_stream(
        &self,
        request: AnthropicRequestParams,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<AnthropicStreamResponse, WireAsyncError> {
        let path = if request.use_beta_api {
            "/v1/messages?beta=true"
        } else {
            "/v1/messages"
        };
        let mut headers = self.headers.clone();
        headers.insert("anthropic-version".to_string(), Some("2023-06-01".to_string()));
        headers.insert("x-api-key".to_string(), Some(self.api_key.clone()));
        if !request.use_beta_api && !request.betas.is_empty() {
            headers.insert("anthropic-beta".to_string(), Some(request.betas.join(",")));
        }
        let mut builder = self
            .client
            .post(format!("{}{path}", self.base_url.trim_end_matches('/')))
            .json(&request.params);
        for (name, value) in headers.into_iter().filter_map(|(name, value)| value.map(|value| (name, value))) {
            builder = builder.header(name, value);
        }
        let response = builder.send().await.map_err(|error| {
            Box::new(if error.is_timeout() {
                AnthropicSdkError::ConnectionTimeout(error.to_string())
            } else {
                AnthropicSdkError::Connection(error.to_string())
            }) as WireAsyncError
        })?;
        let status = response.status();
        let headers = response.headers().clone();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
            let error = parsed.get("error");
            return Err(Box::new(AnthropicSdkError::Api(AnthropicApiError {
                status: Some(status.as_u16()),
                request_id: headers
                    .get("x-request-id")
                    .or_else(|| headers.get("request-id"))
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string),
                headers: Some(headers),
                type_: parsed.get("type").and_then(Value::as_str).map(str::to_string),
                body_type: error
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                body_message: error
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                message: body,
                has_body: error.is_some(),
            })));
        }
        if signal.is_cancelled() {
            return Err("The operation was aborted.".into());
        }
        Ok(AnthropicStreamResponse {
            headers,
            chunks: parse_anthropic_sse_stream(response),
        })
    }
}

fn parse_anthropic_sse_stream(response: reqwest::Response) -> AnthropicChunkStream {
    Box::pin(futures_util::stream::try_unfold(
        (response.bytes_stream(), String::new(), false),
        |(mut stream, mut pending, mut done)| async move {
            loop {
                if done {
                    return Ok(None);
                }
                if let Some(event) = take_anthropic_sse_event(&mut pending) {
                    if event.trim() == "[DONE]" {
                        done = true;
                        return Ok(None);
                    }
                    if event.trim().is_empty() {
                        continue;
                    }
                    let chunk = serde_json::from_str::<AnthropicRawStreamEvent>(&event)?;
                    return Ok(Some((chunk, (stream, pending, done))));
                }
                match stream.next().await {
                    Some(chunk) => {
                        let chunk = chunk.map_err(|error| {
                            Box::new(AnthropicSdkError::Connection(error.to_string())) as WireAsyncError
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
                        let chunk = serde_json::from_str::<AnthropicRawStreamEvent>(&event)?;
                        return Ok(Some((chunk, (stream, pending, done))));
                    }
                }
            }
        }),
    )
}

fn take_anthropic_sse_event(pending: &mut String) -> Option<String> {
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

pub struct AnthropicRequester {
    connection: Option<ProviderConnection>,
    trait_: Option<AnthropicTrait>,
    classify_error: Option<LlmErrorClassifier>,
    format: AnthropicProtocolFormat,
    resolve_client: AnthropicClientFactory,
    beta_api: bool,
}

impl AnthropicRequester {
    async fn execute(
        &self,
        request: AnthropicRequestParams,
        ctx: TraitContext,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let headers = merge_request_headers(
            self.connection
                .as_ref()
                .and_then(|connection| connection.default_headers.as_ref())
                .and_then(|headers| headers(ConnectionContext {
                    model: ctx.model.clone(),
                }))
                .as_ref(),
            ctx.model.connection.default_headers.as_ref(),
        );
        let client = (self.resolve_client)(LlmClientContext {
            model: ctx.model.clone(),
            headers,
        })?;
        (control.on_event)(LlmRequestEvent::Sent);
        let response = client.create_stream(request, control.signal.clone()).await?;
        (control.on_event)(LlmRequestEvent::StreamingHeaders {
            headers: headers_to_record(Some(&response.headers)).unwrap_or_default(),
        });
        let parse = self.format.create_stream_parser(StreamParserOptions::default());
        let mut message_id = None;
        let mut chunks = response.chunks;
        while let Some(chunk) = chunks.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    (control.on_event)(LlmRequestEvent::FailedRemote {
                        error: convert_anthropic_error(&error, None, true),
                        raw_error: Some(Value::String(error.to_string())),
                    });
                    return Ok(());
                }
            };
            let mut failed = false;
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
                    Arc::new(move |id| {
                        if message_id.as_ref() == Some(&id) {
                            return;
                        }
                        message_id = Some(id.clone());
                        on_event(LlmRequestEvent::StreamingMessageId { message_id: id });
                    })
                }),
                on_usage: Some({
                    let on_event = control.on_event.clone();
                    Arc::new(move |usage| on_event(LlmRequestEvent::StreamingUsage { usage }))
                }),
                on_error: Some({
                    let on_event = control.on_event.clone();
                    Arc::new(move |error| {
                        failed = true;
                        on_event(LlmRequestEvent::FailedRemote {
                            error,
                            raw_error: None,
                        });
                    })
                }),
            });
            if let Err(error) = parse_result {
                (control.on_event)(LlmRequestEvent::FailedRemote {
                    error: convert_anthropic_error(&error, None, true),
                    raw_error: Some(Value::String(error.to_string())),
                });
                return Ok(());
            }
            if failed {
                return Ok(());
            }
        }
        (control.on_event)(LlmRequestEvent::Done);
        Ok(())
    }
}

#[async_trait]
impl LlmRequester for AnthropicRequester {
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
                .unwrap_or_else(|| ANTHROPIC_TOOL_CALL_ID_POLICY.clone());
            let messages = normalize_tool_call_ids_for_provider(&content.messages, &policy)?;
            prepare_anthropic_request(
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
                &AnthropicRequestPreparationOptions {
                    trait_: self.trait_.clone(),
                    beta_api: self.beta_api,
                },
            )
        }))
        .unwrap_or_else(|payload| {
            let error: WireAsyncError = match payload.downcast::<SyntaxRequestFormatError>() {
                Ok(error) => error,
                Err(payload) => match payload.downcast::<ThinkingConfigError>() {
                    Ok(error) => error,
                    Err(payload) => {
                        if let Some(message) = payload.downcast_ref::<String>() {
                            message.clone().into()
                        } else if let Some(message) = payload.downcast_ref::<&str>() {
                            (*message).into()
                        } else {
                            std::panic::resume_unwind(payload)
                        }
                    }
                },
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
                error: convert_anthropic_error(&error, self.classify_error.as_ref(), false),
                raw_error: Some(Value::String(error.to_string())),
            });
        }
        Ok(())
    }
}

pub fn create_anthropic_requester(options: AnthropicRequesterOptions) -> Arc<AnthropicRequester> {
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
            let api_key = request
                .model
                .connection
                .api_key
                .clone()
                .unwrap_or_else(|| "unused".to_string());
            Ok(Arc::new(ReqwestAnthropicClient {
                client: builder.build()?,
                base_url: request
                    .model
                    .connection
                    .base_url
                    .clone()
                    .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
                headers: build_default_headers(Some(&api_key), request.headers.as_ref()),
                api_key,
            }) as Arc<dyn AnthropicClient>)
        })
    });
    Arc::new(AnthropicRequester {
        connection: options.connection,
        trait_: options.trait_,
        classify_error: options.classify_error,
        format: AnthropicProtocolFormat::new(),
        resolve_client,
        beta_api: options.beta_api.unwrap_or(false),
    })
}

pub struct AnthropicBase {
    beta_api: bool,
    options: Option<AnthropicRequesterOptions>,
}

impl AnthropicBase {
    const fn unconfigured(beta_api: bool) -> Self {
        Self {
            beta_api,
            options: None,
        }
    }
}

impl ProtocolBase<AnthropicTrait> for AnthropicBase {
    fn capability(&self, model_name: &str) -> Option<crate::human::llm::capability::ModelCapability> {
        get_anthropic_model_capability(model_name)
    }

    fn create_requester(
        &self,
        options: ProtocolRequesterOptions<AnthropicTrait>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        let base = self.options.clone().unwrap_or_default();
        Ok(create_anthropic_requester(AnthropicRequesterOptions {
            connection: options.connection.or(base.connection),
            trait_: options.trait_.or(base.trait_),
            classify_error: options.classify_error.or(base.classify_error),
            client_factory: base.client_factory,
            log: options.log.or(base.log),
            beta_api: Some(self.beta_api),
        }))
    }
}

pub fn create_anthropic_base(options: AnthropicRequesterOptions) -> AnthropicBase {
    AnthropicBase {
        beta_api: options.beta_api.unwrap_or(false),
        options: Some(options),
    }
}

pub static ANTHROPIC_BASE: AnthropicBase = AnthropicBase::unconfigured(false);
pub static ANTHROPIC_BETA_BASE: AnthropicBase = AnthropicBase::unconfigured(true);
