use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use parking_lot::Mutex;

use crate::human::llm::capability::ModelCapability;
use crate::human::llm::media::cache::MediaUploadCache;
use crate::human::llm::media::mime::{MediaKind, media_kind_for_mime, media_mime_for_path};
use crate::human::llm::media::r#ref::{MediaRefKind, media_ref_from_part};
use crate::human::llm::media::source::{MediaContent, MediaSource};
use crate::human::llm::media::upload::{MediaVideoUploadOptions, VideoUploadInput};
use crate::human::llm::message::{
    AssistantMessage, ContentPart, ImageUrl, Message, SystemMessage, ToolMessage, UserMessage,
    VideoUrl,
};
use crate::human::llm::provider::definition::Provider;
use crate::human::llm::requester::actor::{MessageResolveContext, MessageResolver};
use crate::wire::record::WireAsyncError;

const VIDEO_UNAVAILABLE_TEXT: &str = "[video omitted: media unavailable]";
const IMAGE_UNAVAILABLE_TEXT: &str = "[image omitted: media unavailable]";

pub struct MediaRefResolverDeps {
    pub providers: Vec<Arc<dyn Provider>>,
    pub source: Arc<dyn MediaSource>,
    pub cache: Arc<dyn MediaUploadCache>,
}

fn resolve_mime_type(content: &MediaContent, kind: MediaKind) -> Option<String> {
    if let Some(mime_type) = &content.mime_type {
        return (media_kind_for_mime(mime_type) == Some(kind)).then(|| mime_type.clone());
    }
    let mime_type = media_mime_for_path(content.filename.as_deref()?)?;
    (media_kind_for_mime(&mime_type) == Some(kind)).then_some(mime_type)
}

fn data_url(bytes: &[u8], mime_type: &str) -> String {
    format!(
        "data:{mime_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn unavailable_text(kind: MediaRefKind) -> ContentPart {
    ContentPart::Text {
        text: match kind {
            MediaRefKind::Video => VIDEO_UNAVAILABLE_TEXT,
            MediaRefKind::Image => IMAGE_UNAVAILABLE_TEXT,
        }
        .to_string(),
        meta: None,
    }
}

fn is_media_upload_auth_error(error: &WireAsyncError) -> bool {
    if error
        .downcast_ref::<reqwest::Error>()
        .and_then(|error| error.status())
        .is_some_and(|status| status.as_u16() == 401 || status.as_u16() == 403)
    {
        return true;
    }
    let debug = format!("{error:?}");
    ["statusCode: 401", "statusCode: 403", "status: 401", "status: 403"]
        .iter()
        .any(|pattern| debug.contains(pattern))
}

fn content_of(message: &Message) -> &Vec<ContentPart> {
    match message {
        Message::System(message) => &message.content,
        Message::User(message) => &message.content,
        Message::Assistant(message) => &message.content,
        Message::Tool(message) => &message.content,
    }
}

fn with_content(message: &Message, content: Vec<ContentPart>) -> Message {
    match message {
        Message::System(message) => Message::System(SystemMessage {
            content,
            ..message.clone()
        }),
        Message::User(message) => Message::User(UserMessage {
            content,
            ..message.clone()
        }),
        Message::Assistant(message) => Message::Assistant(AssistantMessage {
            content,
            ..message.clone()
        }),
        Message::Tool(message) => Message::Tool(ToolMessage {
            content,
            ..message.clone()
        }),
    }
}

fn has_media_ref(message: &Message) -> bool {
    content_of(message)
        .iter()
        .any(|part| media_ref_from_part(part).is_some())
}

struct MediaRefResolver {
    providers: HashMap<String, Arc<dyn Provider>>,
    source: Arc<dyn MediaSource>,
    cache: Arc<dyn MediaUploadCache>,
    image_memo: Mutex<HashMap<String, ContentPart>>,
}

impl MediaRefResolver {
    async fn resolve_image_part(
        &self,
        ref_: &str,
        capability: &ModelCapability,
    ) -> Result<ContentPart, WireAsyncError> {
        if !capability.image_in {
            return Ok(unavailable_text(MediaRefKind::Image));
        }
        if let Some(memoed) = self.image_memo.lock().get(ref_).cloned() {
            return Ok(memoed);
        }
        let content = self.source.get(ref_).await?;
        let mime_type = content
            .as_ref()
            .and_then(|content| resolve_mime_type(content, MediaKind::Image));
        let (Some(content), Some(mime_type)) = (content, mime_type) else {
            return Ok(unavailable_text(MediaRefKind::Image));
        };
        let part = ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: data_url(&content.bytes, &mime_type),
                id: None,
                name: None,
                dimensions: None,
                detail: None,
            },
        };
        self.image_memo.lock().insert(ref_.to_string(), part.clone());
        Ok(part)
    }

    async fn resolve_video_part(
        &self,
        ref_: &str,
        ctx: &MessageResolveContext,
        provider: Option<&Arc<dyn Provider>>,
        capability: &ModelCapability,
    ) -> Result<ContentPart, WireAsyncError> {
        if !capability.video_in {
            return Ok(unavailable_text(MediaRefKind::Video));
        }
        let provider_key = &ctx.model.provider;
        if let Some(cached) = self.cache.get(ref_, provider_key).await? {
            return Ok(cached);
        }
        let content = self.source.get(ref_).await?;
        let mime_type = content
            .as_ref()
            .and_then(|content| resolve_mime_type(content, MediaKind::Video));
        let (Some(content), Some(mime_type)) = (content, mime_type) else {
            return Ok(unavailable_text(MediaRefKind::Video));
        };
        let media = provider.and_then(|provider| provider.media());
        if let Some(uploader) = media.and_then(|media| media.upload_video.clone()) {
            let result = async {
                let part = uploader(
                    VideoUploadInput {
                        data: content.bytes.clone(),
                        mime_type: mime_type.clone(),
                        filename: content.filename.clone(),
                    },
                    MediaVideoUploadOptions {
                        model: ctx.model.clone(),
                        signal: Some(ctx.signal.clone()),
                    },
                )
                .await?;
                self.cache.put(ref_, provider_key, &part).await?;
                Ok::<ContentPart, WireAsyncError>(part)
            }
            .await;
            match result {
                Ok(part) => return Ok(part),
                Err(error) => {
                    if ctx.signal.is_cancelled() || is_media_upload_auth_error(&error) {
                        return Err(error);
                    }
                }
            }
        }
        if media.is_some_and(|media| media.inline_video == Some(true)) {
            return Ok(ContentPart::VideoUrl {
                video_url: VideoUrl {
                    url: data_url(&content.bytes, &mime_type),
                    id: None,
                    name: None,
                },
            });
        }
        Ok(unavailable_text(MediaRefKind::Video))
    }
}

#[async_trait]
impl MessageResolver for MediaRefResolver {
    fn id(&self) -> &str {
        "media-ref"
    }

    async fn resolve(
        &self,
        messages: Vec<Message>,
        ctx: MessageResolveContext,
    ) -> Result<Vec<Message>, WireAsyncError> {
        if !messages.iter().any(has_media_ref) {
            return Ok(messages);
        }
        let provider = self.providers.get(&ctx.model.provider);
        let mut out = Vec::with_capacity(messages.len());
        for message in messages {
            if !has_media_ref(&message) {
                out.push(message);
                continue;
            }
            let mut content = Vec::new();
            for part in content_of(&message) {
                let Some(ref_) = media_ref_from_part(part) else {
                    content.push(part.clone());
                    continue;
                };
                content.push(match ref_.kind {
                    MediaRefKind::Image => {
                        self.resolve_image_part(&ref_.ref_, &ctx.model.capability).await?
                    }
                    MediaRefKind::Video => {
                        self.resolve_video_part(&ref_.ref_, &ctx, provider, &ctx.model.capability)
                            .await?
                    }
                });
            }
            out.push(with_content(&message, content));
        }
        Ok(out)
    }
}

pub fn create_media_ref_resolver(deps: MediaRefResolverDeps) -> Arc<dyn MessageResolver> {
    Arc::new(MediaRefResolver {
        providers: deps
            .providers
            .into_iter()
            .map(|provider| (provider.id().to_string(), provider))
            .collect(),
        source: deps.source,
        cache: deps.cache,
        image_memo: Mutex::new(HashMap::new()),
    })
}
