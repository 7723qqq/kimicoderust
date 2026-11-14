use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, future::BoxFuture};
use serde::{Deserialize, Serialize};

use crate::_base::exec_env::decode_text::TextDecodeErrors;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostFileStat {
    pub is_file: bool,
    pub is_directory: bool,
    pub is_symbolic_link: bool,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ino: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct HostFileHead {
    pub stat: HostFileStat,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostDirEntry {
    pub name: String,
    pub is_file: bool,
    pub is_directory: bool,
    pub is_symbolic_link: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostWalkKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostWalkEntry {
    pub path: String,
    pub kind: HostWalkKind,
}

pub trait HostWalkMatcher: Send + Sync {
    fn ignores(&self, path: &str) -> bool;
}

pub type HostWalkVisitor = Arc<dyn Fn(HostWalkEntry) -> BoxFuture<'static, bool> + Send + Sync>;

#[derive(Default)]
pub struct HostWalkOptions {
    pub ignore: Option<Vec<String>>,
    pub matcher: Option<Arc<dyn HostWalkMatcher>>,
    pub deadline_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostWalkResult {
    pub truncated: bool,
}

#[derive(Clone, Default)]
pub struct HostReadTextOptions {
    pub encoding: Option<String>,
    pub errors: Option<TextDecodeErrors>,
    pub preserve_bom: Option<bool>,
}

#[derive(Clone, Default)]
pub struct HostReadLinesOptions {
    pub encoding: Option<String>,
    pub errors: Option<TextDecodeErrors>,
}

pub type HostLineStream = Pin<Box<dyn Stream<Item = Result<String, WireAsyncError>> + Send>>;

#[async_trait]
pub trait IHostFileSystem: Send + Sync {
    async fn read_text(
        &self,
        path: &str,
        options: Option<HostReadTextOptions>,
    ) -> Result<String, WireAsyncError>;
    async fn write_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError>;
    async fn append_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError>;
    async fn read_bytes(
        &self,
        path: &str,
        n: u64,
        offset: Option<u64>,
    ) -> Result<Vec<u8>, WireAsyncError>;
    async fn read_file_head(&self, path: &str, max_bytes: u64) -> Result<HostFileHead, WireAsyncError>;
    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), WireAsyncError>;
    fn read_lines(&self, path: &str, options: Option<HostReadLinesOptions>) -> HostLineStream;
    async fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, WireAsyncError>;
    async fn stat(&self, path: &str) -> Result<HostFileStat, WireAsyncError>;
    async fn lstat(&self, path: &str) -> Result<HostFileStat, WireAsyncError>;
    async fn readdir(&self, path: &str) -> Result<Vec<HostDirEntry>, WireAsyncError>;
    async fn walk(
        &self,
        path: &str,
        ignore: Option<&[String]>,
        deadline_ms: Option<i64>,
        visit: HostWalkVisitor,
    ) -> Result<HostWalkResult, WireAsyncError>;
    async fn mkdir(&self, path: &str, recursive: bool) -> Result<(), WireAsyncError>;
    async fn remove(&self, path: &str) -> Result<(), WireAsyncError>;
    async fn rename(&self, from: &str, to: &str) -> Result<(), WireAsyncError>;
    async fn realpath(&self, path: &str) -> Result<String, WireAsyncError>;
}
