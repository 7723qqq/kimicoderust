use std::sync::Arc;

use async_trait::async_trait;

use crate::human::store::types::StoreError;

#[async_trait]
pub trait TreeBackend: Send + Sync {
    async fn list(&self) -> Result<Vec<String>, StoreError>;
    async fn list_branches(&self, tree: &str) -> Result<Vec<String>, StoreError>;
    async fn read(&self, tree: &str, branch: &str) -> Result<String, StoreError>;
    async fn append(&self, tree: &str, branch: &str, data: &str) -> Result<(), StoreError>;
    async fn write(&self, tree: &str, branch: &str, content: &str) -> Result<(), StoreError>;
    async fn sync(&self, _tree: &str, _branch: &str) -> Result<(), StoreError> {
        Ok(())
    }
}

#[async_trait]
pub trait BlobBackend: Send + Sync {
    async fn has(&self, reference: &str) -> Result<bool, StoreError>;
    async fn read(&self, reference: &str) -> Result<String, StoreError>;
    async fn write(&self, reference: &str, data: &str) -> Result<(), StoreError>;
}

#[derive(Clone)]
pub struct StoreBackend {
    pub trees: Arc<dyn TreeBackend>,
    pub blobs: Arc<dyn BlobBackend>,
}
