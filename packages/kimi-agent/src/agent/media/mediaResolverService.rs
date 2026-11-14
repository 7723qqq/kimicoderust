use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use base64::Engine as _;
use futures_util::StreamExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::_base::utils::abort::{AbortSignal, abort_error, abortable};
use crate::agent::profile::profileOps::{WarningIssued, WarningIssuedPayload};
use crate::agent::state::agent_state::StateKey;
use crate::agent::state::agent_state_service::{AgentStateService, StateRegistration};
use crate::app::file::file_service::IFileService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::media::path_tag::MediaPathTagKind;
use crate::human::llm::message::{ContentPart, ImageUrl, VideoUrl};
use crate::human::llm::requester::requester::LlmCredentialProvider;
use crate::llm_adapter::contract::errors::ImageUploadUnsupportedError;
use crate::llm_adapter::contract::message::Message;
use crate::llm_adapter::model::catalog::Model;
use crate::llm_adapter::model::model_requester::{
    ImageUploadInput, MediaUploadOptions, ModelRequester,
};
use crate::persistence::interface::blobStore::{BlobByteStream, IBlobStore};
use crate::state::eventDispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

use super::file_type::{DetectFileTypeMode, FileKind, MEDIA_SNIFF_BYTES, detect_file_type};
use super::image_format_policy::{is_data_url, is_model_accepted_image_mime, normalize_image_mime};
use super::media_read_context::{
    VideoUploadTelemetry, VideoUploadTelemetryProps, create_video_uploader,
};
use super::media_ref::{
    DaemonFileMediaKind, DaemonFileRef, build_media_path_tag, daemon_file_ref_from_part,
    match_single_media_path_tag, parse_daemon_file_url,
};
use super::media_resolver::IAgentMediaResolverService;
use super::session_media_store::ISessionMediaStore;
use super::video_upload::{
    inline_video_part, inline_video_supported_for_protocol, is_media_upload_auth_error,
    is_video_upload_unsupported_error,
};

const VIDEO_CACHE_SCOPE: &str = "video-upload-cache";
const IMAGE_CACHE_SCOPE: &str = "image-upload-cache";
const VIDEO_UNAVAILABLE_TEXT: &str = "[video omitted: the uploaded file is no longer available]";
const IMAGE_UNAVAILABLE_TEXT: &str = "[image omitted: the uploaded file is no longer available]";
const IMAGE_MEMO_MAX_BYTES: usize = 8 * 1024 * 1024;
const IMAGE_MEMO_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const REQUEST_MEDIA_BUDGET_BYTES: usize = 20 * 1024 * 1024;
const REQUEST_MEDIA_BUDGET_LOW_BYTES: usize = 10 * 1024 * 1024;

static PROVIDER_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());
static MEDIA_RESOLVED_KEY: LazyLock<StateKey> = LazyLock::new(|| {
    StateKey::new(
        "media.resolved",
        Arc::new(|| Value::Object(Map::new())),
    )
});
static MEDIA_BUDGET_DROPPED_KEY: LazyLock<StateKey> = LazyLock::new(|| {
    StateKey::new("media.budgetDropped", Arc::new(|| Value::Array(Vec::new())))
});

pub fn media_resolved_key() -> &'static StateKey {
    &MEDIA_RESOLVED_KEY
}

pub fn media_budget_dropped_key() -> &'static StateKey {
    &MEDIA_BUDGET_DROPPED_KEY
}

#[derive(Debug, Clone)]
struct MediaBudgetEntry {
    message_index: usize,
    part_index: usize,
    key: String,
    file_id: Option<String>,
    kind: DaemonFileMediaKind,
    bytes: usize,
}

#[derive(Clone)]
struct ImageMemoEntry {
    part: ContentPart,
    bytes: usize,
    mime_type: String,
}

#[derive(Default)]
struct ImageMemo {
    entries: IndexMap<String, ImageMemoEntry>,
    bytes: usize,
}

pub struct AgentMediaResolverService {
    files: Arc<dyn IFileService>,
    blobs: Arc<dyn IBlobStore>,
    telemetry: Arc<dyn ITelemetryService>,
    states: Arc<AgentStateService>,
    media_store: Arc<dyn ISessionMediaStore>,
    dispatcher: Arc<dyn IEventDispatcher>,
    agent_id: String,
    image_memo: Mutex<ImageMemo>,
    image_upload_unsupported: Mutex<HashSet<String>>,
    _resolved_registration: StateRegistration,
    _budget_registration: StateRegistration,
}

impl AgentMediaResolverService {
    pub fn new(
        files: Arc<dyn IFileService>,
        blobs: Arc<dyn IBlobStore>,
        telemetry: Arc<dyn ITelemetryService>,
        states: Arc<AgentStateService>,
        media_store: Arc<dyn ISessionMediaStore>,
        dispatcher: Arc<dyn IEventDispatcher>,
        agent_id: String,
    ) -> Result<Self, crate::agent::state::agent_state::StateError> {
        let resolved_registration = states.contribute_state(MEDIA_RESOLVED_KEY.clone())?;
        let budget_registration = states.contribute_state(MEDIA_BUDGET_DROPPED_KEY.clone())?;
        Ok(Self {
            files,
            blobs,
            telemetry,
            states,
            media_store,
            dispatcher,
            agent_id,
            image_memo: Mutex::new(ImageMemo::default()),
            image_upload_unsupported: Mutex::new(HashSet::new()),
            _resolved_registration: resolved_registration,
            _budget_registration: budget_registration,
        })
    }

    fn resolved(&self) -> Result<HashMap<String, ContentPart>, WireAsyncError> {
        Ok(serde_json::from_value(
            self.states.get(media_resolved_key())?,
        )?)
    }

    fn set_resolved(&self, value: HashMap<String, ContentPart>) -> Result<(), WireAsyncError> {
        self.states
            .set(media_resolved_key(), serde_json::to_value(value)?)?;
        Ok(())
    }

    fn resolved_part(&self, key: &str) -> Result<Option<ContentPart>, WireAsyncError> {
        Ok(self.resolved()?.remove(key))
    }

    fn cache_resolved_part(&self, key: String, part: ContentPart) -> Result<(), WireAsyncError> {
        let mut resolved = self.resolved()?;
        resolved.insert(key, part);
        self.set_resolved(resolved)
    }

    fn budget_dropped(&self) -> Result<HashSet<String>, WireAsyncError> {
        Ok(serde_json::from_value(
            self.states.get(media_budget_dropped_key())?,
        )?)
    }

    fn set_budget_dropped(&self, value: HashSet<String>) -> Result<(), WireAsyncError> {
        self.states
            .set(media_budget_dropped_key(), serde_json::to_value(value)?)?;
        Ok(())
    }

    async fn display_path(&self, ref_: &DaemonFileRef) -> Result<Option<String>, WireAsyncError> {
        self.media_store.resolve_display_path(&ref_.file_id).await
    }

    async fn apply_media_budget(
        &self,
        out: &mut [Message],
        entries: &[MediaBudgetEntry],
    ) -> Result<bool, WireAsyncError> {
        if entries.is_empty() {
            return Ok(false);
        }
        let mut changed = false;
        let mut dropped = self.budget_dropped()?;
        let mut pending = IndexMap::<String, usize>::new();
        for entry in entries {
            if dropped.contains(&entry.key) {
                self.replace_with_media_tag(out, entry).await?;
                changed = true;
            } else {
                *pending.entry(entry.key.clone()).or_default() += entry.bytes;
            }
        }
        let mut total = pending.values().sum::<usize>();
        if total <= REQUEST_MEDIA_BUDGET_BYTES {
            return Ok(changed);
        }
        let mut dropped_now = HashSet::new();
        for (key, bytes) in pending {
            if total <= REQUEST_MEDIA_BUDGET_LOW_BYTES {
                break;
            }
            if bytes == 0 {
                continue;
            }
            dropped.insert(key.clone());
            dropped_now.insert(key);
            total -= bytes;
        }
        self.set_budget_dropped(dropped)?;
        for entry in entries {
            if dropped_now.contains(&entry.key) {
                self.replace_with_media_tag(out, entry).await?;
            }
        }
        let has_untracked_inline_media = entries.iter().any(|entry| {
            dropped_now.contains(&entry.key) && entry.file_id.is_none()
        });
        let message = format!(
            "Conversation media exceeded the {} MB per-request budget; {} older media item(s) were omitted{}",
            REQUEST_MEDIA_BUDGET_BYTES / (1024 * 1024),
            dropped_now.len(),
            if has_untracked_inline_media {
                "."
            } else {
                " and remain available at their saved paths."
            }
        );
        let dispatcher = self.dispatcher.clone();
        let event = WarningIssued::new(WarningIssuedPayload {
            agent_id: self.agent_id.clone(),
            code: Some("media-budget-exceeded".to_string()),
            message,
        });
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
        Ok(true)
    }

    async fn replace_with_media_tag(
        &self,
        out: &mut [Message],
        entry: &MediaBudgetEntry,
    ) -> Result<(), WireAsyncError> {
        let Some(message) = out.get(entry.message_index).cloned() else {
            return Ok(());
        };
        let mut content = message.content.clone();
        let Some(slot) = content.get_mut(entry.part_index) else {
            return Ok(());
        };
        *slot = if let Some(file_id) = &entry.file_id {
            let path = self
                .display_path(&DaemonFileRef {
                    file_id: file_id.clone(),
                })
                .await?;
            match entry.kind {
                DaemonFileMediaKind::Video => video_tag(path),
                DaemonFileMediaKind::Image => degraded_image(path),
            }
        } else {
            budget_omitted_media(entry.kind)
        };
        out[entry.message_index] = Message { content, ..message };
        Ok(())
    }

    async fn resolve_image_part(
        &self,
        ref_: &DaemonFileRef,
        requester: Arc<dyn ModelRequester>,
        signal: Option<AbortSignal>,
    ) -> Result<ContentPart, WireAsyncError> {
        let model = requester.model().clone();
        if !model.capabilities.image_in {
            self.track_fallback("image", "unsupported", &model.name);
            return Ok(degraded_image(self.display_path(ref_).await?));
        }
        let provider_key = model
            .provider_type
            .clone()
            .unwrap_or_else(|| model.protocol.to_string());
        let uploader_available = !self.image_upload_unsupported.lock().contains(&provider_key);
        let inline_key = format!("image\0{}", ref_.file_id);
        if !uploader_available {
            if let Some(memoed) = self.memoed_image(&inline_key, model.provider_type.as_deref()) {
                return Ok(memoed);
            }
            return self
                .resolve_image_uncached(ref_, requester, inline_key, None, signal)
                .await;
        }
        let cache_key = format!(
            "image\0{}\0{}\0{}\0{}\0{}",
            ref_.file_id,
            provider_key,
            model.protocol,
            model.base_url.as_deref().unwrap_or(""),
            account_hash_for(&model).await
        );
        if let Some(memoed) = self.resolved_part(&cache_key)? {
            return Ok(memoed);
        }
        if let Some(cached_llm_file_id) = self.read_cached_upload(IMAGE_CACHE_SCOPE, &cache_key).await
        {
            let part = ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("ms://{cached_llm_file_id}"),
                    id: Some(cached_llm_file_id),
                    name: None,
                    dimensions: None,
                    detail: None,
                },
            };
            self.cache_resolved_part(cache_key, part.clone())?;
            return Ok(part);
        }
        self.resolve_image_uncached(ref_, requester, inline_key, Some(cache_key), signal)
            .await
    }

    async fn resolve_image_uncached(
        &self,
        ref_: &DaemonFileRef,
        requester: Arc<dyn ModelRequester>,
        inline_key: String,
        upload_cache_key: Option<String>,
        signal: Option<AbortSignal>,
    ) -> Result<ContentPart, WireAsyncError> {
        let model = requester.model().clone();
        let path = self.display_path(ref_).await?;
        let source = match self.read_media(ref_, signal.clone()).await {
            Ok(source) => source,
            Err(error) => {
                if signal.as_ref().is_some_and(AbortSignal::aborted) {
                    return Err(error);
                }
                self.track_fallback("image", "read_failed", &model.name);
                return Ok(degraded_image(path));
            }
        };
        let header = &source.bytes[..source.bytes.len().min(MEDIA_SNIFF_BYTES)];
        let file_type = detect_file_type(&source.filename, Some(header), DetectFileTypeMode::Media);
        let mime_type = normalize_image_mime(&file_type.mime_type);
        if file_type.kind != FileKind::Image
            || !is_model_accepted_image_mime(&mime_type, model.provider_type.as_deref())
        {
            self.track_fallback("image", "invalid", &model.name);
            return Ok(degraded_image(path));
        }
        if let Some(cache_key) = upload_cache_key {
            if let Some(uploaded) = self
                .upload_image_part(requester, &source, &mime_type, cache_key, signal)
                .await?
            {
                return Ok(uploaded);
            }
        }
        let part = ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!(
                    "data:{mime_type};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&source.bytes)
                ),
                id: None,
                name: None,
                dimensions: None,
                detail: None,
            },
        };
        if source.bytes.len() <= IMAGE_MEMO_MAX_BYTES {
            self.memoize_image(inline_key, part.clone(), source.bytes.len(), mime_type);
        }
        Ok(part)
    }

    async fn upload_image_part(
        &self,
        requester: Arc<dyn ModelRequester>,
        source: &MediaSource,
        mime_type: &str,
        cache_key: String,
        signal: Option<AbortSignal>,
    ) -> Result<Option<ContentPart>, WireAsyncError> {
        let model = requester.model().clone();
        let source = source.clone();
        let mime_type = mime_type.to_string();
        let uploaded = run_with_credential_recovery_abort(
            model.credential_provider.as_ref(),
            {
                let requester = requester.clone();
                let source = source.clone();
                let mime_type = mime_type.clone();
                let signal = signal.clone();
                move || {
                    let requester = requester.clone();
                    let source = source.clone();
                    let mime_type = mime_type.clone();
                    let signal = signal.clone();
                    async move {
                        let (upload_options, cancellation_task) = media_upload_options(signal.clone());
                        let upload = requester.upload_image(
                            ImageUploadInput {
                                data: source.bytes.clone(),
                                mime_type,
                                filename: Some(source.filename.clone()),
                            },
                            upload_options,
                        );
                        let Some(upload) = upload else {
                            if let Some(task) = cancellation_task {
                                task.abort();
                            }
                            return Err(Box::new(ImageUploadUnsupportedError::new(
                                "The current model does not support image upload",
                            )) as WireAsyncError);
                        };
                        let result = with_abort(upload, signal).await;
                        if let Some(task) = cancellation_task {
                            task.abort();
                        }
                        result
                    }
                }
            },
            signal.as_ref(),
        )
        .await;
        match uploaded {
            Ok(part @ ContentPart::ImageUrl { .. }) => {
                let ContentPart::ImageUrl { image_url } = &part else {
                    unreachable!()
                };
                let llm_file_id = image_url
                    .id
                    .clone()
                    .or_else(|| ms_file_id_from_url(&image_url.url));
                if let Some(llm_file_id) = llm_file_id {
                    self.write_cached_upload(IMAGE_CACHE_SCOPE, &cache_key, &llm_file_id)
                        .await;
                }
                self.cache_resolved_part(cache_key, part.clone())?;
                Ok(Some(part))
            }
            Ok(_) => Ok(None),
            Err(error) => {
                if signal.as_ref().is_some_and(AbortSignal::aborted)
                    || is_media_upload_auth_error(error.as_ref())
                {
                    return Err(error);
                }
                if error.downcast_ref::<ImageUploadUnsupportedError>().is_some() {
                    self.image_upload_unsupported.lock().insert(
                        model
                            .provider_type
                            .clone()
                            .unwrap_or_else(|| model.protocol.to_string()),
                    );
                }
                self.track_fallback("image", "upload_failed", &model.name);
                Ok(None)
            }
        }
    }

    fn memoed_image(&self, cache_key: &str, provider_type: Option<&str>) -> Option<ContentPart> {
        let mut memo = self.image_memo.lock();
        let entry = memo.entries.get(cache_key)?.clone();
        if !is_model_accepted_image_mime(&entry.mime_type, provider_type) {
            return None;
        }
        memo.entries.shift_remove(cache_key);
        memo.entries.insert(cache_key.to_string(), entry.clone());
        Some(entry.part)
    }

    fn memoize_image(
        &self,
        cache_key: String,
        part: ContentPart,
        bytes: usize,
        mime_type: String,
    ) {
        let mut memo = self.image_memo.lock();
        if let Some(previous) = memo.entries.shift_remove(&cache_key) {
            memo.bytes = memo.bytes.saturating_sub(previous.bytes);
        }
        memo.entries.insert(
            cache_key,
            ImageMemoEntry {
                part,
                bytes,
                mime_type,
            },
        );
        memo.bytes += bytes;
        while memo.bytes > IMAGE_MEMO_MAX_TOTAL_BYTES {
            let Some((_, entry)) = memo.entries.shift_remove_index(0) else {
                break;
            };
            memo.bytes = memo.bytes.saturating_sub(entry.bytes);
        }
    }

    async fn resolve_video_part(
        &self,
        ref_: &DaemonFileRef,
        requester: Arc<dyn ModelRequester>,
        signal: Option<AbortSignal>,
    ) -> Result<ContentPart, WireAsyncError> {
        let model = requester.model().clone();
        if !model.capabilities.video_in {
            return Ok(video_tag(self.display_path(ref_).await?));
        }
        let provider_key = model
            .provider_type
            .clone()
            .unwrap_or_else(|| model.protocol.to_string());
        let cache_key = format!(
            "{}\0{}\0{}\0{}\0{}",
            ref_.file_id,
            provider_key,
            model.protocol,
            model.base_url.as_deref().unwrap_or(""),
            account_hash_for(&model).await
        );
        if let Some(memoed) = self.resolved_part(&cache_key)? {
            return self.memoed_outcome(ref_, memoed).await;
        }
        let (part, memoize) = self
            .resolve_video_uncached(ref_, requester, &cache_key, signal)
            .await?;
        if memoize {
            self.cache_resolved_part(cache_key, part.clone())?;
        }
        Ok(part)
    }

    async fn memoed_outcome(
        &self,
        ref_: &DaemonFileRef,
        memoed: ContentPart,
    ) -> Result<ContentPart, WireAsyncError> {
        let ContentPart::Text { text, .. } = &memoed else {
            return Ok(memoed);
        };
        let Some(tag) = match_single_media_path_tag(text) else {
            return Ok(memoed);
        };
        let path = self.display_path(ref_).await?;
        if path.is_none() || path.as_deref() == Some(tag.path.as_str()) {
            return Ok(memoed);
        }
        Ok(ContentPart::Text {
            text: build_media_path_tag(tag.kind, &path.unwrap()),
            meta: None,
        })
    }

    async fn resolve_video_uncached(
        &self,
        ref_: &DaemonFileRef,
        requester: Arc<dyn ModelRequester>,
        cache_key: &str,
        signal: Option<AbortSignal>,
    ) -> Result<(ContentPart, bool), WireAsyncError> {
        if let Some(cached_llm_file_id) = self.read_cached_upload(VIDEO_CACHE_SCOPE, cache_key).await
        {
            return Ok((
                ContentPart::VideoUrl {
                    video_url: VideoUrl {
                        url: format!("ms://{cached_llm_file_id}"),
                        id: Some(cached_llm_file_id),
                        name: None,
                    },
                },
                true,
            ));
        }
        let tag_path = self.display_path(ref_).await?;
        let source = match self.read_media(ref_, signal.clone()).await {
            Ok(source) => source,
            Err(error) => {
                if signal.as_ref().is_some_and(AbortSignal::aborted) {
                    return Err(error);
                }
                return Ok((video_tag(tag_path), true));
            }
        };
        let header = &source.bytes[..source.bytes.len().min(MEDIA_SNIFF_BYTES)];
        let file_type = detect_file_type(&source.filename, Some(header), DetectFileTypeMode::Media);
        if file_type.kind != FileKind::Video {
            return Ok((video_tag(tag_path), true));
        }
        let mime_type = file_type.mime_type;
        let model = requester.model().clone();
        let inline_supported = inline_video_supported_for_protocol(model.protocol);
        let uploader = create_video_uploader(
            Some(requester.clone()),
            Some(VideoUploadTelemetry {
                client: self.telemetry.clone(),
                props: Some(VideoUploadTelemetryProps {
                    model: Some(model.name.clone()),
                    provider_type: Some(
                        model
                            .provider_type
                            .clone()
                            .unwrap_or_else(|| model.protocol.to_string()),
                    ),
                    protocol: Some(model.protocol.to_string()),
                }),
            }),
        );
        let Some(uploader) = uploader else {
            return Ok((
                if inline_supported {
                    inline_video_part(&source.bytes, &mime_type)
                } else {
                    video_tag(tag_path)
                },
                true,
            ));
        };
        let uploaded = run_with_credential_recovery_abort(
            model.credential_provider.as_ref(),
            {
                let source = source.clone();
                let mime_type = mime_type.clone();
                let signal = signal.clone();
                let uploader = uploader.clone();
                move || {
                    let source = source.clone();
                    let mime_type = mime_type.clone();
                    let signal = signal.clone();
                    let uploader = uploader.clone();
                    async move {
                        uploader(
                            crate::agent::tools::read_media_file::read_media_file::VideoUploadInput {
                                data: source.bytes.clone(),
                                mime_type,
                                filename: Some(source.filename.clone()),
                            },
                            crate::agent::tools::read_media_file::read_media_file::VideoUploadOptions {
                                signal,
                            },
                        )
                        .await
                    }
                }
            },
            signal.as_ref(),
        )
        .await;
        match uploaded {
            Ok(part @ ContentPart::VideoUrl { .. }) => {
                let ContentPart::VideoUrl { video_url } = &part else {
                    unreachable!()
                };
                let llm_file_id = video_url
                    .id
                    .clone()
                    .or_else(|| ms_file_id_from_url(&video_url.url));
                if let Some(llm_file_id) = llm_file_id {
                    self.write_cached_upload(VIDEO_CACHE_SCOPE, cache_key, &llm_file_id)
                        .await;
                }
                Ok((part, true))
            }
            Ok(_) => Ok((video_tag(tag_path), false)),
            Err(error) => {
                if signal.as_ref().is_some_and(AbortSignal::aborted)
                    || is_media_upload_auth_error(error.as_ref())
                {
                    return Err(error);
                }
                self.track_fallback("video", "upload_failed", &model.name);
                if is_video_upload_unsupported_error(error.as_ref()) {
                    return Ok((
                        if inline_supported {
                            inline_video_part(&source.bytes, &mime_type)
                        } else {
                            video_tag(tag_path)
                        },
                        true,
                    ));
                }
                Ok((video_tag(tag_path), false))
            }
        }
    }

    async fn read_media(
        &self,
        ref_: &DaemonFileRef,
        signal: Option<AbortSignal>,
    ) -> Result<MediaSource, WireAsyncError> {
        let primary = async {
            throw_if_aborted(signal.as_ref())?;
            let file = self.files.get(&ref_.file_id).await?;
            let bytes = read_stream(file.stream(None), signal.clone()).await?;
            Ok(MediaSource {
                bytes,
                filename: file.meta.name,
            })
        }
        .await;
        match primary {
            Ok(source) => Ok(source),
            Err(error) => {
                if signal.as_ref().is_some_and(AbortSignal::aborted) {
                    return Err(error);
                }
                let canonical = self.media_store.read(&ref_.file_id).await?;
                let Some(canonical) = canonical else {
                    return Err(Box::new(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!("media {} is unavailable", ref_.file_id),
                    )));
                };
                Ok(MediaSource {
                    bytes: canonical.data,
                    filename: canonical.name,
                })
            }
        }
    }

    async fn read_cached_upload(&self, scope: &str, cache_key: &str) -> Option<String> {
        let data = self.blobs.get(scope, &blob_key(cache_key)).await.ok().flatten()?;
        let llm_file_id = String::from_utf8(data).ok()?;
        PROVIDER_ID_RE.is_match(&llm_file_id).then_some(llm_file_id)
    }

    async fn write_cached_upload(&self, scope: &str, cache_key: &str, llm_file_id: &str) {
        if !PROVIDER_ID_RE.is_match(llm_file_id) {
            return;
        }
        let _ = self
            .blobs
            .put(scope, &blob_key(cache_key), llm_file_id.as_bytes().to_vec())
            .await;
    }

    fn track_fallback(&self, kind: &str, reason: &str, model: &str) {
        self.telemetry.track2(
            "media_resolve_fallback",
            Some(Map::from_iter([
                ("kind".to_string(), Value::String(kind.to_string())),
                ("reason".to_string(), Value::String(reason.to_string())),
                ("model".to_string(), Value::String(model.to_string())),
            ])),
        );
    }
}

#[async_trait]
impl IAgentMediaResolverService for AgentMediaResolverService {
    async fn resolve(
        &self,
        messages: &[Message],
        requester: Arc<dyn ModelRequester>,
        signal: Option<AbortSignal>,
    ) -> Result<Vec<Message>, WireAsyncError> {
        let mut changed = false;
        let mut out = Vec::<Message>::new();
        let mut budget_entries = Vec::new();
        for message in messages {
            let mut content = Vec::new();
            let mut message_changed = false;
            let mut saw_video_ref = false;
            for part in &message.content {
                let daemon_part = daemon_file_ref_from_part(part);
                let Some(daemon_part) = daemon_part else {
                    if let Some(entry) = inline_media_budget_entry(part, out.len(), content.len()) {
                        budget_entries.push(entry);
                    }
                    content.push(part.clone());
                    continue;
                };
                message_changed = true;
                saw_video_ref |= daemon_part.kind == DaemonFileMediaKind::Video;
                let resolved = match daemon_part.kind {
                    DaemonFileMediaKind::Video => {
                        self.resolve_video_part(&daemon_part.ref_, requester.clone(), signal.clone())
                            .await?
                    }
                    DaemonFileMediaKind::Image => {
                        self.resolve_image_part(&daemon_part.ref_, requester.clone(), signal.clone())
                            .await?
                    }
                };
                budget_entries.push(MediaBudgetEntry {
                    message_index: out.len(),
                    part_index: content.len(),
                    key: daemon_part.ref_.file_id.clone(),
                    file_id: Some(daemon_part.ref_.file_id.clone()),
                    kind: daemon_part.kind,
                    bytes: inline_part_bytes(&resolved),
                });
                content.push(resolved);
            }
            out.push(if message_changed {
                Message {
                    content: if content.is_empty() {
                        vec![unavailable_media_text(if saw_video_ref {
                            DaemonFileMediaKind::Video
                        } else {
                            DaemonFileMediaKind::Image
                        })]
                    } else {
                        content
                    },
                    ..message.clone()
                }
            } else {
                message.clone()
            });
            changed |= message_changed;
        }
        changed = self.apply_media_budget(&mut out, &budget_entries).await? || changed;
        Ok(if changed { out } else { messages.to_vec() })
    }

    async fn display_paths(
        &self,
        messages: &[Message],
    ) -> Result<HashMap<String, String>, WireAsyncError> {
        let mut paths = HashMap::new();
        for message in messages {
            for part in &message.content {
                let url = match part {
                    ContentPart::ImageUrl { image_url } => &image_url.url,
                    ContentPart::VideoUrl { video_url } => &video_url.url,
                    _ => continue,
                };
                let Some(ref_) = parse_daemon_file_url(url) else {
                    continue;
                };
                if paths.contains_key(url) {
                    continue;
                }
                if let Some(path) = self.media_store.resolve_display_path(&ref_.file_id).await? {
                    paths.insert(url.clone(), path);
                }
            }
        }
        Ok(paths)
    }
}

#[derive(Debug, Clone)]
struct MediaSource {
    bytes: Vec<u8>,
    filename: String,
}

async fn account_hash_for(model: &Model) -> String {
    let mut identity = None;
    if let Some(authorization) = model.headers.get("Authorization") {
        identity = Some(format!("authorization\0{}", authorization.trim()));
    } else if let Some(provider) = &model.credential_provider {
        if let Ok(credential) = provider.resolve().await {
            if let Some(api_key) = credential.and_then(|credential| credential.api_key) {
                if !api_key.is_empty() {
                    identity = Some(format!(
                        "api-key\0{}",
                        stable_jwt_subject(&api_key).unwrap_or(api_key)
                    ));
                }
            }
        }
    }
    let Some(identity) = identity else {
        return "no-key".to_string();
    };
    format!("{:x}", Sha256::digest(identity.as_bytes()))[..16].to_string()
}

fn stable_jwt_subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let payload = parts.nth(1)?;
    parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let value: Value = serde_json::from_slice(&decoded).ok()?;
    for key in ["sub", "user_id"] {
        if let Some(text) = value.get(key).and_then(Value::as_str)
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
    }
    None
}

fn inline_media_budget_entry(
    part: &ContentPart,
    message_index: usize,
    part_index: usize,
) -> Option<MediaBudgetEntry> {
    let (kind, url) = match part {
        ContentPart::ImageUrl { image_url } => (DaemonFileMediaKind::Image, &image_url.url),
        ContentPart::VideoUrl { video_url } => (DaemonFileMediaKind::Video, &video_url.url),
        _ => return None,
    };
    if !is_data_url(url) {
        return None;
    }
    let kind_text = match kind {
        DaemonFileMediaKind::Image => "image",
        DaemonFileMediaKind::Video => "video",
    };
    let mut hasher = Sha256::new();
    hasher.update(kind_text);
    hasher.update("\0");
    hasher.update(url);
    let hash = format!("{:x}", hasher.finalize());
    Some(MediaBudgetEntry {
        message_index,
        part_index,
        key: format!("inline\0{hash}"),
        file_id: None,
        kind,
        bytes: url.encode_utf16().count(),
    })
}

fn inline_part_bytes(part: &ContentPart) -> usize {
    match part {
        ContentPart::ImageUrl { image_url } if is_data_url(&image_url.url) => {
            image_url.url.encode_utf16().count()
        }
        ContentPart::VideoUrl { video_url } if is_data_url(&video_url.url) => {
            video_url.url.encode_utf16().count()
        }
        _ => 0,
    }
}

fn budget_omitted_media(kind: DaemonFileMediaKind) -> ContentPart {
    let kind = match kind {
        DaemonFileMediaKind::Image => "image",
        DaemonFileMediaKind::Video => "video",
    };
    ContentPart::Text {
        text: format!("[{kind} omitted: dropped to fit the request media budget]"),
        meta: None,
    }
}

fn degraded_image(path: Option<String>) -> ContentPart {
    match path {
        Some(path) => ContentPart::Text {
            text: build_media_path_tag(MediaPathTagKind::Image, &path),
            meta: None,
        },
        None => unavailable_media_text(DaemonFileMediaKind::Image),
    }
}

fn unavailable_media_text(kind: DaemonFileMediaKind) -> ContentPart {
    ContentPart::Text {
        text: match kind {
            DaemonFileMediaKind::Image => IMAGE_UNAVAILABLE_TEXT,
            DaemonFileMediaKind::Video => VIDEO_UNAVAILABLE_TEXT,
        }
        .to_string(),
        meta: None,
    }
}

fn video_tag(path: Option<String>) -> ContentPart {
    match path {
        Some(path) => ContentPart::Text {
            text: build_media_path_tag(MediaPathTagKind::Video, &path),
            meta: None,
        },
        None => ContentPart::Text {
            text: VIDEO_UNAVAILABLE_TEXT.to_string(),
            meta: None,
        },
    }
}

fn ms_file_id_from_url(url: &str) -> Option<String> {
    let id = url.strip_prefix("ms://")?;
    (!id.is_empty()).then(|| id.to_string())
}

fn blob_key(cache_key: &str) -> String {
    format!("{:x}", Sha256::digest(cache_key.as_bytes()))
}

async fn read_stream(
    stream: BlobByteStream,
    signal: Option<AbortSignal>,
) -> Result<Vec<u8>, WireAsyncError> {
    let read = async move {
        let mut stream = stream;
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend(chunk?);
        }
        Ok(bytes)
    };
    with_abort(read, signal).await
}

fn media_upload_options(signal: Option<AbortSignal>) -> (MediaUploadOptions, Option<JoinHandle<()>>) {
    let Some(signal) = signal else {
        return (MediaUploadOptions::default(), None);
    };
    let token = CancellationToken::new();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        signal.cancelled().await;
        task_token.cancel();
    });
    (MediaUploadOptions { signal: Some(token) }, Some(task))
}

async fn with_abort<T, F>(future: F, signal: Option<AbortSignal>) -> Result<T, WireAsyncError>
where
    T: Send + 'static,
    F: Future<Output = Result<T, WireAsyncError>> + Send + 'static,
{
    if let Some(signal) = signal {
        match abortable(future, signal).await {
            Ok(result) => result,
            Err(error) => Err(Box::new(error) as WireAsyncError),
        }
    } else {
        future.await
    }
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal.filter(|signal| signal.aborted()) {
        let reason = signal.reason().unwrap_or_else(|| Arc::new(abort_error()));
        Err(Box::new(reason) as WireAsyncError)
    } else {
        Ok(())
    }
}

async fn run_with_credential_recovery_abort<T, F, Fut>(
    credential_provider: Option<&Arc<dyn LlmCredentialProvider>>,
    mut run: F,
    signal: Option<&AbortSignal>,
) -> Result<T, WireAsyncError>
where
    T: Send,
    F: FnMut() -> Fut + Send,
    Fut: Future<Output = Result<T, WireAsyncError>> + Send,
{
    match run().await {
        Ok(value) => Ok(value),
        Err(error) => {
            if signal.is_some_and(AbortSignal::aborted)
                || !credential_provider.is_some_and(|provider| provider.can_recover(&error))
            {
                return Err(error);
            }
            if let Some(provider) = credential_provider {
                provider.invalidate();
            }
            run().await
        }
    }
}
