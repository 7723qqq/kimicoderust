use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::agent::media::image_format_policy;
use crate::llm::files_upload::UploadError;
use crate::llm::media_budget::Entry;
use crate::rpc::types::{ContentPart, MediaKind};
use crate::server::files::FileStore;
use kimi_core::types::LLMMessage;

pub use kimi_core::types::ResolvedRequest;

pub const IMAGE_UNAVAILABLE_TEXT: &str =
    "[image omitted: the uploaded file is no longer available]";
pub const VIDEO_UNAVAILABLE_TEXT: &str =
    "[video omitted: the uploaded file is no longer available]";
pub const AUDIO_UNAVAILABLE_TEXT: &str =
    "[audio omitted: the uploaded file is no longer available]";

pub fn unavailable_text(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => IMAGE_UNAVAILABLE_TEXT,
        MediaKind::Video => VIDEO_UNAVAILABLE_TEXT,
        MediaKind::Audio => AUDIO_UNAVAILABLE_TEXT,
    }
}

pub const UPLOAD_CACHE_DOMAIN: &str = "media_upload_cache";

fn escape_media_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn build_media_path_tag(kind: MediaKind, path: &str) -> String {
    format!(
        "<{} path=\"{}\"></{}>",
        kind.as_str(),
        escape_media_attribute(path),
        kind.as_str()
    )
}

pub use kimi_core::types::MediaTarget;

pub fn media_url_part(
    kind: MediaKind,
    url: String,
    id: Option<String>,
    name: Option<String>,
) -> ContentPart {
    match kind {
        MediaKind::Image => ContentPart::ImageUrl {
            image_url: kimi_core::types::ImageUrl {
                url,
                id,
                name,
                dimensions: None,
                detail: None,
            },
        },
        MediaKind::Video => ContentPart::VideoUrl {
            video_url: kimi_core::types::VideoUrl { url, id, name },
        },
        MediaKind::Audio => ContentPart::AudioUrl {
            audio_url: kimi_core::types::AudioUrl { url, id },
        },
    }
}

pub fn inline_image_part(
    media_type: String,
    data: String,
    name: Option<String>,
) -> ContentPart {
    media_url_part(
        MediaKind::Image,
        format!("data:{media_type};base64,{data}"),
        None,
        name,
    )
}

pub fn file_media_part(file_id: String, kind: MediaKind) -> ContentPart {
    media_url_part(kind, format!("{KIMI_FILE_SCHEME}{file_id}"), None, None)
}

pub fn media_ref_of(block: &ContentPart) -> Option<(&str, MediaKind)> {
    let (url, kind) = match block {
        ContentPart::ImageUrl { image_url } => (&image_url.url, MediaKind::Image),
        ContentPart::VideoUrl { video_url } => (&video_url.url, MediaKind::Video),
        ContentPart::AudioUrl { audio_url } => (&audio_url.url, MediaKind::Audio),
        _ => return None,
    };
    parse_daemon_file_url(url).map(|file_id| (file_id, kind))
}

pub const KIMI_FILE_SCHEME: &str = "kimi-file://";

pub fn parse_daemon_file_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix(KIMI_FILE_SCHEME)?;
    let file_id = rest.split('?').next().unwrap_or("");
    (!file_id.is_empty()).then_some(file_id)
}

pub fn has_media_refs(messages: &[LLMMessage]) -> bool {
    messages
        .iter()
        .any(|message| message.blocks.iter().any(|b| media_ref_of(b).is_some()))
}

fn inline_bytes(block: &ContentPart) -> usize {
    match block {
        ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:") => {
            image_url.url.len()
        }
        ContentPart::AudioUrl { audio_url } if audio_url.url.starts_with("data:") => {
            audio_url.url.len()
        }
        ContentPart::VideoUrl { video_url } if video_url.url.starts_with("data:") => {
            video_url.url.len()
        }
        _ => 0,
    }
}

fn account_hash(credential: Option<&str>) -> String {
    let Some(credential) = credential.map(str::trim).filter(|value| !value.is_empty()) else {
        return "no-key".to_string();
    };
    let identity = format!(
        "api-key\0{}",
        stable_jwt_subject(credential).unwrap_or_else(|| credential.to_string())
    );
    let digest = Sha256::digest(identity.as_bytes());
    format!("{digest:x}")[..16].to_string()
}

fn stable_jwt_subject(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    let payload = parts.nth(1)?;
    let signature = parts.next()?;
    if signature.is_empty() || parts.next().is_some() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    for key in ["sub", "user_id"] {
        if let Some(text) = value.get(key).and_then(|value| value.as_str())
            && !text.is_empty()
        {
            return Some(text.to_string());
        }
    }
    None
}

pub fn inline_entries(messages: &[LLMMessage]) -> Vec<Entry> {
    let mut entries = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.blocks.iter().enumerate() {
            if let Some((kind, bytes, key)) = inline_budget(block) {
                entries.push(Entry {
                    message_index,
                    block_index,
                    key,
                    file_id: None,
                    path: None,
                    kind,
                    bytes,
                });
            }
        }
    }
    entries
}

fn inline_budget(block: &ContentPart) -> Option<(MediaKind, usize, String)> {
    let (kind, bytes, pieces): (MediaKind, usize, Vec<&str>) = match block {
        ContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:") => {
            (MediaKind::Image, image_url.url.len(), vec![&image_url.url])
        }
        ContentPart::VideoUrl { video_url } if video_url.url.starts_with("data:") => {
            (MediaKind::Video, video_url.url.len(), vec![&video_url.url])
        }
        ContentPart::AudioUrl { audio_url } if audio_url.url.starts_with("data:") => {
            (MediaKind::Audio, audio_url.url.len(), vec![&audio_url.url])
        }
        _ => return None,
    };
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0u8]);
    for piece in pieces {
        hasher.update(piece.as_bytes());
        hasher.update([0u8]);
    }
    Some((kind, bytes, format!("inline\0{:x}", hasher.finalize())))
}

const INLINE_MEMO_MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const INLINE_MEMO_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct InlineMemo {
    entries: HashMap<String, (ContentPart, usize)>,

    order: std::collections::VecDeque<String>,
    bytes: usize,
}

impl InlineMemo {
    fn get(&mut self, key: &str) -> Option<ContentPart> {
        let block = self.entries.get(key)?.0.clone();

        if let Some(position) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.to_string());
        Some(block)
    }

    fn insert(&mut self, key: String, block: ContentPart, bytes: usize) {
        if bytes > INLINE_MEMO_MAX_ENTRY_BYTES {
            return;
        }
        if let Some((_, previous)) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous);
            if let Some(position) = self.order.iter().position(|existing| *existing == key) {
                self.order.remove(position);
            }
        }
        self.bytes += bytes;
        self.order.push_back(key.clone());
        self.entries.insert(key, (block, bytes));
        while self.bytes > INLINE_MEMO_MAX_TOTAL_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some((_, evicted)) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(evicted);
            }
        }
    }
}

pub struct MediaResolver {
    store: FileStore,

    resolved: Mutex<HashMap<String, ContentPart>>,

    inline_memo: Mutex<InlineMemo>,

    upload_unsupported: Mutex<std::collections::HashSet<String>>,

    upload_cache: Option<Arc<crate::session::sqlite_store::SqliteSessionStore>>,
}

impl Default for MediaResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaResolver {
    pub fn new() -> Self {
        Self::with_store(FileStore::new())
    }

    pub fn with_store(store: FileStore) -> Self {
        Self {
            store,
            resolved: Mutex::new(HashMap::new()),
            inline_memo: Mutex::new(InlineMemo::default()),
            upload_unsupported: Mutex::new(std::collections::HashSet::new()),
            upload_cache: None,
        }
    }

    pub fn with_upload_cache(
        mut self,
        store: Arc<crate::session::sqlite_store::SqliteSessionStore>,
    ) -> Self {
        self.upload_cache = Some(store);
        self
    }

    pub fn media_entries(&self, messages: &[LLMMessage]) -> Vec<Entry> {
        let mut entries = Vec::new();
        for (message_index, message) in messages.iter().enumerate() {
            for (block_index, block) in message.blocks.iter().enumerate() {
                if let Some((file_id, kind)) = media_ref_of(block) {
                    entries.push(Entry {
                        message_index,
                        block_index,
                        key: file_id.to_string(),
                        file_id: Some(file_id.to_string()),
                        path: self.display_path(file_id),
                        kind,
                        bytes: 0,
                    });
                } else if let Some((kind, bytes, key)) = inline_budget(block) {
                    entries.push(Entry {
                        message_index,
                        block_index,
                        key,
                        file_id: None,
                        path: None,
                        kind,
                        bytes,
                    });
                }
            }
        }
        entries
    }

    pub async fn resolve<'a>(
        &self,
        messages: &'a [LLMMessage],
        target: Option<&MediaTarget>,
        credential: Option<&str>,
    ) -> Result<ResolvedRequest<'a>, UploadError> {
        let mut entries = Vec::new();
        let mut out: Option<Vec<LLMMessage>> = None;
        for (message_index, message) in messages.iter().enumerate() {
            let mut blocks: Option<Vec<ContentPart>> = None;
            for (block_index, block) in message.blocks.iter().enumerate() {
                match media_ref_of(block) {
                    Some((file_id, kind)) => {
                        let (resolved, path) =
                            self.resolve_one(file_id, kind, target, credential).await?;
                        entries.push(Entry {
                            message_index,
                            block_index,
                            key: file_id.to_string(),
                            file_id: Some(file_id.to_string()),
                            path,
                            kind,
                            bytes: inline_bytes(&resolved),
                        });

                        blocks
                            .get_or_insert_with(|| message.blocks[..block_index].to_vec())
                            .push(resolved);
                    }
                    None => {
                        if let Some((kind, bytes, key)) = inline_budget(block) {
                            entries.push(Entry {
                                message_index,
                                block_index,
                                key,
                                file_id: None,
                                path: None,
                                kind,
                                bytes,
                            });
                        }
                        if let Some(blocks) = blocks.as_mut() {
                            blocks.push(block.clone());
                        }
                    }
                }
            }
            match blocks {
                Some(blocks) => {
                    out.get_or_insert_with(|| messages[..message_index].to_vec())
                        .push(LLMMessage {
                            blocks,
                            ..message.clone()
                        });
                }
                None => {
                    if let Some(out) = out.as_mut() {
                        out.push(message.clone());
                    }
                }
            }
        }
        Ok(ResolvedRequest {
            messages: match out {
                Some(messages) => Cow::Owned(messages),
                None => Cow::Borrowed(messages),
            },
            entries,
        })
    }

    pub fn display_path(&self, file_id: &str) -> Option<String> {
        match self.store.get(file_id) {
            Ok((_, path)) => Some(path.to_string_lossy().into_owned()),
            Err((404, _, _)) => None,
            Err((status, code, message)) => {
                tracing::warn!(
                    file_id,
                    status,
                    code,
                    %message,
                    "failed to read the media file metadata for its display path"
                );
                None
            }
        }
    }

    fn cache_key(
        file_id: &str,
        kind: MediaKind,
        target: Option<&MediaTarget>,
        credential: Option<&str>,
    ) -> String {
        let (provider_key, protocol, base_url) = match target {
            Some(target) => (
                target.provider_key.as_str(),
                target.protocol.as_str(),
                target.base_url.as_str(),
            ),
            None => ("", "", ""),
        };
        format!(
            "{}\0{file_id}\0{provider_key}\0{protocol}\0{base_url}\0{}",
            kind.as_str(),
            account_hash(credential)
        )
    }

    fn inline_key(file_id: &str, kind: MediaKind, target: Option<&MediaTarget>) -> String {
        let provider_key = target
            .map(|target| target.provider_key.as_str())
            .unwrap_or("");
        let accepts = target.is_none_or(|target| target.accepts(kind));
        let inline_video = target.is_none_or(|target| target.inline_video);
        format!(
            "{}\0{file_id}\0{provider_key}\0{accepts}\0{inline_video}",
            kind.as_str()
        )
    }

    async fn resolve_one(
        &self,
        file_id: &str,
        kind: MediaKind,
        target: Option<&MediaTarget>,
        credential: Option<&str>,
    ) -> Result<(ContentPart, Option<String>), UploadError> {
        let cache_key = Self::cache_key(file_id, kind, target, credential);
        if let Some(hit) = self
            .resolved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&cache_key)
        {
            return Ok((hit.clone(), self.display_path(file_id)));
        }
        let inline_key = Self::inline_key(file_id, kind, target);
        if let Some(hit) = self
            .inline_memo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&inline_key)
        {
            return Ok((hit, self.display_path(file_id)));
        }

        if let Some(provider_file_id) = self.cached_upload(&cache_key) {
            let block = reference_block(kind, &provider_file_id);
            self.resolved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(cache_key, block.clone());
            return Ok((block, self.display_path(file_id)));
        }
        let block = self
            .resolve_uncached(file_id, kind, target, credential)
            .await?;
        let bytes = inline_bytes(&block);
        if bytes > 0 {
            self.inline_memo
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(inline_key, block.clone(), bytes);
        } else {
            self.resolved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(cache_key, block.clone());
        }
        Ok((block, self.display_path(file_id)))
    }

    fn cached_upload(&self, cache_key: &str) -> Option<String> {
        let store = self.upload_cache.as_ref()?;
        match store.get_state(UPLOAD_CACHE_DOMAIN, cache_key) {
            Ok(value) => value
                .and_then(|value| value.as_str().map(str::to_string))
                .filter(|id| !id.is_empty()),

            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to read the persisted media upload cache entry"
                );
                None
            }
        }
    }

    fn remember_upload(&self, file_id: &str, cache_key: &str, provider_file_id: &str) {
        let Some(store) = self.upload_cache.as_ref() else {
            return;
        };
        if let Err(error) = store.put_state(
            UPLOAD_CACHE_DOMAIN,
            cache_key,
            &serde_json::json!(provider_file_id),
        ) {
            tracing::warn!(
                file_id,
                %error,
                "failed to persist the media upload cache entry"
            );
        }
    }

    async fn resolve_uncached(
        &self,
        file_id: &str,
        kind: MediaKind,
        target: Option<&MediaTarget>,
        credential: Option<&str>,
    ) -> Result<ContentPart, UploadError> {
        let (meta, path) = match self.store.get(file_id) {
            Ok(found) => found,
            Err((404, _, _)) => {
                return Ok(ContentPart::Text {
                    text: unavailable_text(kind).to_string(),
                    meta: None,
                });
            }
            Err((status, code, message)) => {
                return Err(UploadError::Other(format!(
                    "cannot read media file {file_id} (status {status}, code {code}): {message}"
                )));
            }
        };
        let path = path.to_string_lossy().into_owned();

        if target.is_some_and(|t| !t.accepts(kind)) {
            return Ok(ContentPart::Text {
                text: build_media_path_tag(kind, &path),
                meta: None,
            });
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,

            Err(error) => {
                tracing::warn!(
                    file_id,
                    %path,
                    %error,
                    "failed to read the media blob; degrading to a path tag"
                );
                return Ok(ContentPart::Text {
                    text: build_media_path_tag(kind, &path),
                    meta: None,
                });
            }
        };
        let header = &bytes[..bytes.len().min(crate::native::file_type::MEDIA_SNIFF_BYTES)];
        let sniff_name = if meta.name.is_empty() {
            path.rsplit(['/', '\\']).next().unwrap_or(&path).to_string()
        } else {
            meta.name.clone()
        };
        let media_type = match kind {
            MediaKind::Image => {
                let Some(sniffed) = crate::native::file_type::sniff_media_from_magic(header) else {
                    return Ok(ContentPart::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                };
                if sniffed.kind != crate::native::file_type::MediaSniffKind::Image {
                    return Ok(ContentPart::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                }
                let effective = image_format_policy::normalize_image_mime(sniffed.mime_type);
                let provider = target
                    .map(|t| t.provider_type.as_str())
                    .filter(|value| !value.is_empty());
                if !image_format_policy::is_model_accepted_image_mime(&effective, provider) {
                    return Ok(ContentPart::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                }
                effective
            }
            MediaKind::Video => {
                let Some(sniffed) =
                    crate::native::file_type::detect_media_file_type(&sniff_name, header)
                else {
                    return Ok(ContentPart::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                };
                if sniffed.kind != crate::native::file_type::MediaSniffKind::Video {
                    return Ok(ContentPart::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                }
                sniffed.mime_type.to_string()
            }
            MediaKind::Audio => {
                if meta.media_type.is_empty() {
                    crate::server::infer_media_type(std::path::Path::new(&path))
                } else {
                    meta.media_type.clone()
                }
            }
        };
        let name = (!meta.name.is_empty()).then(|| meta.name.clone());
        if let Some(uploaded) = self
            .upload(
                file_id,
                kind,
                target,
                credential,
                &media_type,
                &sniff_name,
                &bytes,
            )
            .await?
        {
            return Ok(uploaded);
        }
        if kind == MediaKind::Video && target.is_some_and(|t| !t.inline_video) {
            return Ok(ContentPart::Text {
                text: build_media_path_tag(kind, &path),
                meta: None,
            });
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(match kind {
            MediaKind::Image => inline_image_part(media_type, encoded, name),
            MediaKind::Video => media_url_part(
                MediaKind::Video,
                format!("data:{media_type};base64,{encoded}"),
                None,
                name,
            ),
            MediaKind::Audio => media_url_part(
                MediaKind::Audio,
                format!("data:{media_type};base64,{encoded}"),
                None,
                name,
            ),
        })
    }

    async fn upload(
        &self,
        file_id: &str,
        kind: MediaKind,
        target: Option<&MediaTarget>,
        credential: Option<&str>,
        media_type: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Result<Option<ContentPart>, UploadError> {
        let Some(target) = target else {
            return Ok(None);
        };
        if !target.uploads_media {
            return Ok(None);
        }
        let Some(credential) = credential else {
            return Ok(None);
        };
        if self
            .upload_unsupported
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&target.provider_key)
        {
            return Ok(None);
        }
        let purpose = match kind {
            MediaKind::Image => "image",
            MediaKind::Video => "video",

            MediaKind::Audio => return Ok(None),
        };
        let filename = if filename.trim().is_empty() {
            crate::llm::files_upload::upload_filename(media_type)
        } else {
            filename.to_string()
        };
        match crate::llm::files_upload::upload_media(
            &target.base_url,
            &target.protocol,
            credential,
            purpose,
            &filename,
            media_type,
            bytes,
        )
        .await
        {
            Ok(id) => {
                let cache_key = Self::cache_key(file_id, kind, Some(target), Some(credential));
                self.remember_upload(file_id, &cache_key, &id);
                Ok(Some(reference_block(kind, &id)))
            }
            Err(crate::llm::files_upload::UploadError::Unsupported(_)) => {
                self.upload_unsupported
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(target.provider_key.clone());
                Ok(None)
            }

            Err(error @ crate::llm::files_upload::UploadError::Auth(_)) => Err(error),
            Err(crate::llm::files_upload::UploadError::Other(_)) => Ok(None),
        }
    }
}

fn reference_block(kind: MediaKind, id: &str) -> ContentPart {
    media_url_part(
        kind,
        format!("ms://{id}"),
        Some(id.to_string()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::files::FileStore;

    fn store_with(name: &str, media_type: &str, bytes: &[u8]) -> (FileStore, String) {
        let dir = std::env::temp_dir().join(format!("kimi-media-resolver-{}", fastrand::u64(..)));
        let store = FileStore::with_root(dir);
        let meta = store.save(name, media_type, None, bytes).unwrap();
        (store, meta.id)
    }

    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nPNGDATA";

    fn ref_message(file_id: &str, kind: MediaKind) -> LLMMessage {
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: String::new(),
            blocks: vec![file_media_part(file_id.to_string(), kind)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            prompt_id: None,
            origin: None,
        }
    }

    fn accepting() -> MediaTarget {
        MediaTarget {
            image_in: true,
            video_in: true,
            audio_in: true,
            inline_video: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_a_reference_inlines_when_the_model_takes_the_media() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);

        let messages = [ref_message(&file_id, MediaKind::Image)];
        let out = resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert_eq!(
            out.messages[0].blocks[0],
            inline_image_part(
                "image/png".into(),
                base64::engine::general_purpose::STANDARD.encode(PNG_BYTES),
                Some("a.png".into()),
            )
        );
        assert_eq!(out.entries.len(), 1);
        assert_eq!(out.entries[0].key, file_id);
        assert_eq!(out.entries[0].file_id.as_deref(), Some(file_id.as_str()));
        assert!(out.entries[0].path.is_some(), "the saved path is known");
        assert!(out.entries[0].bytes > 0, "an inlined image costs bytes");
    }

    #[tokio::test]
    async fn test_a_model_that_cannot_take_the_media_gets_the_saved_path() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);
        let target = MediaTarget {
            image_in: false,
            ..accepting()
        };

        let messages = [ref_message(&file_id, MediaKind::Image)];
        let out = resolver
            .resolve(&messages, Some(&target), None)
            .await
            .unwrap();

        let ContentPart::Text { text, .. } = &out.messages[0].blocks[0] else {
            panic!("expected a path tag, got {:?}", out.messages[0].blocks[0]);
        };
        assert!(text.starts_with("<image path=\""), "{text}");
        assert!(text.ends_with("\"></image>"), "{text}");
        assert!(text.contains(&file_id), "{text}");
        assert_eq!(out.entries[0].bytes, 0, "a path tag costs no request bytes");
    }

    #[tokio::test]
    async fn test_a_video_inlines_only_for_a_provider_that_declares_inline_video() {
        let (store, file_id) = store_with("a.mp4", "video/mp4", b"MP4DATA");
        let resolver = MediaResolver::with_store(store);
        let messages = [ref_message(&file_id, MediaKind::Video)];

        let google = MediaTarget {
            provider_key: "google".into(),
            inline_video: true,
            ..accepting()
        };
        let out = resolver
            .resolve(&messages, Some(&google), None)
            .await
            .unwrap();
        assert!(
            matches!(
                &out.messages[0].blocks[0],
                ContentPart::VideoUrl { video_url, .. }
                    if video_url.url.starts_with("data:video/mp4;base64,")
            ),
            "{:?}",
            out.messages[0].blocks[0]
        );

        let openai = MediaTarget {
            provider_key: "openai".into(),
            inline_video: false,
            ..accepting()
        };
        let out = resolver
            .resolve(&messages, Some(&openai), None)
            .await
            .unwrap();
        let ContentPart::Text { text, .. } = &out.messages[0].blocks[0] else {
            panic!("expected a path tag, got {:?}", out.messages[0].blocks[0]);
        };
        assert!(text.starts_with("<video path=\""), "{text}");
        assert!(text.ends_with("\"></video>"), "{text}");
        assert_eq!(out.entries[0].bytes, 0, "a path tag costs no request bytes");
    }

    #[tokio::test]
    async fn test_an_image_with_unrecognized_bytes_degrades_to_its_path() {
        let (store, file_id) = store_with("a.png", "image/png", b"not an image");
        let resolver = MediaResolver::with_store(store);

        let messages = [ref_message(&file_id, MediaKind::Image)];

        let out = resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        let ContentPart::Text { text, .. } = &out.messages[0].blocks[0] else {
            panic!("expected a path tag, got {:?}", out.messages[0].blocks[0]);
        };
        assert!(text.starts_with("<image path=\""), "{text}");
        assert_eq!(out.entries[0].bytes, 0);
    }

    #[tokio::test]
    async fn test_an_image_format_the_provider_rejects_degrades_to_its_path() {
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00heicmif1";
        let (store, file_id) = store_with("a.heic", "image/heic", heic);
        let resolver = MediaResolver::with_store(store);
        let messages = [ref_message(&file_id, MediaKind::Image)];

        let openai = MediaTarget {
            provider_key: "openai".into(),
            provider_type: "openai".into(),
            ..accepting()
        };
        let out = resolver
            .resolve(&messages, Some(&openai), None)
            .await
            .unwrap();
        let ContentPart::Text { text, .. } = &out.messages[0].blocks[0] else {
            panic!("expected a path tag, got {:?}", out.messages[0].blocks[0]);
        };
        assert!(text.starts_with("<image path=\""), "{text}");

        let kimi = MediaTarget {
            provider_key: "kimi".into(),
            provider_type: "kimi".into(),
            ..accepting()
        };
        let out = resolver
            .resolve(&messages, Some(&kimi), None)
            .await
            .unwrap();
        assert_eq!(
            out.messages[0].blocks[0],
            inline_image_part(
                "image/heic".into(),
                base64::engine::general_purpose::STANDARD.encode(heic),
                Some("a.heic".into()),
            )
        );
    }

    #[test]
    fn test_a_rotated_credential_keeps_the_account_cache_key() {
        let header = |payload: &str| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes())
        };
        let token = |sub: &str, iat: u32| {
            format!(
                "{}.{}.{}",
                header(r#"{"alg":"HS256"}"#),
                header(&format!(r#"{{"sub":"{sub}","iat":{iat}}}"#)),
                header("signature")
            )
        };
        let first = token("user-1", 1);
        let refreshed = token("user-1", 2);
        let other = token("user-2", 2);
        assert_eq!(
            MediaResolver::cache_key("f_1", MediaKind::Image, Some(&accepting()), Some(&first)),
            MediaResolver::cache_key(
                "f_1",
                MediaKind::Image,
                Some(&accepting()),
                Some(&refreshed)
            )
        );
        assert_ne!(
            MediaResolver::cache_key("f_1", MediaKind::Image, Some(&accepting()), Some(&first)),
            MediaResolver::cache_key("f_1", MediaKind::Image, Some(&accepting()), Some(&other))
        );
    }

    #[test]
    fn test_the_media_cache_key_separates_protocol_endpoint_and_account() {
        let target = MediaTarget {
            protocol: "openai".into(),
            base_url: "https://api.example".into(),
            provider_key: "kimi".into(),
            ..accepting()
        };
        let base =
            MediaResolver::cache_key("f_1", MediaKind::Image, Some(&target), Some("token-a"));
        assert_eq!(
            base,
            MediaResolver::cache_key("f_1", MediaKind::Image, Some(&target), Some("token-a"))
        );
        assert_ne!(
            base,
            MediaResolver::cache_key("f_1", MediaKind::Image, Some(&target), Some("token-b")),
            "a different account must not share the upload"
        );
        assert_ne!(
            base,
            MediaResolver::cache_key(
                "f_1",
                MediaKind::Image,
                Some(&MediaTarget {
                    protocol: "anthropic".into(),
                    ..target.clone()
                }),
                Some("token-a")
            ),
            "a different wire protocol must not share the upload"
        );
        assert_ne!(
            base,
            MediaResolver::cache_key(
                "f_1",
                MediaKind::Image,
                Some(&MediaTarget {
                    base_url: "https://other.example".into(),
                    ..target.clone()
                }),
                Some("token-a")
            ),
            "a different endpoint must not share the upload"
        );
        assert_ne!(
            base,
            MediaResolver::cache_key("f_1", MediaKind::Video, Some(&target), Some("token-a")),
            "a different media family must not share the entry"
        );
        assert!(
            !base.contains("token-a"),
            "the credential itself must never enter a key: {base:?}"
        );
    }

    #[tokio::test]
    async fn test_an_inline_payload_stays_out_of_the_unbounded_map() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);
        let messages = [ref_message(&file_id, MediaKind::Image)];

        resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert!(
            resolver
                .resolved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty(),
            "an inline payload must not enter the unbounded map"
        );
        assert_eq!(
            resolver
                .inline_memo
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn test_the_inline_memo_is_bounded() {
        let mut memo = InlineMemo::default();
        let block = ContentPart::Text {
            text: "x".into(),
            meta: None,
        };
        for index in 0..9 {
            memo.insert(
                format!("k{index}"),
                block.clone(),
                INLINE_MEMO_MAX_ENTRY_BYTES,
            );
        }
        assert_eq!(
            memo.entries.len(),
            INLINE_MEMO_MAX_TOTAL_BYTES / INLINE_MEMO_MAX_ENTRY_BYTES
        );
        assert!(memo.bytes <= INLINE_MEMO_MAX_TOTAL_BYTES);
        assert!(
            !memo.entries.contains_key("k0"),
            "the oldest entry is evicted"
        );

        memo.insert("huge".into(), block, INLINE_MEMO_MAX_ENTRY_BYTES + 1);
        assert!(
            !memo.entries.contains_key("huge"),
            "an entry above the per-item cap is not memoized"
        );
    }

    #[tokio::test]
    async fn test_a_missing_blob_becomes_the_unavailable_placeholder() {
        let dir = std::env::temp_dir().join(format!("kimi-media-gone-{}", fastrand::u64(..)));
        let resolver = MediaResolver::with_store(FileStore::with_root(dir));

        let messages = [ref_message("f_missing", MediaKind::Video)];
        let out = resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert_eq!(
            out.messages[0].blocks[0],
            ContentPart::Text {
                text: VIDEO_UNAVAILABLE_TEXT.into(),
                meta: None,
            }
        );
        assert_eq!(out.entries[0].bytes, 0);
    }

    #[tokio::test]
    async fn test_a_history_without_media_is_returned_borrowed() {
        let resolver = MediaResolver::new();
        let messages = vec![LLMMessage::new(kimi_core::types::Role::User, "hello")];

        let out = resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert!(matches!(out.messages, Cow::Borrowed(_)));
        assert!(out.entries.is_empty());
    }

    #[tokio::test]
    async fn test_inline_media_is_budgeted_without_a_file_behind_it() {
        let resolver = MediaResolver::new();
        let messages = vec![LLMMessage {
            role: kimi_core::types::Role::User,
            content: String::new(),
            blocks: vec![inline_image_part("image/png".into(), "AAAA".into(), None)],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            prompt_id: None,
            origin: None,
        }];

        let out = resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert!(matches!(out.messages, Cow::Borrowed(_)));
        assert_eq!(out.entries.len(), 1);
        assert!(out.entries[0].key.starts_with("inline\0"));
        assert_eq!(out.entries[0].file_id, None);
        assert_eq!(out.entries[0].bytes, 26);
    }

    #[tokio::test]
    async fn test_a_remote_url_is_not_budgeted() {
        let resolver = MediaResolver::new();
        let messages = vec![LLMMessage {
            role: kimi_core::types::Role::User,
            content: String::new(),
            blocks: vec![media_url_part(
                MediaKind::Image,
                "https://example.test/huge.png".into(),
                None,
                None,
            )],
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
            prompt_id: None,
            origin: None,
        }];

        let out = resolver
            .resolve(&messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert!(
            out.entries.is_empty(),
            "a remote URL costs no request bytes"
        );
    }

    #[tokio::test]
    async fn test_the_resolved_block_is_memoized_per_file_and_provider() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);
        let message = ref_message(&file_id, MediaKind::Image);

        let first_messages = [message.clone()];
        let first = resolver
            .resolve(&first_messages, Some(&accepting()), None)
            .await
            .unwrap();

        let second_messages = [message];
        let second = resolver
            .resolve(&second_messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert_eq!(first.messages[0].blocks[0], second.messages[0].blocks[0]);
    }

    #[tokio::test]
    async fn an_upload_survives_a_resolver_restart_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let files = FileStore::with_root(dir.path().join("files"));
        let png = files.save("a.png", "image/png", None, PNG_BYTES).unwrap();
        let sqlite =
            Arc::new(crate::session::sqlite_store::SqliteSessionStore::in_memory().unwrap());
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (base_url, server) = spawn_counting_files_server(Arc::clone(&requests)).await;
        let target = MediaTarget {
            protocol: "openai".into(),
            base_url,
            provider_key: "kimi".into(),
            uploads_media: true,
            ..accepting()
        };
        let messages = [ref_message(&png.id, MediaKind::Image)];

        let first = MediaResolver::with_store(files.clone())
            .with_upload_cache(sqlite.clone())
            .resolve(&messages, Some(&target), Some("token"))
            .await
            .unwrap();
        assert_eq!(
            first.messages[0].blocks[0],
            ContentPart::ImageUrl {
                image_url: kimi_core::types::ImageUrl {
                    url: "ms://file-abc".into(),
                    id: Some("file-abc".into()),
                    name: None,
                    dimensions: None,
                    detail: None,
                },
            }
        );

        let second = MediaResolver::with_store(files)
            .with_upload_cache(sqlite)
            .resolve(&messages, Some(&target), Some("token"))
            .await
            .unwrap();
        assert_eq!(second.messages[0].blocks[0], first.messages[0].blocks[0]);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.await.unwrap();
    }

    async fn spawn_counting_files_server(
        requests: Arc<std::sync::atomic::AtomicUsize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let body = "{\"id\":\"file-abc\"}";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
        });
        (format!("http://{addr}"), server)
    }

    async fn spawn_files_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let _ = sock.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.flush().await;
        });
        (format!("http://{addr}"), server)
    }

    #[tokio::test]
    async fn test_an_upload_capable_provider_gets_a_reference_instead_of_bytes() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);
        let (base_url, server) = spawn_files_server(200, "{\"id\":\"file-abc\"}").await;
        let target = MediaTarget {
            protocol: "openai".into(),
            base_url,
            provider_key: "kimi".into(),
            uploads_media: true,
            ..accepting()
        };
        let messages = [ref_message(&file_id, MediaKind::Image)];

        let out = resolver
            .resolve(&messages, Some(&target), Some("token"))
            .await
            .unwrap();

        assert_eq!(
            out.messages[0].blocks[0],
            ContentPart::ImageUrl {
                image_url: kimi_core::types::ImageUrl {
                    url: "ms://file-abc".into(),
                    id: Some("file-abc".into()),
                    name: None,
                    dimensions: None,
                    detail: None,
                },
            }
        );
        assert_eq!(
            out.entries[0].bytes, 0,
            "a provider-side reference costs no request bytes"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_a_rejected_upload_credential_fails_the_resolve() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);
        let (base_url, server) = spawn_files_server(401, "unauthorized").await;
        let target = MediaTarget {
            protocol: "openai".into(),
            base_url,
            provider_key: "kimi".into(),
            uploads_media: true,
            ..accepting()
        };
        let messages = [ref_message(&file_id, MediaKind::Image)];

        let error = resolver
            .resolve(&messages, Some(&target), Some("bad-token"))
            .await
            .err()
            .expect("a rejected credential fails the resolve");

        assert!(matches!(error, UploadError::Auth(_)), "{error:?}");
        assert!(error.message().contains("401"), "{}", error.message());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_an_upload_capable_provider_without_a_credential_inlines() {
        let (store, file_id) = store_with("a.png", "image/png", PNG_BYTES);
        let resolver = MediaResolver::with_store(store);
        let target = MediaTarget {
            protocol: "openai".into(),
            base_url: "http://127.0.0.1:1".into(),
            provider_key: "kimi".into(),
            uploads_media: true,
            ..accepting()
        };
        let messages = [ref_message(&file_id, MediaKind::Image)];

        let out = resolver
            .resolve(&messages, Some(&target), None)
            .await
            .unwrap();

        assert!(
            matches!(&out.messages[0].blocks[0], ContentPart::ImageUrl { .. }),
            "no credential means no upload: {:?}",
            out.messages[0].blocks[0]
        );
    }

    #[test]
    fn test_a_path_tag_escapes_the_attribute() {
        assert_eq!(
            build_media_path_tag(MediaKind::Image, "/a\"b&c<d>"),
            "<image path=\"/a&quot;b&amp;c&lt;d&gt;\"></image>"
        );
    }

    #[test]
    fn test_a_daemon_file_url_is_recognized_and_its_query_dropped() {
        assert_eq!(parse_daemon_file_url("kimi-file://f_abc"), Some("f_abc"));
        assert_eq!(
            parse_daemon_file_url("kimi-file://f_abc?path=/tmp/x.png"),
            Some("f_abc")
        );
        assert_eq!(parse_daemon_file_url("kimi-file://"), None);
        assert_eq!(parse_daemon_file_url("https://example.test/a.png"), None);
    }

    #[test]
    fn test_a_daemon_url_is_exposed_as_a_media_reference() {
        let blocks = vec![
            media_url_part(
                MediaKind::Image,
                "kimi-file://f_abc".into(),
                None,
                None,
            ),
            media_url_part(
                MediaKind::Video,
                "ms://provider-id".into(),
                Some("provider-id".into()),
                None,
            ),
        ];

        assert_eq!(media_ref_of(&blocks[0]), Some(("f_abc", MediaKind::Image)));
        assert_eq!(media_ref_of(&blocks[1]), None);
    }
}
