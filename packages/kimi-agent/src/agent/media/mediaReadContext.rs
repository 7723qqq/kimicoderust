use std::sync::Arc;
use std::time::Instant;

use serde_json::Value;

use crate::_base::utils::abort::abortable;
use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::profile::profile::AgentProfileService;
use crate::agent::tools::read_media_file::execute_media_read::{MediaReadContext, MediaTelemetry};
use crate::agent::tools::read_media_file::read_media_file::{VideoUploadOptions, VideoUploader};
use crate::app::telemetry::events::{VideoUploadEvent, VideoUploadOutcome};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::media::upload::VideoUploadInput;
use crate::llm_adapter::contract::errors::{VideoUploadUnsupportedError, chat_error_of};
use crate::llm_adapter::model::catalog::{IModelCatalog, Model};
use crate::llm_adapter::model::model_requester::{
    MediaUploadOptions, ModelRequester, VideoUploadSource,
};
use crate::llm_adapter::protocol::protocol::Protocol;
use crate::wire::record::WireAsyncError;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct VideoUploadTelemetryProps {
    pub model: Option<String>,
    pub provider_type: Option<String>,
    pub protocol: Option<String>,
}

#[derive(Clone)]
pub struct VideoUploadTelemetry {
    pub client: Arc<dyn ITelemetryService>,
    pub props: Option<VideoUploadTelemetryProps>,
}

pub fn create_video_uploader(
    requester: Option<Arc<dyn ModelRequester>>,
    telemetry: Option<VideoUploadTelemetry>,
) -> Option<VideoUploader> {
    let requester = requester?;
    Some(Arc::new(move |input: VideoUploadInput, options: VideoUploadOptions| {
        let requester = requester.clone();
        let telemetry = telemetry.clone();
        Box::pin(async move {
            let started_at = Instant::now();
            let mime_type = input.mime_type.clone();
            let size_bytes = input.data.len() as u64;
            let signal = options.signal.clone();
            let (upload_signal, cancellation_task) = signal.clone().map_or((None, None), |signal| {
                let token = CancellationToken::new();
                let task_token = token.clone();
                let task = tokio::spawn(async move {
                    signal.cancelled().await;
                    task_token.cancel();
                });
                (Some(token), Some(task))
            });
            let upload = requester.upload_video(
                VideoUploadSource::Upload(input),
                MediaUploadOptions {
                    signal: upload_signal,
                },
            );
            let upload = match upload {
                Some(upload) => upload,
                None => {
                    if let Some(task) = cancellation_task {
                        task.abort();
                    }
                    let error: WireAsyncError = Box::new(VideoUploadUnsupportedError::new(
                        "The current model does not support video upload",
                    ));
                    track_video_upload(
                        &telemetry,
                        mime_type,
                        size_bytes,
                        started_at,
                        VideoUploadOutcome::Error,
                        Some("VideoUploadUnsupportedError".to_string()),
                    );
                    return Err(error);
                }
            };
            let result = if let Some(signal) = signal {
                match abortable(upload, signal).await {
                    Ok(result) => result,
                    Err(error) => Err(Box::new(error) as WireAsyncError),
                }
            } else {
                upload.await
            };
            if let Some(task) = cancellation_task {
                task.abort();
            }
            match &result {
                Ok(_) => track_video_upload(
                    &telemetry,
                    mime_type,
                    size_bytes,
                    started_at,
                    VideoUploadOutcome::Success,
                    None,
                ),
                Err(error) => {
                    let error_type = chat_error_of(error.as_ref())
                        .map(|error| error.name.clone())
                        .or_else(|| Some("Error".to_string()));
                    track_video_upload(
                        &telemetry,
                        mime_type,
                        size_bytes,
                        started_at,
                        VideoUploadOutcome::Error,
                        error_type,
                    );
                }
            }
            result
        })
    }))
}

fn track_video_upload(
    telemetry: &Option<VideoUploadTelemetry>,
    mime_type: String,
    size_bytes: u64,
    started_at: Instant,
    outcome: VideoUploadOutcome,
    error_type: Option<String>,
) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let props = telemetry.props.clone().unwrap_or_default();
    let event = VideoUploadEvent {
        model: props.model,
        provider_type: props.provider_type,
        protocol: props.protocol,
        mime_type,
        size_bytes,
        outcome,
        duration_ms: started_at.elapsed().as_millis() as u64,
        error_type,
    };
    if let Ok(Value::Object(properties)) = serde_json::to_value(event) {
        telemetry.client.track2("video_upload", Some(properties));
    }
}

pub struct MediaReadContextService {
    profile: Arc<dyn AgentProfileService>,
    model_catalog: Arc<dyn IModelCatalog>,
    runtime: Arc<dyn IAgentEnvironmentService>,
    telemetry: Arc<dyn ITelemetryService>,
}

impl MediaReadContextService {
    pub fn new(
        profile: Arc<dyn AgentProfileService>,
        model_catalog: Arc<dyn IModelCatalog>,
        runtime: Arc<dyn IAgentEnvironmentService>,
        telemetry: Arc<dyn ITelemetryService>,
    ) -> Self {
        Self {
            profile,
            model_catalog,
            runtime,
            telemetry,
        }
    }

    pub fn get_media_read_context(&self) -> Option<MediaReadContext> {
        if !self.runtime.is_available(&["fs".to_string()]) {
            return None;
        }
        let model_alias = self.profile.get_model();
        let mut video_uploader = None;
        let mut model: Option<Model> = None;
        if !model_alias.is_empty() {
            if let Ok(requester) = self.model_catalog.get_requester(&model_alias) {
                model = Some(requester.model().clone());
                video_uploader = create_video_uploader(
                    Some(requester),
                    Some(VideoUploadTelemetry {
                        client: self.telemetry.clone(),
                        props: Some(VideoUploadTelemetryProps {
                            model: Some(model_alias),
                            provider_type: model
                                .as_ref()
                                .and_then(|model| model.provider_type.clone())
                                .or_else(|| model.as_ref().map(|model| model.protocol.to_string())),
                            protocol: model.as_ref().map(|model| model.protocol.to_string()),
                        }),
                    }),
                );
            } else {
                model = None;
            }
        }
        let telemetry = self.telemetry.clone();
        let media_telemetry: MediaTelemetry = Arc::new(move |event, value| {
            telemetry.track2(event, value.as_object().cloned());
        });
        Some(MediaReadContext {
            capabilities: self.profile.get_model_capabilities(),
            video_uploader,
            inline_video_supported: model.as_ref().is_none_or(|model| {
                !matches!(model.protocol, Protocol::OpenAI | Protocol::OpenAIResponses)
            }),
            provider_type: model.and_then(|model| model.provider_type),
            telemetry: Some(media_telemetry),
        })
    }
}
