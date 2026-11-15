use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use async_trait::async_trait;
use serde_json::Value;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::wire::record::WireAsyncError;

pub trait DocumentCodec: Send + Sync {
    fn format(&self) -> &str;
    fn encode(&self, value: &Value) -> Result<Vec<u8>, WireAsyncError>;
    fn decode(&self, bytes: &[u8]) -> Result<Value, WireAsyncError>;
}

#[async_trait]
pub trait IAtomicDocumentStore: Send + Sync {
    async fn get(&self, scope: &str, key: &str) -> Result<Option<Value>, WireAsyncError>;
    async fn set(&self, scope: &str, key: &str, value: Value) -> Result<(), WireAsyncError>;
    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError>;
    async fn list(&self, scope: &str, prefix: Option<&str>) -> Result<Vec<String>, WireAsyncError>;
    fn acquire(&self, scope: &str, key: &str) -> DisposableRef;
}

#[async_trait]
pub trait IAtomicTomlDocumentStore: IAtomicDocumentStore {
    async fn get_text(&self, scope: &str, key: &str) -> Result<Option<String>, WireAsyncError>;
    async fn set_text(&self, scope: &str, key: &str, text: &str) -> Result<(), WireAsyncError>;
}


pub static I_ATOMIC_DOCUMENT_STORE: LazyLock<ServiceIdentifier<Arc<dyn IAtomicDocumentStore>>> =
    LazyLock::new(|| create_decorator("atomicDocumentStore"));
