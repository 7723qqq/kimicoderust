use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::human::llm::message::{ContentPart, VideoUrl};
use crate::human::store::backend::backend::BlobBackend;
use crate::human::store::internal::blob::sha256_hex;
use crate::wire::record::WireAsyncError;

const CACHE_PREFIX: &str = "media-upload";

pub type VideoURLPart = ContentPart;

#[async_trait]
pub trait MediaUploadCache: Send + Sync {
    async fn get(&self, ref_: &str, provider_key: &str) -> Result<Option<VideoURLPart>, WireAsyncError>;
    async fn put(
        &self,
        ref_: &str,
        provider_key: &str,
        part: &VideoURLPart,
    ) -> Result<(), WireAsyncError>;
}

pub struct MemoryMediaUploadCache {
    map: Mutex<HashMap<String, VideoURLPart>>,
}

#[async_trait]
impl MediaUploadCache for MemoryMediaUploadCache {
    async fn get(&self, ref_: &str, provider_key: &str) -> Result<Option<VideoURLPart>, WireAsyncError> {
        Ok(self.map.lock().get(&format!("{ref_}\0{provider_key}")).cloned())
    }

    async fn put(
        &self,
        ref_: &str,
        provider_key: &str,
        part: &VideoURLPart,
    ) -> Result<(), WireAsyncError> {
        self.map
            .lock()
            .insert(format!("{ref_}\0{provider_key}"), part.clone());
        Ok(())
    }
}

pub fn create_memory_media_upload_cache() -> Arc<dyn MediaUploadCache> {
    Arc::new(MemoryMediaUploadCache {
        map: Mutex::new(HashMap::new()),
    })
}

pub struct BlobMediaUploadCache {
    blobs: Arc<dyn BlobBackend>,
}

impl BlobMediaUploadCache {
    fn key(&self, ref_: &str, provider_key: &str) -> String {
        sha256_hex(&format!("{CACHE_PREFIX}\0{ref_}\0{provider_key}"))
    }
}

#[async_trait]
impl MediaUploadCache for BlobMediaUploadCache {
    async fn get(&self, ref_: &str, provider_key: &str) -> Result<Option<VideoURLPart>, WireAsyncError> {
        let ref_key = self.key(ref_, provider_key);
        if !self.blobs.has(&ref_key).await? {
            return Ok(None);
        }
        let Ok(raw) = self.blobs.read(&ref_key).await else {
            return Ok(None);
        };
        Ok(parse_cached_part(&raw))
    }

    async fn put(
        &self,
        ref_: &str,
        provider_key: &str,
        part: &VideoURLPart,
    ) -> Result<(), WireAsyncError> {
        let ContentPart::VideoUrl { video_url } = part else {
            return Ok(());
        };
        let ref_key = self.key(ref_, provider_key);
        if let Ok(raw) = serde_json::to_string(video_url) {
            let _ = self.blobs.write(&ref_key, &raw).await;
        }
        Ok(())
    }
}

pub fn create_blob_media_upload_cache(blobs: Arc<dyn BlobBackend>) -> Arc<dyn MediaUploadCache> {
    Arc::new(BlobMediaUploadCache { blobs })
}

fn parse_cached_part(raw: &str) -> Option<VideoURLPart> {
    let data = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    let url = data.get("url")?.as_str().filter(|url| !url.is_empty())?;
    Some(ContentPart::VideoUrl {
        video_url: VideoUrl {
            url: url.to_string(),
            id: data
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            name: None,
        },
    })
}
