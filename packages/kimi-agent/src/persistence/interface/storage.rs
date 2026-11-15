use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::Stream;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::utils::abort::AbortSignal;
use crate::wire::record::WireAsyncError;

pub use kimi_core::primitives::errors::domains::storage::{
    DOMAIN as STORAGE_ERRORS, STORAGE_CORRUPTED, STORAGE_DECODE_FAILED, STORAGE_DISK_FULL,
    STORAGE_IO_FAILED, STORAGE_LOCKED, STORAGE_NOT_FOUND, STORAGE_PERMISSION_DENIED,
};

pub type StorageErrorCode = &'static str;

#[derive(Debug)]
pub struct StorageError {
    inner: Error2,
}

impl StorageError {
    pub fn new(code: StorageErrorCode, message: impl Into<String>) -> Self {
        Self::with_options(code, message, Error2Options::default())
    }

    pub fn with_options(
        code: StorageErrorCode,
        message: impl Into<String>,
        mut options: Error2Options,
    ) -> Self {
        options.name = Some("StorageError".to_string());
        Self {
            inner: Error2::with_options(code, message, options),
        }
    }
}

impl Deref for StorageError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}

pub fn is_storage_error(error: &WireAsyncError, code: StorageErrorCode) -> bool {
    error
        .downcast_ref::<StorageError>()
        .is_some_and(|error| error.code().as_str() == code)
}

pub type StorageByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, WireAsyncError>> + Send>>;

pub fn to_storage_io_error(error: WireAsyncError, path: &str, op: &str) -> WireAsyncError {
    if error.downcast_ref::<StorageError>().is_some() {
        return error;
    }
    let errno = read_errno(error.as_ref());
    let code = map_errno(errno.as_deref());
    let reason = match code {
        STORAGE_NOT_FOUND => "path does not exist",
        STORAGE_PERMISSION_DENIED => "permission denied",
        STORAGE_DISK_FULL => "no space left on device",
        _ => "unrecognized I/O error",
    };
    let mut details = Map::new();
    details.insert("path".to_string(), Value::String(path.to_string()));
    details.insert("op".to_string(), Value::String(op.to_string()));
    if let Some(errno) = errno {
        details.insert("errno".to_string(), Value::String(errno));
    }
    Box::new(StorageError::with_options(
        code,
        format!("storage {op} failed: {reason}"),
        Error2Options {
            details: Some(details),
            cause: Some(error),
            ..Default::default()
        },
    ))
}

fn read_errno(error: &(dyn Error + 'static)) -> Option<String> {
    let io = error.downcast_ref::<std::io::Error>()?;
    match io.kind() {
        std::io::ErrorKind::NotFound => Some("ENOENT".to_string()),
        std::io::ErrorKind::PermissionDenied => {
            #[cfg(unix)]
            if io.raw_os_error() == Some(1) {
                return Some("EPERM".to_string());
            }
            Some("EACCES".to_string())
        }
        std::io::ErrorKind::StorageFull => Some("ENOSPC".to_string()),
        _ => io.raw_os_error().map(|errno| errno.to_string()),
    }
}

fn map_errno(errno: Option<&str>) -> StorageErrorCode {
    match errno {
        Some("ENOENT") => STORAGE_NOT_FOUND,
        Some("EACCES" | "EPERM") => STORAGE_PERMISSION_DENIED,
        Some("ENOSPC") => STORAGE_DISK_FULL,
        _ => STORAGE_IO_FAILED,
    }
}

#[derive(Clone, Default)]
pub struct StorageWriteOptions {
    pub atomic: Option<bool>,
    pub signal: Option<AbortSignal>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageAppendOptions {
    pub durable: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageReadRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorageListOptions {
    pub recursive: Option<bool>,
}

#[async_trait]
pub trait IFileSystemStorageService: Send + Sync {
    async fn read(&self, scope: &str, key: &str) -> Result<Option<Vec<u8>>, WireAsyncError>;
    fn read_stream(
        &self,
        scope: &str,
        key: &str,
        range: Option<StorageReadRange>,
    ) -> StorageByteStream;
    async fn write(
        &self,
        scope: &str,
        key: &str,
        data: Vec<u8>,
        options: Option<StorageWriteOptions>,
    ) -> Result<(), WireAsyncError>;
    async fn write_stream(
        &self,
        scope: &str,
        key: &str,
        source: StorageByteStream,
        options: Option<StorageWriteOptions>,
    ) -> Result<(), WireAsyncError>;
    async fn append(
        &self,
        scope: &str,
        key: &str,
        data: Vec<u8>,
        options: Option<StorageAppendOptions>,
    ) -> Result<(), WireAsyncError>;
    async fn list(
        &self,
        scope: &str,
        prefix: Option<&str>,
        options: Option<StorageListOptions>,
    ) -> Result<Vec<String>, WireAsyncError>;
    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError>;
    async fn size(&self, scope: &str, key: &str) -> Result<Option<u64>, WireAsyncError>;
    fn size_sync(&self, scope: &str, key: &str) -> Option<u64>;
    async fn mtime(&self, scope: &str, key: &str) -> Result<Option<i64>, WireAsyncError>;
    fn path_for(&self, scope: &str, key: &str) -> Option<String>;
    async fn flush(&self) -> Result<(), WireAsyncError>;
    async fn close(&self) -> Result<(), WireAsyncError>;
}

pub static I_FILE_SYSTEM_STORAGE_SERVICE: LazyLock<
    ServiceIdentifier<Arc<dyn IFileSystemStorageService>>,
> = LazyLock::new(|| create_decorator("fileSystemStorageService"));
