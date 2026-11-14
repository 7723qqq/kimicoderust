use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat};
use kimi_core::primitives::errors::{
    Error2, Error2Options, ErrorDomain, ErrorInfo, register_error_domain,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::persistence::interface::blobStore::{BlobByteStream, BlobReadRange};
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SaveOptions {
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub expires_in_sec: Option<i64>,
}

pub struct GetResult {
    pub meta: FileMeta,
    stream: Arc<dyn Fn(Option<FileReadRange>) -> BlobByteStream + Send + Sync>,
}

impl GetResult {
    pub fn new(
        meta: FileMeta,
        stream: Arc<dyn Fn(Option<FileReadRange>) -> BlobByteStream + Send + Sync>,
    ) -> Self {
        Self { meta, stream }
    }

    pub fn stream(&self, range: Option<FileReadRange>) -> BlobByteStream {
        (self.stream)(range)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileReadRange {
    pub start: u64,
    pub end: u64,
}

impl From<FileReadRange> for BlobReadRange {
    fn from(range: FileReadRange) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

#[async_trait]
pub trait IFileService: Send + Sync {
    async fn save(
        &self,
        source: BlobByteStream,
        filename: &str,
        options: Option<SaveOptions>,
    ) -> Result<FileMeta, WireAsyncError>;

    async fn get(&self, file_id: &str) -> Result<GetResult, WireAsyncError>;

    async fn delete(&self, file_id: &str) -> Result<(), WireAsyncError>;
}

pub static FILE_ID_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^f_[A-Za-z0-9][A-Za-z0-9_-]*$").unwrap());

pub fn is_file_id(value: &str) -> bool {
    FILE_ID_REGEX.is_match(value)
}

pub const FILE_NOT_FOUND: &str = "file.not_found";
pub const FILE_TOO_LARGE: &str = "file.too_large";

pub static FILE_ERRORS: ErrorDomain = ErrorDomain {
    id: "file",
    codes: &[("FILE_NOT_FOUND", FILE_NOT_FOUND), ("FILE_TOO_LARGE", FILE_TOO_LARGE)],
    retryable: &[],
    info: &[
        (FILE_NOT_FOUND, ErrorInfo::new(false, true)),
        (FILE_TOO_LARGE, ErrorInfo::new(false, true)),
    ],
};

static FILE_ERROR_REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    let _ = register_error_domain(&FILE_ERRORS);
});

pub fn register_file_errors() {
    LazyLock::force(&FILE_ERROR_REGISTRATION);
}

pub type FileErrorCode = &'static str;

#[derive(Debug)]
pub struct FileError {
    inner: Error2,
}

impl FileError {
    pub fn new(
        code: FileErrorCode,
        message: impl Into<String>,
        details: Option<Map<String, Value>>,
    ) -> Self {
        register_file_errors();
        Self {
            inner: Error2::with_options(
                code,
                message,
                Error2Options {
                    details,
                    name: Some("FileError".to_string()),
                    ..Default::default()
                },
            ),
        }
    }
}

impl Deref for FileError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl Error for FileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.inner.source()
    }
}

pub fn file_not_found_error(file_id: &str) -> FileError {
    let mut details = Map::new();
    details.insert("fileId".to_string(), Value::String(file_id.to_string()));
    FileError::new(
        FILE_NOT_FOUND,
        format!("file not found: {file_id}"),
        Some(details),
    )
}

pub fn is_file_error(error: &(dyn Error + 'static), code: FileErrorCode) -> bool {
    if let Some(error) = error.downcast_ref::<FileError>() {
        return error.code().as_str() == code;
    }
    error
        .downcast_ref::<Error2>()
        .is_some_and(|error| error.code().as_str() == code)
}

pub fn file_meta_schema(value: &Value) -> Result<FileMeta, String> {
    let object = value.as_object().ok_or_else(|| "expected an object".to_string())?;
    let id = required_string(object, "id")?;
    let name = required_string(object, "name")?;
    let media_type = required_string(object, "media_type")?;
    if id.is_empty() || name.is_empty() || media_type.is_empty() {
        return Err("id, name and media_type must not be empty".to_string());
    }
    let size = object
        .get("size")
        .and_then(Value::as_u64)
        .filter(|size| *size <= 9_007_199_254_740_991)
        .ok_or_else(|| "size must be a nonnegative safe integer".to_string())?;
    let created_at = canonical_iso_date(required_string(object, "created_at")?)?;
    let expires_at = match object.get("expires_at") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(canonical_iso_date(value)?),
        Some(_) => return Err("expires_at must be an ISO 8601 datetime string".to_string()),
    };
    Ok(FileMeta {
        id,
        name,
        media_type,
        size,
        created_at,
        expires_at,
    })
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| format!("{key} must be a string"))
}

fn canonical_iso_date(value: &str) -> Result<String, String> {
    static ISO_8601_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,9})?(?:Z|[+-]\d{2}(?::?\d{2})?)$")
            .unwrap()
    });
    if !ISO_8601_REGEX.is_match(value) {
        return Err("must be an ISO 8601 datetime string".to_string());
    }
    DateTime::parse_from_rfc3339(value)
        .map(|value| {
            value
                .with_timezone(&chrono::Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
        })
        .map_err(|_| "invalid ISO 8601 datetime".to_string())
}
