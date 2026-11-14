use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;

use crate::wire::record::WireAsyncError;

use super::storage::StorageListOptions;

pub type BlobByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, WireAsyncError>> + Send>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobReadRange {
    pub start: u64,
    pub end: u64,
}

#[async_trait]
pub trait IBlobStore: Send + Sync {
    async fn put(&self, scope: &str, key: &str, data: Vec<u8>) -> Result<(), WireAsyncError>;
    async fn put_stream(
        &self,
        scope: &str,
        key: &str,
        source: BlobByteStream,
    ) -> Result<(), WireAsyncError>;
    async fn get(&self, scope: &str, key: &str) -> Result<Option<Vec<u8>>, WireAsyncError>;
    fn get_stream(
        &self,
        scope: &str,
        key: &str,
        range: Option<BlobReadRange>,
    ) -> BlobByteStream;
    async fn has(&self, scope: &str, key: &str) -> Result<bool, WireAsyncError>;
    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError>;
    async fn list(
        &self,
        scope: &str,
        prefix: Option<&str>,
        options: Option<StorageListOptions>,
    ) -> Result<Vec<String>, WireAsyncError>;
}
