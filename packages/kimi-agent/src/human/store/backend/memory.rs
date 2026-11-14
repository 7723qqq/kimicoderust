use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::human::store::backend::backend::{BlobBackend, StoreBackend, TreeBackend};
use crate::human::store::types::StoreError;

fn js_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    values
}

#[derive(Default)]
pub struct MemoryTreeBackend {
    pub files: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
}

#[async_trait]
impl TreeBackend for MemoryTreeBackend {
    async fn list(&self) -> Result<Vec<String>, StoreError> {
        Ok(js_sorted(self.files.lock().keys().cloned().collect()))
    }

    async fn list_branches(&self, tree: &str) -> Result<Vec<String>, StoreError> {
        Ok(self
            .files
            .lock()
            .get(tree)
            .map(|file| js_sorted(file.keys().cloned().collect()))
            .unwrap_or_default())
    }

    async fn read(&self, tree: &str, branch: &str) -> Result<String, StoreError> {
        self.files
            .lock()
            .get(tree)
            .and_then(|file| file.get(branch))
            .cloned()
            .ok_or_else(|| {
                StoreError::new("ENOENT", format!("ENOENT: no such branch {tree}/{branch}"))
            })
    }

    async fn append(&self, tree: &str, branch: &str, data: &str) -> Result<(), StoreError> {
        let mut files = self.files.lock();
        let file = files.entry(tree.to_string()).or_default();
        file.entry(branch.to_string())
            .and_modify(|content| content.push_str(data))
            .or_insert_with(|| data.to_string());
        Ok(())
    }

    async fn write(&self, tree: &str, branch: &str, content: &str) -> Result<(), StoreError> {
        self.files
            .lock()
            .entry(tree.to_string())
            .or_default()
            .insert(branch.to_string(), content.to_string());
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryBlobBackend {
    pub files: Mutex<BTreeMap<String, String>>,
}

#[async_trait]
impl BlobBackend for MemoryBlobBackend {
    async fn has(&self, reference: &str) -> Result<bool, StoreError> {
        Ok(self.files.lock().contains_key(reference))
    }

    async fn read(&self, reference: &str) -> Result<String, StoreError> {
        self.files.lock().get(reference).cloned().ok_or_else(|| {
            StoreError::new("ENOENT", format!("ENOENT: no such blob {reference}"))
        })
    }

    async fn write(&self, reference: &str, data: &str) -> Result<(), StoreError> {
        self.files
            .lock()
            .insert(reference.to_string(), data.to_string());
        Ok(())
    }
}

#[derive(Default)]
pub struct MemoryBackend {
    pub trees: Arc<MemoryTreeBackend>,
    pub blobs: Arc<MemoryBlobBackend>,
}

impl MemoryBackend {
    pub fn store_backend(&self) -> StoreBackend {
        self.into()
    }
}

impl From<&MemoryBackend> for StoreBackend {
    fn from(backend: &MemoryBackend) -> Self {
        Self {
            trees: backend.trees.clone(),
            blobs: backend.blobs.clone(),
        }
    }
}
