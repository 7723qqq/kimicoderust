use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::human::credentials::credentials::apply_credential;
use crate::human::llm::empty_response::empty_response_error;
use crate::human::llm::errors::LlmErrorMessage;
use crate::human::llm::finish_reason::{FinishInfo, NO_FINISH};
use crate::human::llm::media::upload::{
    ImageUploadInput as HumanImageUploadInput, MediaVideoUploadOptions, ProviderMediaContribution,
    VideoUploadInput as HumanVideoUploadInput,
};
use crate::human::llm::message::{AssistantMessage, MessageAccumulator};
use crate::human::llm::model::LlmModel;
use crate::human::llm::requester::bases::anthropic::extra_params::AnthropicExtraParams;
use crate::human::llm::requester::bases::google_genai::extra_params::GoogleGenAIExtraParams;
use crate::human::llm::requester::bases::openai::extra_params::OpenAIExtraParams;
use crate::human::llm::requester::bases::openai_responses::extra_params::OpenAIResponsesExtraParams;
use crate::human::llm::requester::requester::{
    ExtraParams, LlmRequestConfig, LlmRequestContent, LlmRequestControl, LlmRequestEvent,
    LlmRequester,
};
use crate::human::llm::thinking::ThinkingRequestOptions;
use crate::human::llm::usage::{TokenUsage, merge_usage_patch};
use crate::llm_adapter::contract::errors::{
    ChatProviderError, ImageUploadUnsupportedError, ProviderErrorCode, VideoUploadUnsupportedError,
    error_from_llm_message, is_abort_error, llm_message_from_error, trace_id_from_headers_record,
};
use crate::llm_adapter::contract::message::{
    ContentPart, StreamedMessagePart, Tool, from_llm_assistant_message, to_llm_message,
};
use crate::llm_adapter::model::catalog::Model;
use crate::llm_adapter::model::model_requester::{
    ImageUploadInput, MediaUploadOptions, ModelRequestAttemptFailure, ModelRequestEvent,
    ModelRequestInput, ModelRequestParams, ModelRequestStream, ModelRequestTiming, ModelRequester,
    SamplingOptions, VideoUploadSource,
};
use crate::llm_adapter::protocol::errors::translate_provider_error;
use crate::llm_adapter::protocol::protocol::{IProtocolAdapterRegistry, Protocol};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct ResolvedLlmModel {
    pub requester: Arc<dyn LlmRequester>,
    pub protocol: Protocol,
    pub model: LlmModel,
    pub media: Option<ProviderMediaContribution>,
}

pub trait ModelLlmGateway: Send + Sync {
    fn resolve(&self, model: &Model) -> Result<ResolvedLlmModel, WireAsyncError>;
}

pub struct RegistryGateway {
    registry: Arc<dyn IProtocolAdapterRegistry>,
}

impl RegistryGateway {
    pub fn new(registry: Arc<dyn IProtocolAdapterRegistry>) -> Self {
        Self { registry }
    }
}

impl ModelLlmGateway for RegistryGateway {
    fn resolve(&self, model: &Model) -> Result<ResolvedLlmModel, WireAsyncError> {
        self.registry.resolve(model)
    }
}

#[derive(Debug, Clone, Default)]
pub struct StreamDecodeStats {
    pub server_decode_ms: u64,
    pub client_consume_ms: u64,
    pub client_blocked_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct OutputPartTimes {
    pub first_token_at: Option<u64>,
    pub last_token_at: Option<u64>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

struct RequestState {
    request_started_at: u64,
    request_sent_at: Option<u64>,
    first_chunk_at: Option<u64>,
    stream_ended_at: Option<u64>,
    first_token_at: Option<u64>,
    last_token_at: Option<u64>,
    server_decode_ms: u64,
    client_consume_ms: u64,
    last_resume_at: u64,
    accumulator: MessageAccumulator,
    usage: Option<TokenUsage>,
    finish: Option<FinishInfo>,
    message_id: Option<String>,
    trace_id: Option<String>,
    failed: Option<LlmErrorMessage>,
}

impl Default for RequestState {
    fn default() -> Self {
        Self {
            request_started_at: now_ms(),
            request_sent_at: None,
            first_chunk_at: None,
            stream_ended_at: None,
            first_token_at: None,
            last_token_at: None,
            server_decode_ms: 0,
            client_consume_ms: 0,
            last_resume_at: 0,
            accumulator: MessageAccumulator::default(),
            usage: None,
            finish: None,
            message_id: None,
            trace_id: None,
            failed: None,
        }
    }
}

impl RequestState {
    fn reset_attempt(&mut self) {
        self.accumulator = MessageAccumulator::default();
        self.usage = None;
        self.finish = None;
        self.message_id = None;
    }
}

pub struct ModelRequesterImpl {
    model: Model,
    gateway: Arc<dyn ModelLlmGateway>,
    cached: Mutex<Option<ResolvedLlmModel>>,
    cached_requester: Mutex<Option<Arc<dyn LlmRequester>>>,
}

impl ModelRequesterImpl {
    pub fn new(model: Model, gateway: Arc<dyn ModelLlmGateway>) -> Self {
        Self {
            model,
            gateway,
            cached: Mutex::new(None),
            cached_requester: Mutex::new(None),
        }
    }

    fn resolve(&self) -> Result<ResolvedLlmModel, WireAsyncError> {
        if let Some(resolved) = self.cached.lock().clone() {
            return Ok(resolved);
        }
        let resolved = self.gateway.resolve(&self.model)?;
        *self.cached.lock() = Some(resolved.clone());
        Ok(resolved)
    }

    fn requester_for(
        &self,
        resolved: &ResolvedLlmModel,
    ) -> Arc<dyn LlmRequester> {
        self.cached_requester
            .lock()
            .get_or_insert_with(|| Arc::new(ThrowToEventRequester {
                inner: resolved.requester.clone(),
            }))
            .clone()
    }

    async fn run_request(
        model: Model,
        resolved: ResolvedLlmModel,
        requester: Arc<dyn LlmRequester>,
        input: ModelRequestInput,
        signal: Option<CancellationToken>,
        params: Option<ModelRequestParams>,
        sender: mpsc::UnboundedSender<Result<ModelRequestEvent, WireAsyncError>>,
    ) -> Result<(), WireAsyncError> {
        if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
            return Err("The operation was aborted.".into());
        }
        let state = Arc::new(Mutex::new(RequestState::default()));
        let credential = match &model.credential_provider {
            Some(provider) => provider.resolve().await?,
            None => None,
        };
        let config = LlmRequestConfig {
            model: apply_credential(&resolved.model, credential),
            credential_provider: None,
            system_prompt: Some(input.system_prompt),
            tools: Some(wire_tools(input.tools)),
            cache_key: params.as_ref().and_then(|params| params.cache_key.clone()),
            thinking: params.as_ref().and_then(|params| {
                params.thinking_effort.clone().map(|effort| ThinkingRequestOptions {
                    effort,
                    keep: params.thinking_keep.clone(),
                })
            }),
            response_format: input.response_format,
            max_completion_tokens: params
                .as_ref()
                .and_then(|params| params.max_completion_tokens)
                .map(u64::from),
            max_context_tokens: params
                .as_ref()
                .and_then(|params| params.max_context_tokens)
                .map(u64::from),
            determinism: None,
            extra_params: sampling_extra_params(
                resolved.protocol,
                params.as_ref().and_then(|params| params.sampling.as_ref()),
            ),
            tool_message_conversion: None,
        };
        let content = LlmRequestContent {
            messages: input.messages.into_iter().map(to_llm_message).collect(),
            used_context_tokens: params
                .as_ref()
                .and_then(|params| params.used_context_tokens)
                .map(u64::from),
        };
        let on_trace_id = params.as_ref().and_then(|params| params.on_trace_id.clone());
        let on_event = {
            let state = state.clone();
            let sender = sender.clone();
            Arc::new(move |event: LlmRequestEvent| {
                match event {
                    LlmRequestEvent::Sent => {
                        let now = now_ms();
                        let mut state = state.lock();
                        if state.request_sent_at.is_some() {
                            state.request_started_at = now;
                            state.reset_attempt();
                        }
                        state.request_sent_at = Some(now);
                    }
                    LlmRequestEvent::StreamingHeaders { headers } => {
                        let trace_id = trace_id_from_headers_record(Some(&headers));
                        state.lock().trace_id = trace_id.clone();
                        if let Some(on_trace_id) = &on_trace_id {
                            on_trace_id(trace_id);
                        }
                    }
                    LlmRequestEvent::StreamingPart { part } => {
                        let arrived_at = now_ms();
                        {
                            let mut state = state.lock();
                            if state.first_chunk_at.is_none() {
                                state.first_chunk_at = Some(arrived_at);
                            } else {
                                state.server_decode_ms = state
                                    .server_decode_ms
                                    .saturating_add(arrived_at.saturating_sub(state.last_resume_at));
                            }
                            if carries_output_tokens(&part) {
                                if state.first_token_at.is_none() {
                                    state.first_token_at = Some(arrived_at);
                                }
                                state.last_token_at = Some(arrived_at);
                            }
                            state.accumulator.push(part.clone());
                            let _ = sender.send(Ok(ModelRequestEvent::Part { part }));
                            state.last_resume_at = now_ms();
                            state.client_consume_ms = state
                                .client_consume_ms
                                .saturating_add(state.last_resume_at.saturating_sub(arrived_at));
                        }
                    }
                    LlmRequestEvent::StreamingUsage { usage: patch } => {
                        let mut state = state.lock();
                        state.usage = Some(merge_usage_patch(state.usage.as_ref(), patch));
                    }
                    LlmRequestEvent::StreamingFinish { finish } => {
                        state.lock().finish = Some(finish);
                    }
                    LlmRequestEvent::StreamingMessageId { message_id } => {
                        state.lock().message_id = Some(message_id);
                    }
                    LlmRequestEvent::FailedSyntax { error }
                    | LlmRequestEvent::FailedRemote { error, .. } => {
                        state.lock().failed = Some(error);
                    }
                    LlmRequestEvent::RequestRetrying => {
                        state.lock().reset_attempt();
                    }
                    LlmRequestEvent::Done => {
                        let now = now_ms();
                        let mut state = state.lock();
                        state.stream_ended_at = Some(now);
                        if state.first_chunk_at.is_some() {
                            state.server_decode_ms = state
                                .server_decode_ms
                                .saturating_add(now.saturating_sub(state.last_resume_at));
                        }
                    }
                }
            })
        };
        requester
            .generate(
                config.clone(),
                content,
                LlmRequestControl {
                    signal: signal.unwrap_or_default(),
                    on_event,
                },
            )
            .await?;

        if let Some(failed) = state.lock().failed.clone() {
            let (usage, message_id, timing) = {
                let snapshot = state.lock();
                let timing = snapshot.first_chunk_at.map(|first_chunk_at| {
                    build_stream_timing(
                        snapshot.request_started_at,
                        snapshot.request_sent_at,
                        first_chunk_at,
                        snapshot.stream_ended_at,
                        Some(StreamDecodeStats {
                            server_decode_ms: snapshot.server_decode_ms,
                            client_consume_ms: snapshot.client_consume_ms,
                            client_blocked_ms: None,
                        }),
                        OutputPartTimes {
                            first_token_at: snapshot.first_token_at,
                            last_token_at: snapshot.last_token_at,
                        },
                    )
                });
                (snapshot.usage.clone(), snapshot.message_id.clone(), timing)
            };
            if let Some(on_attempt_failure) = params
                .as_ref()
                .and_then(|params| params.on_attempt_failure.clone())
            {
                on_attempt_failure(ModelRequestAttemptFailure {
                    usage,
                    message_id,
                    timing,
                });
            }
            return Err(error_from_llm_message(&failed));
        }
        let final_message = state.lock().accumulator.finish();
        let finish = state.lock().finish.clone().unwrap_or(NO_FINISH);
        if let Some(empty) = empty_response_error(&final_message, &config.model, &finish) {
            return Err(error_from_llm_message(&empty));
        }
        let snapshot = state.lock();
        if let Some(usage) = snapshot.usage.clone() {
            let _ = sender.send(Ok(ModelRequestEvent::Usage {
                usage,
                model: Some(model.name.clone()),
            }));
        }
        let _ = sender.send(Ok(ModelRequestEvent::Finish {
            message: from_llm_assistant_message(final_message),
            provider_finish_reason: finish.finish_reason,
            raw_finish_reason: finish.raw_finish_reason.clone(),
            id: snapshot.message_id.clone(),
            trace_id: snapshot.trace_id.clone(),
        }));
        if let Some(first_chunk_at) = snapshot.first_chunk_at {
            let timing = build_stream_timing(
                snapshot.request_started_at,
                snapshot.request_sent_at,
                first_chunk_at,
                snapshot.stream_ended_at,
                Some(StreamDecodeStats {
                    server_decode_ms: snapshot.server_decode_ms,
                    client_consume_ms: snapshot.client_consume_ms,
                    client_blocked_ms: None,
                }),
                OutputPartTimes {
                    first_token_at: snapshot.first_token_at,
                    last_token_at: snapshot.last_token_at,
                },
            );
            let _ = sender.send(Ok(ModelRequestEvent::Timing(timing)));
        }
        Ok(())
    }
}

impl ModelRequester for ModelRequesterImpl {
    fn model(&self) -> &Model {
        &self.model
    }

    fn request(
        &self,
        input: ModelRequestInput,
        signal: Option<CancellationToken>,
        params: Option<ModelRequestParams>,
    ) -> ModelRequestStream {
        let (sender, receiver) = mpsc::unbounded_channel();
        let task_sender = sender.clone();
        let model = self.model.clone();
        let resolved = if signal.as_ref().is_some_and(CancellationToken::is_cancelled) {
            Err("The operation was aborted.".into())
        } else {
            self.resolve()
        };
        let requester = match &resolved {
            Ok(resolved) => Ok(self.requester_for(resolved)),
            Err(error) => Err(std::io::Error::other(error.to_string())),
        };
        tokio::spawn(async move {
            let result = match (resolved, requester) {
                (Ok(resolved), Ok(requester)) => {
                    ModelRequesterImpl::run_request(
                        model,
                        resolved,
                        requester,
                        input,
                        signal,
                        params,
                        task_sender.clone(),
                    )
                    .await
                }
                (Err(error), _) => Err(error),
                (_, Err(error)) => Err(error.into()),
            };
            if let Err(error) = result {
                let _ = task_sender.send(Err(error));
            }
        });
        drop(sender);
        stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|event| (event, receiver))
        })
        .boxed()
    }

    fn upload_video(
        &self,
        input: VideoUploadSource,
        options: MediaUploadOptions,
    ) -> Option<futures_util::future::BoxFuture<'static, Result<ContentPart, WireAsyncError>>> {
        let model = self.model.clone();
        let resolved = match self.resolve() {
            Ok(resolved) => resolved,
            Err(error) => return Some(Box::pin(async move { Err(error) })),
        };
        let uploader = resolved.media.as_ref().and_then(|media| media.upload_video.clone());
        Some(Box::pin(async move {
            let Some(uploader) = uploader else {
                return Err(Box::new(VideoUploadUnsupportedError::new(format!(
                    "Model \"{}\" (protocol={}) does not support video upload",
                    model.id, model.protocol
                ))) as WireAsyncError);
            };
            let video = match input {
                VideoUploadSource::Url(path) => read_video_file(&path)?,
                VideoUploadSource::Upload(input) => HumanVideoUploadInput {
                    data: input.data,
                    mime_type: input.mime_type,
                    filename: input.filename,
                },
            };
            let credential = match &model.credential_provider {
                Some(provider) => provider.resolve().await?,
                None => None,
            };
            let llm_model = apply_credential(&resolved.model, credential);
            uploader(video, MediaVideoUploadOptions {
                model: llm_model,
                signal: options.signal,
            })
            .await
        }))
    }

    fn upload_image(
        &self,
        input: ImageUploadInput,
        options: MediaUploadOptions,
    ) -> Option<futures_util::future::BoxFuture<'static, Result<ContentPart, WireAsyncError>>> {
        let model = self.model.clone();
        let resolved = match self.resolve() {
            Ok(resolved) => resolved,
            Err(error) => return Some(Box::pin(async move { Err(error) })),
        };
        let uploader = resolved.media.as_ref().and_then(|media| media.upload_image.clone());
        Some(Box::pin(async move {
            let Some(uploader) = uploader else {
                return Err(Box::new(ImageUploadUnsupportedError::new(format!(
                    "Model \"{}\" (protocol={}) does not support image upload",
                    model.id, model.protocol
                ))) as WireAsyncError);
            };
            let credential = match &model.credential_provider {
                Some(provider) => provider.resolve().await?,
                None => None,
            };
            let llm_model = apply_credential(&resolved.model, credential);
            uploader(
                HumanImageUploadInput {
                    data: input.data,
                    mime_type: input.mime_type,
                    filename: input.filename,
                },
                MediaVideoUploadOptions {
                    model: llm_model,
                    signal: options.signal,
                },
            )
            .await
        }))
    }
}

struct ThrowToEventRequester {
    inner: Arc<dyn LlmRequester>,
}

#[async_trait]
impl LlmRequester for ThrowToEventRequester {
    async fn generate(
        &self,
        config: LlmRequestConfig,
        content: LlmRequestContent,
        control: LlmRequestControl,
    ) -> Result<(), WireAsyncError> {
        let on_event = control.on_event.clone();
        match self.inner.generate(config, content, control).await {
            Ok(()) => Ok(()),
            Err(error) if is_abort_error(error.as_ref()) => Err(error),
            Err(error) => {
                if let Some(message) = llm_message_from_error(error.as_ref()) {
                    on_event(LlmRequestEvent::FailedRemote {
                        error: message,
                        raw_error: None,
                    });
                    return Ok(());
                }
                match translate_provider_error(error) {
                    Ok(error) => Err(Box::new(error)),
                    Err(error) => Err(error),
                }
            }
        }
    }
}

pub fn carries_output_tokens(part: &StreamedMessagePart) -> bool {
    matches!(
        part,
        StreamedMessagePart::Content(ContentPart::Text { .. } | ContentPart::Think { .. })
            | StreamedMessagePart::ToolCallPart(_)
    )
}

fn wire_tools(tools: Vec<Tool>) -> Vec<Tool> {
    if !tools.iter().any(|tool| tool.deferred.is_some()) {
        return tools;
    }
    tools
        .into_iter()
        .filter(|tool| tool.deferred.is_none())
        .collect()
}

fn sampling_extra_params(
    protocol: Protocol,
    sampling: Option<&SamplingOptions>,
) -> Option<ExtraParams> {
    let sampling = sampling?;
    if sampling.temperature.is_none() && sampling.top_p.is_none() {
        return None;
    }
    let mut extra = ExtraParams::default();
    match protocol {
        Protocol::OpenAI => {
            extra.openai = Some(OpenAIExtraParams {
                temperature: sampling.temperature,
                top_p: sampling.top_p,
                ..Default::default()
            });
        }
        Protocol::OpenAIResponses => {
            extra.responses = Some(OpenAIResponsesExtraParams {
                temperature: sampling.temperature,
                top_p: sampling.top_p,
                ..Default::default()
            });
        }
        Protocol::Anthropic => {
            extra.anthropic = Some(AnthropicExtraParams {
                temperature: sampling.temperature,
                top_p: sampling.top_p,
                ..Default::default()
            });
        }
        Protocol::GoogleGenAI => {
            extra.google_genai = Some(GoogleGenAIExtraParams {
                temperature: sampling.temperature,
                top_p: sampling.top_p,
                ..Default::default()
            });
        }
        Protocol::Antigravity => return None,
    }
    Some(extra)
}

fn read_video_file(path: &str) -> Result<HumanVideoUploadInput, WireAsyncError> {
    let path_ref = Path::new(path);
    if !path_ref.exists() {
        return Err(Box::new(ChatProviderError::new(
            format!("Video file not found: {path}"),
            ProviderErrorCode::ApiError,
            true,
        )));
    }
    let filename = path_ref
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string();
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_lowercase())
        .unwrap_or_default();
    let mime_type = match extension.as_str() {
        "mp4" => "video/mp4",
        "mpeg" => "video/mpeg",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "flv" => "video/x-flv",
        "3gp" => "video/3gpp",
        _ => {
            return Err(Box::new(ChatProviderError::new(
                format!(
                    "KimiFiles.uploadVideo: file extension does not indicate a video type: {filename}"
                ),
                ProviderErrorCode::ApiError,
                true,
            )));
        }
    };
    let data = std::fs::read(path_ref)?;
    Ok(HumanVideoUploadInput {
        data,
        mime_type: mime_type.to_string(),
        filename: Some(filename),
    })
}

pub fn build_stream_timing(
    request_started_at: u64,
    request_sent_at: Option<u64>,
    first_chunk_at: u64,
    stream_ended_at: Option<u64>,
    decode_stats: Option<StreamDecodeStats>,
    output_parts: OutputPartTimes,
) -> ModelRequestTiming {
    let output_ended_at = stream_ended_at.unwrap_or_else(now_ms);
    let mut timing = ModelRequestTiming {
        first_token_latency_ms: first_chunk_at.saturating_sub(request_started_at),
        stream_duration_ms: output_ended_at.saturating_sub(first_chunk_at),
        llm_first_token_offset_ms: output_parts.first_token_at,
        llm_last_token_offset_ms: output_parts.last_token_at,
        llm_window_on_frame_clock: (output_parts.first_token_at.is_some()
            && output_parts.last_token_at.is_some())
        .then_some(true),
        ..Default::default()
    };
    if let Some(request_sent_at) = request_sent_at {
        let sent_at = request_sent_at
            .max(request_started_at)
            .min(first_chunk_at);
        timing.request_build_ms = Some(sent_at.saturating_sub(request_started_at));
        timing.server_first_token_ms = Some(first_chunk_at.saturating_sub(sent_at));
    }
    if let Some(decode_stats) = decode_stats {
        timing.server_decode_ms = Some(decode_stats.server_decode_ms);
        timing.client_consume_ms = Some(decode_stats.client_consume_ms);
        timing.client_blocked_ms = decode_stats.client_blocked_ms;
    }
    timing
}
