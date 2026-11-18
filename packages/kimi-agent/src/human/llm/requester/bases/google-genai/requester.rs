use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde_json::{Map, Value};

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
use crate::human::llm::requester::bases::google_genai::capability::get_google_gen_ai_model_capability;
use crate::human::llm::requester::bases::google_genai::format::*;
use crate::human::llm::requester::bases::google_genai::r#trait::GoogleGenAITrait;
use crate::human::llm::requester::requester::*;
use crate::human::llm::requester::timeout::{Dispatcher, get_llm_dispatcher};
use crate::human::llm::syntax_errors::{SyntaxRequestFormatError, to_llm_syntax_error_message};
use crate::human::llm::thinking::ThinkingConfigError;
use crate::human::log::log::LlmLogger;
use crate::wire::record::WireAsyncError;

const GEMINI_API_VERSION: &str = "v1beta";
const VERTEX_API_VERSION: &str = "v1beta1";

pub type GoogleGenAIClientFactory = Arc<
    dyn Fn(LlmClientContext) -> Result<Arc<dyn GoogleGenAIClient>, WireAsyncError> + Send + Sync,
>;

#[derive(Clone, Default)]
pub struct GoogleGenAIRequesterOptions {
    pub connection: Option<ProviderConnection>,
    pub trait_: Option<GoogleGenAITrait>,
    pub classify_error: Option<LlmErrorClassifier>,
    pub client_factory: Option<GoogleGenAIClientFactory>,
    pub log: Option<LlmLogger>,
    pub vertexai: Option<bool>,
}

#[derive(Clone, Default)]
pub struct GoogleGenAIRequestPreparationOptions {
    pub trait_: Option<GoogleGenAITrait>,
}

pub fn prepare_google_gen_ai_request(
    input: &FormatRequestInput,
    options: &GoogleGenAIRequestPreparationOptions,
) -> Result<GoogleGenAIRequestParams, WireAsyncError> {
    let trait_ = options.trait_.as_ref();
    let ctx = TraitContext {
        model: input.model.clone(),
    };
    let mut kwargs = Map::new();
    if let Some(thinking) = &input.thinking {
        let fallback: crate::human::llm::protocol::thinking::ThinkingFallback = Arc::new(
            |thinking: &crate::human::llm::thinking::ThinkingRequestOptions, ctx: &TraitContext| {
                Some(Map::from_iter([(
                    "thinkingConfig".to_string(),
                    Value::Object(encode_google_gen_ai_thinking(
                        &ctx.model.model,
                        &thinking.effort,
                    )),
                )]))
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
    if let Some(cap) = resolve_max_completion_cap(input) {
        kwargs.extend(
            trait_
                .and_then(|trait_| trait_.encode_max_completion_tokens.as_ref())
                .and_then(|encode| encode(cap, &ctx))
                .unwrap_or_else(|| encode_google_gen_ai_max_output_tokens(cap)),
        );
    }
    if let Some(format) = &input.response_format {
        kwargs = apply_google_gen_ai_response_format(&kwargs, format);
    }
    if let Some(extra) = input
        .extra_params
        .as_ref()
        .and_then(|extra| extra.google_genai.as_ref())
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
    let contents = lower_google_gen_ai_messages(&input.messages, &input.model.model)?;
    let merged = trait_
        .and_then(|trait_| trait_.merge_history.as_ref())
        .and_then(|merge| merge(&contents, &ctx))
        .unwrap_or(contents);
    let tools = input
        .tools
        .iter()
        .map(|tool| {
            trait_
                .and_then(|trait_| trait_.convert_tool.as_ref())
                .and_then(|convert| convert(tool, &ctx))
                .unwrap_or_else(|| default_google_gen_ai_tool(tool))
        })
        .collect();
    let params = assemble_google_gen_ai_request(
        input,
        &GoogleGenAIRequestParts {
            contents: merged,
            tools,
            kwargs,
        },
    );
    let final_params = trait_
        .and_then(|trait_| trait_.build_params.as_ref())
        .and_then(|build| build(params.clone(), &ctx))
        .unwrap_or(params);
    Ok(encode_google_gen_ai_request(final_params))
}

fn sdk_http_base(base_url: &str, vertexai: bool) -> String {
    let version = if vertexai {
        VERTEX_API_VERSION
    } else {
        GEMINI_API_VERSION
    };
    let Ok(mut url) = reqwest::Url::parse(base_url) else {
        return base_url.to_string();
    };
    let mut segments = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if segments.last().map(String::as_str) != Some(version) {
        return base_url.to_string();
    }
    segments.pop();
    let pathname = if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    };
    url.set_path(&pathname);
    url.to_string()
}

pub type GoogleGenAIChunkStream = Pin<Box<dyn Stream<Item = Result<Value, WireAsyncError>> + Send>>;

#[async_trait]
pub trait GoogleGenAIClient: Send + Sync {
    async fn create_stream(
        &self,
        request: GoogleGenAIRequestParams,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<GoogleGenAIChunkStream, WireAsyncError>;
}

#[derive(Debug)]
struct GoogleGenAIAbortError;

impl std::fmt::Display for GoogleGenAIAbortError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The operation was aborted.")
    }
}

impl std::error::Error for GoogleGenAIAbortError {}

struct ReqwestGoogleGenAIClient {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    headers: HashMap<String, String>,
    vertexai: bool,
}

#[async_trait]
impl GoogleGenAIClient for ReqwestGoogleGenAIClient {
    async fn create_stream(
        &self,
        request: GoogleGenAIRequestParams,
        signal: tokio_util::sync::CancellationToken,
    ) -> Result<GoogleGenAIChunkStream, WireAsyncError> {
        if signal.is_cancelled() {
            return Err(Box::new(GoogleGenAIAbortError));
        }
        let version = if self.vertexai {
            VERTEX_API_VERSION
        } else {
            GEMINI_API_VERSION
        };
        let model = request
            .params
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if model.is_empty() {
            return Err(Box::new(GoogleGenAISdkError::Other {
                message: "model is required and must be a string".to_string(),
                code: None,
            }));
        }
        if model.contains("..") || model.contains('?') || model.contains('&') {
            return Err(Box::new(GoogleGenAISdkError::Other {
                message: "invalid model parameter".to_string(),
                code: None,
            }));
        }
        let model_path = if self.vertexai {
            if model.starts_with("publishers/") || model.starts_with("projects/") || model.starts_with("models/") {
                model.to_string()
            } else if model.contains('/') {
                let mut segments = model.split('/');
                let publisher = segments.next().unwrap();
                let model = segments.next().unwrap();
                format!("publishers/{publisher}/models/{model}")
            } else {
                format!("publishers/google/models/{model}")
            }
        } else if model.starts_with("models/") || model.starts_with("tunedModels/") {
            model.to_string()
        } else {
            format!("models/{model}")
        };
        let endpoint = format!(
            "{}/{version}/{model_path}:streamGenerateContent?alt=sse",
            self.base_url.trim_end_matches('/')
        );
        let mut body = request.params.clone();
        body.remove("model");
        if let Some(config) = body.remove("config") {
            let mut config = config.as_object().cloned().unwrap_or_default();
            for key in [
                "systemInstruction",
                "tools",
                "safetySettings",
                "toolConfig",
                "cachedContent",
                "serviceTier",
            ] {
                if let Some(value) = config.remove(key)
                    && !value.is_null()
                {
                    let value = if key == "systemInstruction" && value.is_string() {
                        serde_json::json!({ "parts": [{ "text": value }] })
                    } else {
                        value
                    };
                    body.insert(key.to_string(), value);
                }
            }
            if self.vertexai {
                for key in ["labels", "modelArmorConfig"] {
                    if let Some(value) = config.remove(key)
                        && !value.is_null()
                    {
                        body.insert(key.to_string(), value);
                    }
                }
                if let Some(value) = config.remove("modelSelectionConfig")
                    && !value.is_null()
                {
                    body.insert("modelConfig".to_string(), value);
                }
            }
            body.insert("generationConfig".to_string(), Value::Object(config));
        }
        let mut builder = self.client.post(endpoint).json(&body);
        if let Some(api_key) = &self.api_key {
            builder = builder.header("x-goog-api-key", api_key);
        }
        for (name, value) in &self.headers {
            builder = builder.header(name, value);
        }
        let response = tokio::select! {
            response = builder.send() => response.map_err(|error| {
                Box::new(GoogleGenAISdkError::Other {
                    message: error.to_string(),
                    code: None,
                }) as WireAsyncError
            })?,
            _ = signal.cancelled() => return Err(Box::new(GoogleGenAIAbortError)),
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Box::new(GoogleGenAISdkError::Api(GoogleGenAIApiError {
                status: status.as_u16(),
                message: body,
            })));
        }
        Ok(parse_google_gen_ai_sse_stream(response, signal))
    }
}

fn parse_google_gen_ai_sse_stream(
    response: reqwest::Response,
    signal: tokio_util::sync::CancellationToken,
) -> GoogleGenAIChunkStream {
    Box::pin(futures_util::stream::try_unfold(
        (response.bytes_stream(), String::new(), false, signal),
        |(mut stream, mut pending, mut done, signal)| async move {
            loop {
                if signal.is_cancelled() {
                    return Err(Box::new(GoogleGenAIAbortError) as WireAsyncError);
                }
                if done {
                    return Ok(None);
                }
                if let Some(event) = take_google_gen_ai_sse_event(&mut pending) {
                    if event.trim() == "[DONE]" {
                        done = true;
                        return Ok(None);
                    }
                    if event.trim().is_empty() {
                        continue;
                    }
                    let chunk = serde_json::from_str::<Value>(&event)?;
                    return Ok(Some((chunk, (stream, pending, done, signal))));
                }
                let next = tokio::select! {
                    next = stream.next() => next,
                    _ = signal.cancelled() => return Err(Box::new(GoogleGenAIAbortError) as WireAsyncError),
                };
                match next {
                    Some(chunk) => {
                        let chunk = chunk.map_err(|error| {
                            Box::new(GoogleGenAISdkError::Other {
                                message: error.to_string(),
                                code: None,
                            }) as WireAsyncError
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
                        let chunk = serde_json::from_str::<Value>(&event)?;
                        return Ok(Some((chunk, (stream, pending, done, signal))));
                    }
                }
            }
        }),
    )
}

fn take_google_gen_ai_sse_event(pending: &mut String) -> Option<String> {
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

pub struct GoogleGenAIRequester {
    connection: Option<ProviderConnection>,
    trait_: Option<GoogleGenAITrait>,
    classify_error: Option<LlmErrorClassifier>,
    format: GoogleGenAIProtocolFormat,
    resolve_client: GoogleGenAIClientFactory,
}

impl GoogleGenAIRequester {
    async fn execute(
        &self,
        request: GoogleGenAIRequestParams,
        ctx: TraitContext,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let headers = merge_request_headers(
            merge_request_headers(
                self.connection
                    .as_ref()
                    .and_then(|connection| connection.default_headers.as_ref())
                    .and_then(|headers| headers(ConnectionContext {
                        model: ctx.model.clone(),
                    }))
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
        let mut chunks = client
            .create_stream(request, control.signal.clone())
            .await?;
        let parse = self.format.create_stream_parser(StreamParserOptions::default());
        let message_id = std::sync::Arc::new(parking_lot::Mutex::new(None::<String>));
        while let Some(chunk) = chunks.next().await {
            if control.signal.is_cancelled() {
                let error = Box::new(GoogleGenAIAbortError) as WireAsyncError;
                (control.on_event)(LlmRequestEvent::FailedRemote {
                    error: convert_google_gen_ai_error(
                        &error,
                        self.classify_error.as_ref(),
                        true,
                    ),
                    raw_error: None,
                });
                return Ok(());
            }
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    (control.on_event)(LlmRequestEvent::FailedRemote {
                        error: convert_google_gen_ai_error(
                            &error,
                            self.classify_error.as_ref(),
                            true,
                        ),
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
                    error: convert_google_gen_ai_error(
                        &error,
                        self.classify_error.as_ref(),
                        true,
                    ),
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
impl LlmRequester for GoogleGenAIRequester {
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
            prepare_google_gen_ai_request(
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
                    messages: content.messages,
                    tools,
                    used_context_tokens: content.used_context_tokens,
                },
                &GoogleGenAIRequestPreparationOptions {
                    trait_: self.trait_.clone(),
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
                error: convert_google_gen_ai_error(&error, self.classify_error.as_ref(), false),
                raw_error: Some(Value::String(error.to_string())),
            });
        }
        Ok(())
    }
}

pub fn create_google_gen_ai_requester(
    options: GoogleGenAIRequesterOptions,
) -> Arc<GoogleGenAIRequester> {
    let vertexai = options.vertexai.unwrap_or(false);
    let resolve_client = options.client_factory.unwrap_or_else(|| {
        Arc::new(move |request: LlmClientContext| {
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
            let vertexai = vertexai || request.model.connection.vertexai == Some(true);
            let base_url = request.model.connection.base_url.as_deref().map_or_else(
                || {
                    if vertexai {
                        "https://aiplatform.googleapis.com/".to_string()
                    } else {
                        "https://generativelanguage.googleapis.com/".to_string()
                    }
                },
                |base_url| sdk_http_base(base_url, vertexai),
            );
            Ok(Arc::new(ReqwestGoogleGenAIClient {
                client: builder.build()?,
                base_url,
                api_key: request.model.connection.api_key.clone(),
                headers: request.headers.unwrap_or_default(),
                vertexai,
            }) as Arc<dyn GoogleGenAIClient>)
        })
    });
    Arc::new(GoogleGenAIRequester {
        connection: options.connection,
        trait_: options.trait_,
        classify_error: options.classify_error,
        format: GoogleGenAIProtocolFormat::new(),
        resolve_client,
    })
}

pub struct GoogleGenAIBase {
    client_factory: Option<GoogleGenAIClientFactory>,
    vertexai: bool,
}

impl GoogleGenAIBase {
    const fn unconfigured() -> Self {
        Self {
            client_factory: None,
            vertexai: false,
        }
    }
}

impl ProtocolBase<GoogleGenAITrait> for GoogleGenAIBase {
    fn capability(&self, model_name: &str) -> Option<crate::human::llm::capability::ModelCapability> {
        get_google_gen_ai_model_capability(model_name)
    }

    fn create_requester(
        &self,
        options: ProtocolRequesterOptions<GoogleGenAITrait>,
    ) -> Result<Arc<dyn LlmRequester>, WireAsyncError> {
        Ok(create_google_gen_ai_requester(GoogleGenAIRequesterOptions {
            connection: options.connection,
            trait_: options.trait_,
            classify_error: options.classify_error,
            client_factory: self.client_factory.clone(),
            log: options.log,
            vertexai: Some(self.vertexai),
        }))
    }
}

pub fn create_google_gen_ai_base(
    client_factory: Option<GoogleGenAIClientFactory>,
    vertexai: bool,
) -> GoogleGenAIBase {
    GoogleGenAIBase {
        client_factory,
        vertexai,
    }
}

pub static GOOGLE_GEN_AI_BASE: GoogleGenAIBase = GoogleGenAIBase::unconfigured();
