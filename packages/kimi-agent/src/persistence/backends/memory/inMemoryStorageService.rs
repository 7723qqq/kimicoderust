use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageAppendOptions, StorageByteStream, StorageListOptions,
    StorageReadRange, StorageWriteOptions,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone, Default)]
pub struct InMemoryStorageService {
    scopes: Arc<Mutex<IndexMap<String, IndexMap<String, Vec<u8>>>>>,
    mtimes: Arc<Mutex<HashMap<String, i64>>>,
}

impl InMemoryStorageService {
    pub fn new() -> Self {
        Self::default()
    }

    fn key_for(scope: &str, key: &str) -> String {
        format!("{scope}\0{key}")
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

#[async_trait]
impl IFileSystemStorageService for InMemoryStorageService {
    async fn read(&self, scope: &str, key: &str) -> Result<Option<Vec<u8>>, WireAsyncError> {
        Ok(self
            .scopes
            .lock()
            .get(scope)
            .and_then(|bucket| bucket.get(key))
            .cloned())
    }

    fn read_stream(
        &self,
        scope: &str,
        key: &str,
        range: Option<StorageReadRange>,
    ) -> StorageByteStream {
        let data = self
            .scopes
            .lock()
            .get(scope)
            .and_then(|bucket| bucket.get(key))
            .cloned();
        let data = data.and_then(|data| {
            let Some(range) = range else {
                return Some(data);
            };
            let start = range.start.min(data.len() as u64) as usize;
            let end = range
                .end
                .saturating_add(1)
                .min(data.len() as u64) as usize;
            if start < end {
                Some(data[start..end].to_vec())
            } else {
                None
            }
        });
        Box::pin(futures_util::stream::iter(
            data.into_iter().map(Ok::<Vec<u8>, WireAsyncError>),
        ))
    }

    async fn write(
        &self,
        scope: &str,
        key: &str,
        data: Vec<u8>,
        options: Option<StorageWriteOptions>,
    ) -> Result<(), WireAsyncError> {
        if let Some(signal) = options.and_then(|options| options.signal) {
            signal.throw_if_aborted()?;
        }
        self.scopes
            .lock()
            .entry(scope.to_string())
            .or_default()
            .insert(key.to_string(), data);
        self.mtimes
            .lock()
            .insert(Self::key_for(scope, key), Self::now());
        Ok(())
    }

    async fn write_stream(
        &self,
        scope: &str,
        key: &str,
        mut source: StorageByteStream,
        options: Option<StorageWriteOptions>,
    ) -> Result<(), WireAsyncError> {
        let signal = options.and_then(|options| options.signal);
        let mut merged = Vec::new();
        while let Some(chunk) = source.next().await {
            let chunk = chunk?;
            if let Some(signal) = &signal {
                signal.throw_if_aborted()?;
            }
            merged.extend(chunk);
        }
        if let Some(signal) = &signal {
            signal.throw_if_aborted()?;
        }
        self.scopes
            .lock()
            .entry(scope.to_string())
            .or_default()
            .insert(key.to_string(), merged);
        self.mtimes
            .lock()
            .insert(Self::key_for(scope, key), Self::now());
        Ok(())
    }

    async fn append(
        &self,
        scope: &str,
        key: &str,
        data: Vec<u8>,
        _options: Option<StorageAppendOptions>,
    ) -> Result<(), WireAsyncError> {
        let mut scopes = self.scopes.lock();
        let bucket = scopes.entry(scope.to_string()).or_default();
        if let Some(existing) = bucket.get_mut(key) {
            existing.extend(data);
        } else {
            bucket.insert(key.to_string(), data);
        }
        drop(scopes);
        self.mtimes
            .lock()
            .insert(Self::key_for(scope, key), Self::now());
        Ok(())
    }

    async fn list(
        &self,
        scope: &str,
        prefix: Option<&str>,
        _options: Option<StorageListOptions>,
    ) -> Result<Vec<String>, WireAsyncError> {
        let scopes = self.scopes.lock();
        let Some(bucket) = scopes.get(scope) else {
            return Ok(Vec::new());
        };
        Ok(bucket
            .keys()
            .filter(|key| prefix.is_none_or(|prefix| key.starts_with(prefix)))
            .cloned()
            .collect())
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        if let Some(bucket) = self.scopes.lock().get_mut(scope) {
            bucket.shift_remove(key);
        }
        self.mtimes.lock().remove(&Self::key_for(scope, key));
        Ok(())
    }

    async fn size(&self, scope: &str, key: &str) -> Result<Option<u64>, WireAsyncError> {
        Ok(self
            .scopes
            .lock()
            .get(scope)
            .and_then(|bucket| bucket.get(key))
            .map(|data| data.len() as u64))
    }

    fn size_sync(&self, scope: &str, key: &str) -> Option<u64> {
        self.scopes
            .lock()
            .get(scope)
            .and_then(|bucket| bucket.get(key))
            .map(|data| data.len() as u64)
    }

    async fn mtime(&self, scope: &str, key: &str) -> Result<Option<i64>, WireAsyncError> {
        Ok(self.mtimes.lock().get(&Self::key_for(scope, key)).copied())
    }

    fn path_for(&self, _scope: &str, _key: &str) -> Option<String> {
        None
    }

    async fn flush(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }
}
