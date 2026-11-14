use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::utils::abort::AbortSignal;
use crate::persistence::interface::blobStore::BlobByteStream;
use crate::wire::record::WireAsyncError;

pub type SessionMediaStream = Arc<
    dyn Fn() -> BlobByteStream + Send + Sync + 'static,
>;

pub struct SessionMediaMaterializeInput {
    pub file_id: String,
    pub size: u64,
    pub name: String,
    pub mime_type: String,
    pub stream: SessionMediaStream,
    pub signal: Option<AbortSignal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMediaReadRange {
    pub start: u64,
    pub end: u64,
}

pub type SessionMediaStreamFactory = Arc<
    dyn Fn(Option<SessionMediaReadRange>) -> BlobByteStream + Send + Sync + 'static,
>;

pub struct SessionMediaFile {
    pub path: Option<String>,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub stream: SessionMediaStreamFactory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMediaReadResult {
    pub data: Vec<u8>,
    pub name: String,
}

#[async_trait]
pub trait ISessionMediaStore: Send + Sync {
    fn path_for(&self, file_id: &str, ext: &str) -> Option<String>;

    async fn resolve_display_path(&self, file_id: &str) -> Result<Option<String>, WireAsyncError>;

    async fn read(&self, file_id: &str) -> Result<Option<SessionMediaReadResult>, WireAsyncError>;

    async fn open(&self, file_id: &str) -> Result<Option<SessionMediaFile>, WireAsyncError>;

    async fn materialize(
        &self,
        input: SessionMediaMaterializeInput,
    ) -> Result<Option<String>, WireAsyncError>;
}
