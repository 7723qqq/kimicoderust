use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::persistence::interface::atomic_document_store::{
    DocumentCodec, IAtomicDocumentStore, IAtomicTomlDocumentStore,
};
use crate::persistence::interface::storage::{
    IFileSystemStorageService, STORAGE_DECODE_FAILED, StorageError, StorageWriteOptions,
};
use crate::wire::record::WireAsyncError;

pub struct JsonDocumentCodec;

impl DocumentCodec for JsonDocumentCodec {
    fn format(&self) -> &str {
        "json"
    }

    fn encode(&self, value: &Value) -> Result<Vec<u8>, WireAsyncError> {
        Ok(serde_json::to_vec(value)?)
    }

    fn decode(&self, bytes: &[u8]) -> Result<Value, WireAsyncError> {
        Ok(serde_json::from_str(&decode_text(bytes))?)
    }
}

pub struct TomlDocumentCodec;

impl DocumentCodec for TomlDocumentCodec {
    fn format(&self) -> &str {
        "toml"
    }

    fn encode(&self, value: &Value) -> Result<Vec<u8>, WireAsyncError> {
        let text = toml::to_string(value)?;
        Ok(format!("{}\n", text.trim_end_matches(['\r', '\n'])).into_bytes())
    }

    fn decode(&self, bytes: &[u8]) -> Result<Value, WireAsyncError> {
        let text = decode_text(bytes);
        if text.trim().is_empty() {
            return Ok(json!({}));
        }
        let value = toml::from_str::<toml::Value>(&text)?;
        Ok(serde_json::to_value(value)?)
    }
}

pub static JSON_DOCUMENT_CODEC: JsonDocumentCodec = JsonDocumentCodec;
pub static TOML_DOCUMENT_CODEC: TomlDocumentCodec = TomlDocumentCodec;

fn decode_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes)
}

struct AtomicDocumentStoreBase {
    storage: Arc<dyn IFileSystemStorageService>,
    codec: Arc<dyn DocumentCodec>,
}

impl AtomicDocumentStoreBase {
    fn new(storage: Arc<dyn IFileSystemStorageService>, codec: Arc<dyn DocumentCodec>) -> Self {
        Self { storage, codec }
    }

    async fn get(&self, scope: &str, key: &str) -> Result<Option<Value>, WireAsyncError> {
        let Some(bytes) = self.storage.read(scope, key).await? else {
            return Ok(None);
        };
        self.codec.decode(&bytes).map(Some).map_err(|cause| {
            Box::new(StorageError::with_options(
                STORAGE_DECODE_FAILED,
                format!("failed to decode {scope}/{key} as {}", self.codec.format()),
                kimi_core::primitives::errors::Error2Options {
                    details: Some(Map::from_iter([
                        ("scope".to_string(), Value::String(scope.to_string())),
                        ("key".to_string(), Value::String(key.to_string())),
                        (
                            "format".to_string(),
                            Value::String(self.codec.format().to_string()),
                        ),
                    ])),
                    cause: Some(cause),
                    name: None,
                },
            )) as WireAsyncError
        })
    }

    async fn set(&self, scope: &str, key: &str, value: Value) -> Result<(), WireAsyncError> {
        let bytes = self.codec.encode(&value)?;
        self.storage
            .write(
                scope,
                key,
                bytes,
                Some(StorageWriteOptions {
                    atomic: Some(true),
                    signal: None,
                }),
            )
            .await
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        self.storage.delete(scope, key).await
    }

    async fn list(&self, scope: &str, prefix: Option<&str>) -> Result<Vec<String>, WireAsyncError> {
        self.storage.list(scope, prefix, None).await
    }

    fn acquire(&self) -> Box<dyn IDisposable> {
        to_disposable(|| {})
    }
}

pub struct JsonAtomicDocumentStore {
    base: AtomicDocumentStoreBase,
}

impl JsonAtomicDocumentStore {
    pub fn new(storage: Arc<dyn IFileSystemStorageService>) -> Self {
        Self {
            base: AtomicDocumentStoreBase::new(storage, Arc::new(JsonDocumentCodec)),
        }
    }
}

#[async_trait]
impl IAtomicDocumentStore for JsonAtomicDocumentStore {
    async fn get(&self, scope: &str, key: &str) -> Result<Option<Value>, WireAsyncError> {
        self.base.get(scope, key).await
    }

    async fn set(&self, scope: &str, key: &str, value: Value) -> Result<(), WireAsyncError> {
        self.base.set(scope, key, value).await
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        self.base.delete(scope, key).await
    }

    async fn list(&self, scope: &str, prefix: Option<&str>) -> Result<Vec<String>, WireAsyncError> {
        self.base.list(scope, prefix).await
    }

    fn acquire(&self, _scope: &str, _key: &str) -> Box<dyn IDisposable> {
        self.base.acquire()
    }
}

pub struct TomlAtomicDocumentStore {
    base: AtomicDocumentStoreBase,
}

impl TomlAtomicDocumentStore {
    pub fn new(storage: Arc<dyn IFileSystemStorageService>) -> Self {
        Self {
            base: AtomicDocumentStoreBase::new(storage, Arc::new(TomlDocumentCodec)),
        }
    }
}

#[async_trait]
impl IAtomicDocumentStore for TomlAtomicDocumentStore {
    async fn get(&self, scope: &str, key: &str) -> Result<Option<Value>, WireAsyncError> {
        self.base.get(scope, key).await
    }

    async fn set(&self, scope: &str, key: &str, value: Value) -> Result<(), WireAsyncError> {
        self.base.set(scope, key, value).await
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        self.base.delete(scope, key).await
    }

    async fn list(&self, scope: &str, prefix: Option<&str>) -> Result<Vec<String>, WireAsyncError> {
        self.base.list(scope, prefix).await
    }

    fn acquire(&self, _scope: &str, _key: &str) -> Box<dyn IDisposable> {
        self.base.acquire()
    }
}

#[async_trait]
impl IAtomicTomlDocumentStore for TomlAtomicDocumentStore {
    async fn get_text(&self, scope: &str, key: &str) -> Result<Option<String>, WireAsyncError> {
        Ok(self
            .base
            .storage
            .read(scope, key)
            .await?
            .map(|bytes| decode_text(&bytes).into_owned()))
    }

    async fn set_text(&self, scope: &str, key: &str, text: &str) -> Result<(), WireAsyncError> {
        self.base
            .storage
            .write(
                scope,
                key,
                text.as_bytes().to_vec(),
                Some(StorageWriteOptions {
                    atomic: Some(true),
                    signal: None,
                }),
            )
            .await
    }
}
