use std::sync::Arc;

use async_trait::async_trait;

use crate::persistence::interface::blob_store::{
    BlobByteStream, BlobReadRange, IBlobStore,
};
use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageListOptions, StorageReadRange, StorageWriteOptions,
};
use crate::wire::record::WireAsyncError;

pub struct BlobStoreService {
    storage: Arc<dyn IFileSystemStorageService>,
}

impl BlobStoreService {
    pub fn new(storage: Arc<dyn IFileSystemStorageService>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl IBlobStore for BlobStoreService {
    async fn put(&self, scope: &str, key: &str, data: Vec<u8>) -> Result<(), WireAsyncError> {
        self.storage
            .write(
                scope,
                key,
                data,
                Some(StorageWriteOptions {
                    atomic: Some(true),
                    signal: None,
                }),
            )
            .await
    }

    async fn put_stream(
        &self,
        scope: &str,
        key: &str,
        source: BlobByteStream,
    ) -> Result<(), WireAsyncError> {
        self.storage
            .write_stream(
                scope,
                key,
                source,
                Some(StorageWriteOptions {
                    atomic: Some(true),
                    signal: None,
                }),
            )
            .await
    }

    async fn get(&self, scope: &str, key: &str) -> Result<Option<Vec<u8>>, WireAsyncError> {
        self.storage.read(scope, key).await
    }

    fn get_stream(
        &self,
        scope: &str,
        key: &str,
        range: Option<BlobReadRange>,
    ) -> BlobByteStream {
        self.storage.read_stream(
            scope,
            key,
            range.map(|range| StorageReadRange {
                start: range.start,
                end: range.end,
            }),
        )
    }

    async fn has(&self, scope: &str, key: &str) -> Result<bool, WireAsyncError> {
        Ok(self.storage.size(scope, key).await?.is_some())
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        self.storage.delete(scope, key).await
    }

    async fn list(
        &self,
        scope: &str,
        prefix: Option<&str>,
        options: Option<StorageListOptions>,
    ) -> Result<Vec<String>, WireAsyncError> {
        self.storage.list(scope, prefix, options).await
    }
}
