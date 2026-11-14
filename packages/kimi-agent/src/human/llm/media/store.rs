use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::human::llm::media::source::{MediaContent, MediaSource};
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait MediaStore: MediaSource {
    async fn put(&self, content: MediaContent) -> Result<String, WireAsyncError>;
}

struct MemoryMediaStore {
    map: Mutex<HashMap<String, MediaContent>>,
}

#[async_trait]
impl MediaSource for MemoryMediaStore {
    async fn get(&self, ref_: &str) -> Result<Option<MediaContent>, WireAsyncError> {
        Ok(self.map.lock().get(ref_).cloned())
    }
}

#[async_trait]
impl MediaStore for MemoryMediaStore {
    async fn put(&self, content: MediaContent) -> Result<String, WireAsyncError> {
        let ref_ = format!("{:x}", Sha256::digest(&content.bytes));
        self.map.lock().insert(ref_.clone(), content);
        Ok(ref_)
    }
}

pub fn create_memory_media_store() -> Arc<dyn MediaStore> {
    Arc::new(MemoryMediaStore {
        map: Mutex::new(HashMap::new()),
    })
}
