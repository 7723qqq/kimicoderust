use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::sync::LazyLock;

use async_trait::async_trait;
use kimi_core::primitives::errors::domains::core::VALIDATION_FAILED;
use kimi_core::primitives::errors::domains::fs::{FS_PATH_NOT_FOUND, FS_PERMISSION_DENIED};
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsBrowseQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

pub static FS_BROWSE_QUERY_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {"path": {"type": "string", "minLength": 1}},
        "additionalProperties": false
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsBrowseEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

pub static FS_BROWSE_ENTRY_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["name", "path", "is_dir"],
        "properties": {
            "name": {"type": "string", "minLength": 1},
            "path": {"type": "string", "minLength": 1},
            "is_dir": {"const": true}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsBrowseResponse {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<FsBrowseEntry>,
}

pub static FS_BROWSE_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["path", "parent", "entries"],
        "properties": {
            "path": {"type": "string", "minLength": 1},
            "parent": {"anyOf": [{"type": "string", "minLength": 1}, {"type": "null"}]},
            "entries": {"type": "array", "items": FS_BROWSE_ENTRY_SCHEMA.clone()}
        }
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsHomeResponse {
    pub home: String,
    pub recent_roots: Vec<String>,
}

pub static FS_HOME_RESPONSE_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "required": ["home", "recent_roots"],
        "properties": {
            "home": {"type": "string", "minLength": 1},
            "recent_roots": {"type": "array", "items": {"type": "string", "minLength": 1}}
        }
    })
});

#[derive(Debug)]
pub struct HostFolderNotAbsoluteError {
    inner: Error2,
    pub path: String,
}

impl HostFolderNotAbsoluteError {
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            inner: host_folder_error(
                VALIDATION_FAILED,
                format!("path must be absolute: {path}"),
                &path,
                "HostFolderNotAbsoluteError",
            ),
            path,
        }
    }
}

#[derive(Debug)]
pub struct HostFolderNotFoundError {
    inner: Error2,
    pub path: String,
}

impl HostFolderNotFoundError {
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            inner: host_folder_error(
                FS_PATH_NOT_FOUND,
                format!("path not found: {path}"),
                &path,
                "HostFolderNotFoundError",
            ),
            path,
        }
    }
}

#[derive(Debug)]
pub struct HostFolderPermissionError {
    inner: Error2,
    pub path: String,
}

impl HostFolderPermissionError {
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            inner: host_folder_error(
                FS_PERMISSION_DENIED,
                format!("permission denied: {path}"),
                &path,
                "HostFolderPermissionError",
            ),
            path,
        }
    }
}

macro_rules! impl_host_folder_error {
    ($type:ty) => {
        impl Deref for $type {
            type Target = Error2;

            fn deref(&self) -> &Self::Target {
                &self.inner
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.inner.fmt(formatter)
            }
        }

        impl Error for $type {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                self.inner.source()
            }
        }
    };
}

impl_host_folder_error!(HostFolderNotAbsoluteError);
impl_host_folder_error!(HostFolderNotFoundError);
impl_host_folder_error!(HostFolderPermissionError);

fn host_folder_error(
    code: &'static str,
    message: String,
    path: &str,
    name: &str,
) -> Error2 {
    let mut details = Map::new();
    details.insert("path".to_string(), Value::String(path.to_string()));
    Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(details),
            name: Some(name.to_string()),
            ..Default::default()
        },
    )
}

#[async_trait]
pub trait IHostFolderBrowser: Send + Sync {
    async fn browse(&self, abs_path: Option<&str>) -> Result<FsBrowseResponse, WireAsyncError>;
    async fn home(&self) -> Result<FsHomeResponse, WireAsyncError>;
}

pub const RECENT_ROOTS_LIMIT: usize = 8;
