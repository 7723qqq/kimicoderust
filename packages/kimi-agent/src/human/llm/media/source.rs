use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaContent {
    pub bytes: Vec<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

#[async_trait]
pub trait MediaSource: Send + Sync {
    async fn get(&self, ref_: &str) -> Result<Option<MediaContent>, WireAsyncError>;
}

#[async_trait]
pub trait MemoryMediaSource: MediaSource {
    fn set(&self, ref_: &str, content: MediaContent);
}

struct MemoryMediaSourceImpl {
    map: Mutex<HashMap<String, MediaContent>>,
}

#[async_trait]
impl MediaSource for MemoryMediaSourceImpl {
    async fn get(&self, ref_: &str) -> Result<Option<MediaContent>, WireAsyncError> {
        Ok(self.map.lock().get(ref_).cloned())
    }
}

#[async_trait]
impl MemoryMediaSource for MemoryMediaSourceImpl {
    fn set(&self, ref_: &str, content: MediaContent) {
        self.map.lock().insert(ref_.to_string(), content);
    }
}

pub fn create_memory_media_source(
    entries: Option<HashMap<String, MediaContent>>,
) -> Arc<dyn MemoryMediaSource> {
    Arc::new(MemoryMediaSourceImpl {
        map: Mutex::new(entries.unwrap_or_default()),
    })
}
