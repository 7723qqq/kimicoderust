use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use parking_lot::Mutex;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::human::llm::message::ContentPart;
use crate::persistence::interface::blobStore::IBlobStore;
use crate::wire::record::WireAsyncError;

use super::agent_blob_service::{BLOBREF_PROTOCOL, IAgentBlobService, MISSING_MEDIA_PLACEHOLDER};
use super::byte_lru_cache::ByteLruCache;

const DEFAULT_THRESHOLD: usize = 4096;
const DEFAULT_MAX_CACHE_SIZE: usize = 50 * 1024 * 1024;
static DATA_URI_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^data:([^;]+);base64,").unwrap());
static LENIENT_BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

pub struct AgentBlobServiceImpl {
    blobs: Arc<dyn IBlobStore>,
    storage_scope: String,
    cache: Mutex<ByteLruCache>,
}

impl AgentBlobServiceImpl {
    pub fn new(blobs: Arc<dyn IBlobStore>, storage_scope: String) -> Self {
        Self {
            blobs,
            storage_scope,
            cache: Mutex::new(ByteLruCache::new(DEFAULT_MAX_CACHE_SIZE)),
        }
    }

    fn threshold(&self) -> usize {
        DEFAULT_THRESHOLD
    }

    async fn offload_content_part(&self, part: &ContentPart) -> Result<ContentPart, WireAsyncError> {
        self.rewrite_media_urls(part, |url| self.maybe_offload_string(url))
            .await
    }

    async fn load_content_part(&self, part: &ContentPart) -> Result<ContentPart, WireAsyncError> {
        self.rewrite_media_urls(part, |url| async move {
            if !self.is_blob_ref(&url) {
                return Ok(url);
            }
            Ok(self
                .load_blob_ref_url(&url)
                .await
                .unwrap_or_else(|| MISSING_MEDIA_PLACEHOLDER.to_string()))
        })
        .await
    }

    async fn rewrite_media_urls<F, Fut>(
        &self,
        part: &ContentPart,
        mut transform: F,
    ) -> Result<ContentPart, WireAsyncError>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<String, WireAsyncError>>,
    {
        let mut next = part.clone();
        match &mut next {
            ContentPart::ImageUrl { image_url } => {
                image_url.url = transform(image_url.url.clone()).await?;
            }
            ContentPart::AudioUrl { audio_url } => {
                audio_url.url = transform(audio_url.url.clone()).await?;
            }
            ContentPart::VideoUrl { video_url } => {
                video_url.url = transform(video_url.url.clone()).await?;
            }
            ContentPart::DocumentUrl { document_url } => {
                document_url.url = transform(document_url.url.clone()).await?;
            }
            _ => {}
        }
        Ok(next)
    }

    async fn load_blob_ref_url(&self, url: &str) -> Option<String> {
        let (mime_type, hash) = parse_blob_ref(url)?;
        let payload = self.read_blob(&hash).await?;
        Some(format_data_uri(&mime_type, &payload))
    }

    async fn read_blob(&self, hash: &str) -> Option<Vec<u8>> {
        if let Some(cached) = self.cache.lock().get(hash) {
            return Some(cached);
        }
        let payload = self
            .blobs
            .get(&self.storage_scope, hash)
            .await
            .ok()
            .flatten()?;
        self.cache.lock().set(hash, payload.clone());
        Some(payload)
    }

    async fn maybe_offload_string(&self, value: String) -> Result<String, WireAsyncError> {
        if self.is_blob_ref(&value) {
            return Ok(value);
        }
        let Some(captures) = DATA_URI_HEADER_RE.captures(&value) else {
            return Ok(value);
        };
        let header = captures.get(0).unwrap();
        let mime_type = captures.get(1).unwrap().as_str();
        let payload = &value[header.end()..];
        if payload.encode_utf16().count() < self.threshold() {
            return Ok(value);
        }
        self.write_blob(mime_type, payload).await
    }

    async fn write_blob(&self, mime_type: &str, base64_payload: &str) -> Result<String, WireAsyncError> {
        let hash = format!("{:x}", Sha256::digest(base64_payload.as_bytes()));
        let binary = decode_lenient_base64(base64_payload);
        self.blobs
            .put(&self.storage_scope, &hash, binary.clone())
            .await?;
        self.cache.lock().set(&hash, binary);
        Ok(format_blob_ref(mime_type, &hash))
    }
}

#[async_trait]
impl IAgentBlobService for AgentBlobServiceImpl {
    async fn offload_parts(&self, parts: &[ContentPart]) -> Result<Vec<ContentPart>, WireAsyncError> {
        let mut out = Vec::with_capacity(parts.len());
        for part in parts {
            out.push(self.offload_content_part(part).await?);
        }
        Ok(out)
    }

    async fn load_parts(&self, parts: &[ContentPart]) -> Result<Vec<ContentPart>, WireAsyncError> {
        let mut out = Vec::with_capacity(parts.len());
        for part in parts {
            out.push(self.load_content_part(part).await?);
        }
        Ok(out)
    }

    fn is_blob_ref(&self, url: &str) -> bool {
        url.starts_with(BLOBREF_PROTOCOL)
    }
}

fn format_blob_ref(mime_type: &str, hash: &str) -> String {
    format!("{BLOBREF_PROTOCOL}{mime_type};{hash}")
}

fn parse_blob_ref(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix(BLOBREF_PROTOCOL)?;
    let (mime_type, hash) = rest.split_once(';')?;
    if hash.is_empty() {
        return None;
    }
    Some((mime_type.to_string(), hash.to_string()))
}

fn format_data_uri(mime_type: &str, payload: &[u8]) -> String {
    format!(
        "data:{mime_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(payload)
    )
}

fn decode_lenient_base64(payload: &str) -> Vec<u8> {
    let mut cleaned = Vec::with_capacity(payload.len());
    for unit in payload.encode_utf16() {
        let byte = unit as u8;
        match byte {
            b'=' => break,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => cleaned.push(byte),
            b'-' => cleaned.push(b'+'),
            b'_' => cleaned.push(b'/'),
            _ => {}
        }
    }
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    LENIENT_BASE64.decode(cleaned).unwrap_or_default()
}
