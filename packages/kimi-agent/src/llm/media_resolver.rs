//! Media reference resolution (v2 `AgentMediaResolverService`).
//!
//! The host hands the engine a [`ContentBlock::MediaRef`] — a file id, not
//! bytes. This module turns that reference into what the active model can
//! actually take, at request time: inline base64, a provider-side `ms://`
//! reference, or a `<image path="…">` tag when the model cannot take the
//! media at all.
//!
//! Resolving here rather than inlining at the host boundary is what lets the
//! decision follow the model, and what gives the request media budget a
//! stable per-file identity to key on.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::llm::files_upload::UploadError;
use crate::llm::media_budget::Entry;
use crate::rpc::types::{ContentBlock, MediaKind};
use crate::server::files::FileStore;
use kimi_core::types::LLMMessage;

pub use kimi_core::types::ResolvedRequest;

/// v2 `IMAGE_UNAVAILABLE_TEXT` / `VIDEO_UNAVAILABLE_TEXT`: what a reference
/// becomes when its blob is gone (expired, pruned, or never stored).
pub const IMAGE_UNAVAILABLE_TEXT: &str =
    "[image omitted: the uploaded file is no longer available]";
pub const VIDEO_UNAVAILABLE_TEXT: &str =
    "[video omitted: the uploaded file is no longer available]";
pub const AUDIO_UNAVAILABLE_TEXT: &str =
    "[audio omitted: the uploaded file is no longer available]";

/// The placeholder for a reference whose bytes cannot be read.
pub fn unavailable_text(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => IMAGE_UNAVAILABLE_TEXT,
        MediaKind::Video => VIDEO_UNAVAILABLE_TEXT,
        MediaKind::Audio => AUDIO_UNAVAILABLE_TEXT,
    }
}

/// The `state_entries` domain the persisted upload tier lives in (v2's blob
/// store `image-upload-cache` scope).
pub const UPLOAD_CACHE_DOMAIN: &str = "media_upload_cache";

/// v2 `escapeMediaAttribute`.
fn escape_media_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// v2 `buildMediaPathTag`: the tag a model reads to re-open the file itself.
pub fn build_media_path_tag(kind: MediaKind, path: &str) -> String {
    format!(
        "<{} path=\"{}\"></{}>",
        kind.as_str(),
        escape_media_attribute(path),
        kind.as_str()
    )
}

pub use kimi_core::types::MediaTarget;


/// The daemon file id a block references, with the media family the
/// submitting part claimed.
pub fn media_ref_of(block: &ContentBlock) -> Option<(&str, MediaKind)> {
    match block {
        ContentBlock::MediaRef { file_id, kind } => Some((file_id.as_str(), *kind)),
        _ => None,
    }
}

/// The daemon file scheme a client uses to reference an uploaded file
/// (v2 `KIMI_FILE_SCHEME`).
pub const KIMI_FILE_SCHEME: &str = "kimi-file://";

/// The file id a `kimi-file://<id>` URL names (v2 `parseDaemonFileUrl`). A
/// `?query` suffix is dropped: the engine's prompt intake appends the saved
/// path there, and the id is what identifies the file.
pub fn parse_daemon_file_url(url: &str) -> Option<&str> {
    let rest = url.strip_prefix(KIMI_FILE_SCHEME)?;
    let file_id = rest.split('?').next().unwrap_or("");
    (!file_id.is_empty()).then_some(file_id)
}

/// Rewrite `kimi-file://` URL blocks into [`ContentBlock::MediaRef`].
///
/// The client submits a reference as an ordinary media URL; the engine is the
/// side that knows it is a daemon file. Converting at intake is what keeps the
/// reference from reaching a provider as an unfetchable `kimi-file://` URL.
pub fn normalize_media_refs(blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    blocks
        .into_iter()
        .map(|block| {
            let (url, kind) = match &block {
                ContentBlock::ImageUrl { url, .. } => (url, MediaKind::Image),
                ContentBlock::VideoUrl { url, .. } => (url, MediaKind::Video),
                ContentBlock::AudioUrl { url, .. } => (url, MediaKind::Audio),
                _ => return block,
            };
            match parse_daemon_file_url(url) {
                Some(file_id) => ContentBlock::MediaRef {
                    file_id: file_id.to_string(),
                    kind,
                },
                None => block,
            }
        })
        .collect()
}

/// Whether any block in the list still carries an unresolved reference.
pub fn has_media_refs(messages: &[LLMMessage]) -> bool {
    messages
        .iter()
        .any(|message| message.blocks.iter().any(|b| media_ref_of(b).is_some()))
}

/// `data:` plus `;base64,` — what an inline image's wire URL adds to its
/// base64 payload.
const DATA_URL_PREFIX_BYTES: usize = 13;

/// The bytes one resolved block costs on the wire (v2 `inlinePartBytes`): a
/// data URL's length, and nothing for a provider-side reference.
fn inline_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Image {
            media_type, data, ..
        } => DATA_URL_PREFIX_BYTES + media_type.len() + data.len(),
        ContentBlock::ImageUrl { url, .. }
        | ContentBlock::AudioUrl { url, .. }
        | ContentBlock::VideoUrl { url, .. }
            if url.starts_with("data:") =>
        {
            url.len()
        }
        _ => 0,
    }
}

/// v2 `accountHashFor`: an upload belongs to the account that made it, so the
/// memo and the persisted upload cache are scoped by credential. Only a
/// truncated SHA-256 of the credential enters a key — the keys are written to
/// the session store, so the secret itself must never appear in one.
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

/// The budget entries a history carries with no store behind it: inline
/// payloads only, since a reference cannot be read without one.
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

/// The budget identity of a block that carries its own bytes (v2
/// `inlineMediaBudgetEntry`): a payload with no file behind it is keyed by a
/// hash of what it carries, so identical payloads share one budget item.
fn inline_budget(block: &ContentBlock) -> Option<(MediaKind, usize, String)> {
    let (kind, bytes, pieces): (MediaKind, usize, Vec<&str>) = match block {
        ContentBlock::Image {
            media_type, data, ..
        } => (
            MediaKind::Image,
            DATA_URL_PREFIX_BYTES + media_type.len() + data.len(),
            vec![media_type, data],
        ),
        ContentBlock::ImageUrl { url, .. } if url.starts_with("data:") => {
            (MediaKind::Image, url.len(), vec![url])
        }
        ContentBlock::VideoUrl { url, .. } if url.starts_with("data:") => {
            (MediaKind::Video, url.len(), vec![url])
        }
        ContentBlock::AudioUrl { url, .. } if url.starts_with("data:") => {
            (MediaKind::Audio, url.len(), vec![url])
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


/// v2 `IMAGE_MEMO_MAX_BYTES` / `IMAGE_MEMO_MAX_TOTAL_BYTES`: the bound on the
/// inline-payload memo. An entry larger than the per-item cap is not memoized
/// at all; the total is enforced oldest-first, like v2's `memoizeImage`.
const INLINE_MEMO_MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const INLINE_MEMO_MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// The bounded memo for resolved *inline* payloads (v2 `imageMemo`).
///
/// Inline base64 is bytes, not an identity: it is expensive to keep and
/// re-reading the blob is always possible, so it must never join the
/// unbounded [`MediaResolver::resolved`] map. It still has to be cached —
/// re-encoding a multi-megabyte image on every step of a turn is worse — so
/// this map carries v2's byte caps.
#[derive(Default)]
struct InlineMemo {
    entries: HashMap<String, (ContentBlock, usize)>,
    /// Insertion order, oldest first — the eviction order.
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

impl InlineMemo {
    fn get(&mut self, key: &str) -> Option<ContentBlock> {
        let block = self.entries.get(key)?.0.clone();
        // Touch: v2 deletes and re-inserts on a hit, so a hot entry survives
        // the oldest-first sweep.
        if let Some(position) = self.order.iter().position(|existing| existing == key) {
            self.order.remove(position);
        }
        self.order.push_back(key.to_string());
        Some(block)
    }

    fn insert(&mut self, key: String, block: ContentBlock, bytes: usize) {
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

/// Resolves [`ContentBlock::MediaRef`] blocks against the daemon file store.
///
/// One resolver serves a session: the memo it keeps is what stops a media
/// item from being re-read — or re-uploaded — on every step of a turn.
pub struct MediaResolver {
    store: FileStore,
    /// Resolved blocks that are *identities* — a provider-side `ms://`
    /// reference or a small text placeholder — keyed by the full upload
    /// identity (v2's `media.resolved` map). Inline payloads go to
    /// [`InlineMemo`] instead, because this map has no size bound.
    resolved: Mutex<HashMap<String, ContentBlock>>,
    /// Inline payloads, bounded (v2 `imageMemo`).
    inline_memo: Mutex<InlineMemo>,
    /// Providers that answered an upload with "no such route" (v2
    /// `imageUploadUnsupported`): the resolver stops trying for them.
    upload_unsupported: Mutex<std::collections::HashSet<String>>,
    /// The persisted upload tier (v2's blob-store `image-upload-cache`
    /// scope): the full upload identity → provider file id, so a daemon
    /// restart reuses the provider-side upload instead of repeating it.
    /// `None` keeps the memo process-only (tests, store-less hosts).
    upload_cache: Option<Arc<crate::session::sqlite_store::SqliteSessionStore>>,
}

impl Default for MediaResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaResolver {
    /// A resolver over the daemon's own file store.
    pub fn new() -> Self {
        Self::with_store(FileStore::new())
    }

    /// A resolver over an explicit store (tests).
    pub fn with_store(store: FileStore) -> Self {
        Self {
            store,
            resolved: Mutex::new(HashMap::new()),
            inline_memo: Mutex::new(InlineMemo::default()),
            upload_unsupported: Mutex::new(std::collections::HashSet::new()),
            upload_cache: None,
        }
    }

    /// Persist successful uploads through `store` (v2's `image-upload-cache`
    /// scope): a later resolver over the same store reuses the provider-side
    /// file instead of uploading again.
    pub fn with_upload_cache(
        mut self,
        store: Arc<crate::session::sqlite_store::SqliteSessionStore>,
    ) -> Self {
        self.upload_cache = Some(store);
        self
    }

    /// Every media entry of `messages`, in message order — the reference
    /// entries (with their saved paths) plus the inline ones. The degrade /
    /// strip recoveries run on the *unresolved* messages, where a reference
    /// still names its file, so this is the entry list they transform by.
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

    /// Rewrite every reference in `messages` into a request-ready block, and
    /// collect the media the budget must weigh.
    ///
    /// `credential` is the provider credential an upload would authenticate
    /// with; `None` inlines instead. A history with neither references nor
    /// inline media is returned borrowed, so a text-only turn costs nothing.
    ///
    /// `Err` carries a failure that must abort the request: a rejected upload
    /// credential (v2 `isMediaUploadAuthError` → throw,
    /// mediaResolverService.ts:356), or a referenced file whose metadata could
    /// not be read — the user's login is broken, or the store failed, so the
    /// request fails with the real reason instead of degrading the media and
    /// letting the chat request report a different failure.
    pub async fn resolve<'a>(
        &self,
        messages: &'a [LLMMessage],
        target: Option<&MediaTarget>,
        credential: Option<&str>,
    ) -> Result<ResolvedRequest<'a>, UploadError> {
        let mut entries = Vec::new();
        let mut out: Option<Vec<LLMMessage>> = None;
        for (message_index, message) in messages.iter().enumerate() {
            let mut blocks: Option<Vec<ContentBlock>> = None;
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
                        // The blocks before this one are carried over as they
                        // were; only the reference is replaced.
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

    /// The saved path of one referenced file, for a caller that wants to show
    /// it (v2 `displayPaths`).
    ///
    /// A missing file is `None`; a metadata read that failed for any other
    /// reason is also `None` (there is no path to show), but it is logged —
    /// this side channel must not make an unreadable index look like an
    /// ordinary miss.
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

    /// The memo / upload-cache identity of one reference — v2's
    /// `image\0fileId\0providerKey\0protocol\0baseUrl\0accountHash`
    /// (`mediaResolverService.ts:257,396`). Everything that can change what a
    /// provider-side id means is in it: a memo keyed only by file + provider
    /// would hand one account's upload to another after an account or
    /// endpoint switch.
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

    /// The memo identity of an inline payload. The bytes are the same for
    /// every account, so the credential is not part of it — but the model
    /// capabilities that decide whether the media is inlined at all are, or a
    /// provider key shared by two models would let one model's data URL answer
    /// the other's placeholder.
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
    ) -> Result<(ContentBlock, Option<String>), UploadError> {
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
        // The persisted upload tier (v2 `image-upload-cache`): a provider
        // file id recorded by an earlier process answers without an upload.
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
            // Base64 bytes never enter the unbounded `resolved` map; the
            // bounded memo caps them the way v2's `imageMemo` does.
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

    /// The provider file id an earlier process uploaded this file as, for this
    /// exact upload identity (v2's cache scope read).
    fn cached_upload(&self, cache_key: &str) -> Option<String> {
        let store = self.upload_cache.as_ref()?;
        match store.get_state(UPLOAD_CACHE_DOMAIN, cache_key) {
            Ok(value) => value
                .and_then(|value| value.as_str().map(str::to_string))
                .filter(|id| !id.is_empty()),
            // A cache read failure costs one redundant upload; the request
            // still goes out. It is not the same as "never uploaded", so it is
            // logged instead of silently reading as a miss.
            Err(error) => {
                tracing::warn!(
                    %error,
                    "failed to read the persisted media upload cache entry"
                );
                None
            }
        }
    }

    /// Record a successful upload for later processes (v2's cache scope
    /// write). A failed write costs the next process one re-upload, so it is
    /// logged, not propagated.
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
    ) -> Result<ContentBlock, UploadError> {
        // Only a 404 says the file is gone (the store prunes entries whose
        // blob vanished); every other error is a read failure and must not be
        // told to the model as "no longer available".
        let (meta, path) = match self.store.get(file_id) {
            Ok(found) => found,
            Err((404, _, _)) => {
                return Ok(ContentBlock::Text {
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
        // A model that cannot take this media family still gets the path: the
        // tag is what lets it re-open the file with its own tools.
        if target.is_some_and(|t| !t.accepts(kind)) {
            return Ok(ContentBlock::Text {
                text: build_media_path_tag(kind, &path),
                meta: None,
            });
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            // v2 `degradedImage(path)` (mediaResolverService.ts:224,245 →
            // media/pathTag.ts:29-31): a read failure is not evidence the
            // blob is gone, so keep the path for a path-based tool instead of
            // claiming it no longer exists.
            Err(error) => {
                tracing::warn!(
                    file_id,
                    %path,
                    %error,
                    "failed to read the media blob; degrading to a path tag"
                );
                return Ok(ContentBlock::Text {
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
                let Some(sniffed) = crate::native::file_type::sniff_media_from_magic(header)
                else {
                    return Ok(ContentBlock::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                };
                if sniffed.kind != crate::native::file_type::MediaSniffKind::Image {
                    return Ok(ContentBlock::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                }
                let effective =
                    crate::llm::image_policy::normalize_image_mime(sniffed.mime_type);
                let provider = target
                    .map(|t| t.provider_type.as_str())
                    .filter(|value| !value.is_empty());
                if !crate::llm::image_policy::is_model_accepted_image_mime(&effective, provider) {
                    return Ok(ContentBlock::Text {
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
                    return Ok(ContentBlock::Text {
                        text: build_media_path_tag(kind, &path),
                        meta: None,
                    });
                };
                if sniffed.kind != crate::native::file_type::MediaSniffKind::Video {
                    return Ok(ContentBlock::Text {
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
            return Ok(ContentBlock::Text {
                text: build_media_path_tag(kind, &path),
                meta: None,
            });
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(match kind {
            MediaKind::Image => ContentBlock::Image {
                media_type,
                data: encoded,
                name,
            },
            MediaKind::Video => ContentBlock::VideoUrl {
                url: format!("data:{media_type};base64,{encoded}"),
                id: None,
                name,
            },
            MediaKind::Audio => ContentBlock::AudioUrl {
                url: format!("data:{media_type};base64,{encoded}"),
                id: None,
                name,
            },
        })
    }

    /// The provider-side reference for one file, when the provider takes media
    /// that way (v2 `uploadImagePart` / `resolveVideoPart`). `Ok(None)` means
    /// the caller inlines instead; `Err` carries a rejected credential, which
    /// v2 throws rather than degrading (mediaResolverService.ts:356).
    async fn upload(
        &self,
        file_id: &str,
        kind: MediaKind,
        target: Option<&MediaTarget>,
        credential: Option<&str>,
        media_type: &str,
        filename: &str,
        bytes: &[u8],
    ) -> Result<Option<ContentBlock>, UploadError> {
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
            // The files API has no audio purpose; audio stays inline.
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
            // v2 `isMediaUploadAuthError` → throw: the user's login is
            // broken, so the request fails with the upload's own error
            // instead of degrading the media to a path tag and letting the
            // chat request report a different failure (ROADMAP #17 ③).
            Err(error @ crate::llm::files_upload::UploadError::Auth(_)) => Err(error),
            Err(crate::llm::files_upload::UploadError::Other(_)) => Ok(None),
        }
    }
}

/// The `ms://<id>` block a successful upload becomes (v2 `uploadImage` /
/// `uploadVideo`).
fn reference_block(kind: MediaKind, id: &str) -> ContentBlock {
    let url = format!("ms://{id}");
    match kind {
        MediaKind::Image => ContentBlock::ImageUrl {
            url,
            id: Some(id.to_string()),
            name: None,
        },
        MediaKind::Video => ContentBlock::VideoUrl {
            url,
            id: Some(id.to_string()),
            name: None,
        },
        MediaKind::Audio => ContentBlock::AudioUrl {
            url,
            id: Some(id.to_string()),
            name: None,
        },
    }
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
            role: "user".into(),
            content: String::new(),
            blocks: vec![ContentBlock::MediaRef {
                file_id: file_id.to_string(),
                kind,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
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
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(PNG_BYTES),
                name: Some("a.png".into()),
            }
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

        let ContentBlock::Text { text, .. } = &out.messages[0].blocks[0] else {
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
                ContentBlock::VideoUrl { url, .. }
                    if url.starts_with("data:video/mp4;base64,")
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
        let ContentBlock::Text { text, .. } = &out.messages[0].blocks[0] else {
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

        let ContentBlock::Text { text, .. } = &out.messages[0].blocks[0] else {
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
        let out = resolver.resolve(&messages, Some(&openai), None).await.unwrap();
        let ContentBlock::Text { text, .. } = &out.messages[0].blocks[0] else {
            panic!("expected a path tag, got {:?}", out.messages[0].blocks[0]);
        };
        assert!(text.starts_with("<image path=\""), "{text}");

        let kimi = MediaTarget {
            provider_key: "kimi".into(),
            provider_type: "kimi".into(),
            ..accepting()
        };
        let out = resolver.resolve(&messages, Some(&kimi), None).await.unwrap();
        assert_eq!(
            out.messages[0].blocks[0],
            ContentBlock::Image {
                media_type: "image/heic".into(),
                data: base64::engine::general_purpose::STANDARD.encode(heic),
                name: Some("a.heic".into()),
            }
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
            MediaResolver::cache_key(
                "f_1",
                MediaKind::Image,
                Some(&accepting()),
                Some(&first)
            ),
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

    /// v2 keys its memo and its persisted upload cache by
    /// `fileId / providerKey / protocol / baseUrl / accountHash`
    /// (`mediaResolverService.ts:257,396`). An account or endpoint switch must
    /// not reuse another identity's provider-side upload, and the credential
    /// itself must never be written into a key (these keys are persisted).
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

    /// Inline base64 is bytes, not an identity: it goes to the bounded memo,
    /// never to the unbounded `resolved` map.
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

    /// v2 `IMAGE_MEMO_MAX_BYTES` / `IMAGE_MEMO_MAX_TOTAL_BYTES`: an entry above
    /// the per-item cap is not memoized, and the total evicts oldest-first.
    #[test]
    fn test_the_inline_memo_is_bounded() {
        let mut memo = InlineMemo::default();
        let block = ContentBlock::Text {
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
            ContentBlock::Text {
                text: VIDEO_UNAVAILABLE_TEXT.into(),
                meta: None,
            }
        );
        assert_eq!(out.entries[0].bytes, 0);
    }

    #[tokio::test]
    async fn test_a_history_without_media_is_returned_borrowed() {
        let resolver = MediaResolver::new();
        let messages = vec![LLMMessage::new("user", "hello")];

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
            role: "user".into(),
            content: String::new(),
            blocks: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
                name: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
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
        assert_eq!(out.entries[0].bytes, DATA_URL_PREFIX_BYTES + 9 + 4);
    }

    #[tokio::test]
    async fn test_a_remote_url_is_not_budgeted() {
        let resolver = MediaResolver::new();
        let messages = vec![LLMMessage {
            role: "user".into(),
            content: String::new(),
            blocks: vec![ContentBlock::ImageUrl {
                url: "https://example.test/huge.png".into(),
                id: None,
                name: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
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
        // The blob is gone, but the memo still answers: the file was already
        // read once for this provider.
        let second_messages = [message];
        let second = resolver
            .resolve(&second_messages, Some(&accepting()), None)
            .await
            .unwrap();

        assert_eq!(first.messages[0].blocks[0], second.messages[0].blocks[0]);
    }

    /// v2 keeps the upload result in the blob store's `image-upload-cache`
    /// scope, so a daemon restart reuses the provider-side file instead of
    /// uploading again (ROADMAP #17 ①). The fork's tier is a `state_entries`
    /// domain keyed by the full upload identity (kind / file / provider /
    /// protocol / endpoint / account); a fresh resolver over the same store
    /// must answer from it without a second request.
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
            ContentBlock::ImageUrl {
                url: "ms://file-abc".into(),
                id: Some("file-abc".into()),
                name: None,
            }
        );

        // A fresh resolver (the restart) answers from the persisted tier: the
        // same reference, and the files API saw exactly one request.
        let second = MediaResolver::with_store(files)
            .with_upload_cache(sqlite)
            .resolve(&messages, Some(&target), Some("token"))
            .await
            .unwrap();
        assert_eq!(second.messages[0].blocks[0], first.messages[0].blocks[0]);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.await.unwrap();
    }

    /// A mock files API that counts its requests and always answers with a
    /// file id. It serves exactly one connection and returns: a wrong second
    /// upload attempt is then refused, which fails the block assertion — the
    /// counter is the diagnostic for which side went wrong.
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

    /// A mock files API: one request, answered with `status` and `body`.
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
            ContentBlock::ImageUrl {
                url: "ms://file-abc".into(),
                id: Some("file-abc".into()),
                name: None,
            }
        );
        assert_eq!(
            out.entries[0].bytes, 0,
            "a provider-side reference costs no request bytes"
        );
        server.await.unwrap();
    }

    /// v2 `isMediaUploadAuthError` → throw (mediaResolverService.ts:356): a
    /// rejected credential fails the resolve with the upload's own error
    /// instead of degrading the media to a path tag and letting the chat
    /// request report a different failure (ROADMAP #17 ③).
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
            matches!(&out.messages[0].blocks[0], ContentBlock::Image { .. }),
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
    fn test_normalize_turns_a_daemon_url_into_a_reference() {
        let blocks = vec![
            ContentBlock::ImageUrl {
                url: "kimi-file://f_abc".into(),
                id: None,
                name: None,
            },
            ContentBlock::VideoUrl {
                url: "ms://provider-id".into(),
                id: Some("provider-id".into()),
                name: None,
            },
        ];

        let out = normalize_media_refs(blocks);

        assert_eq!(
            out[0],
            ContentBlock::MediaRef {
                file_id: "f_abc".into(),
                kind: MediaKind::Image,
            }
        );
        assert!(
            matches!(&out[1], ContentBlock::VideoUrl { url, .. } if url == "ms://provider-id"),
            "a provider reference is not a daemon file"
        );
    }
}
