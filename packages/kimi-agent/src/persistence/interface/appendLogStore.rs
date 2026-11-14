use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use kimi_core::primitives::errors::domains::storage::STORAGE_CORRUPTED;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::IDisposable;
use crate::wire::record::WireAsyncError;

#[derive(Debug)]
pub struct AppendLogCorruptedError {
    inner: Error2,
}

impl AppendLogCorruptedError {
    pub fn new(scope: &str, key: &str, line_number: usize, cause: Option<WireAsyncError>) -> Self {
        Self {
            inner: Error2::with_options(
                STORAGE_CORRUPTED,
                format!("append-log {scope}/{key}: corrupted line {line_number}"),
                Error2Options {
                    details: Some(Map::from_iter([
                        ("scope".to_string(), Value::String(scope.to_string())),
                        ("key".to_string(), Value::String(key.to_string())),
                        ("lineNumber".to_string(), Value::from(line_number)),
                    ])),
                    cause,
                    name: Some("AppendLogCorruptedError".to_string()),
                },
            ),
        }
    }
}

impl Deref for AppendLogCorruptedError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for AppendLogCorruptedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl Error for AppendLogCorruptedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}

#[derive(Clone)]
pub struct AppendLogOptions {
    pub on_error: Option<Arc<dyn Fn(WireAsyncError) + Send + Sync>>,
}

pub enum AppendLogTruncationReason {
    Corrupted,
    Truncated,
}

pub struct AppendLogTruncation {
    pub line_number: usize,
    pub reason: AppendLogTruncationReason,
    pub cause: Option<WireAsyncError>,
}

pub struct AppendLogReadOptions {
    pub on_truncate: Option<Arc<dyn Fn(AppendLogTruncation) + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendLogWrite {
    pub scope: String,
    pub key: String,
}

#[derive(Clone)]
pub struct AppendLogRewriteOptions {
    pub on_error: Option<Arc<dyn Fn(WireAsyncError) + Send + Sync>>,
}

pub type AppendLogReadStream =
    Pin<Box<dyn Stream<Item = Result<Value, WireAsyncError>> + Send>>;

#[async_trait]
pub trait IAppendLogStore: Send + Sync {
    fn on_did_write(
        &self,
        listener: Arc<dyn Fn(AppendLogWrite) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn append(
        &self,
        scope: &str,
        key: &str,
        record: Value,
        options: Option<AppendLogOptions>,
    ) -> Result<(), WireAsyncError>;
    fn read(&self, scope: &str, key: &str, options: Option<AppendLogReadOptions>)
        -> AppendLogReadStream;
    async fn rewrite(
        &self,
        scope: &str,
        key: &str,
        records: Vec<Value>,
        options: Option<AppendLogRewriteOptions>,
    ) -> Result<(), WireAsyncError>;
    async fn flush(&self) -> Result<(), WireAsyncError>;
    async fn flush_log(&self, scope: &str, key: &str) -> Result<(), WireAsyncError>;
    async fn close(&self) -> Result<(), WireAsyncError>;
    fn acquire(&self, scope: &str, key: &str) -> Box<dyn IDisposable>;
    async fn drain_retirements(&self) -> Result<(), WireAsyncError>;
    fn externally_modified(&self, scope: &str, key: &str) -> bool;
    fn bind_write_guard(
        &self,
        scope: &str,
        key: &str,
        guard: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Box<dyn IDisposable>;
}
