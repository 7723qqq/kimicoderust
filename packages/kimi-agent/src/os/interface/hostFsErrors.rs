use std::fmt;
use std::ops::Deref;

use kimi_core::primitives::errors::{Error2, Error2Options, unwrap_error_cause};
use serde_json::{Map, Value};

use crate::wire::record::WireAsyncError;

pub const OS_FS_NOT_FOUND: &str = "os.fs.not_found";
pub const OS_FS_IS_DIRECTORY: &str = "os.fs.is_directory";
pub const OS_FS_NOT_DIRECTORY: &str = "os.fs.not_directory";
pub const OS_FS_ALREADY_EXISTS: &str = "os.fs.already_exists";
pub const OS_FS_PERMISSION_DENIED: &str = "os.fs.permission_denied";
pub const OS_FS_NOT_EMPTY: &str = "os.fs.not_empty";
pub const OS_FS_DIRECTORY_TOO_LARGE: &str = "os.fs.directory_too_large";
pub const OS_FS_UNAVAILABLE: &str = "os.fs.unavailable";
pub const OS_FS_UNKNOWN: &str = "os.fs.unknown";

pub type HostFsErrorCode = &'static str;

#[derive(Debug)]
pub struct HostFsError {
    inner: Error2,
}

impl HostFsError {
    pub fn new(
        code: HostFsErrorCode,
        message: impl Into<String>,
        details: Option<Value>,
    ) -> Self {
        Self::with_options(
            code,
            message,
            Error2Options {
                details: details.and_then(|details| details.as_object().cloned()),
                ..Default::default()
            },
        )
    }

    pub fn with_options(
        code: HostFsErrorCode,
        message: impl Into<String>,
        mut options: Error2Options,
    ) -> Self {
        options.name = Some("HostFsError".to_string());
        Self {
            inner: Error2::with_options(code, message, options),
        }
    }
}

impl Deref for HostFsError {
    type Target = Error2;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl fmt::Display for HostFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl std::error::Error for HostFsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.inner.source()
    }
}

pub fn to_host_fs_error(error: WireAsyncError, path: &str, op: &str) -> WireAsyncError {
    if error.downcast_ref::<HostFsError>().is_some() {
        return error;
    }
    let errno = read_errno(error.as_ref());
    let code = map_errno(errno.as_deref());
    let reason = reason(code);
    let mut details = Map::from_iter([
        ("path".to_string(), Value::String(path.to_string())),
        ("op".to_string(), Value::String(op.to_string())),
    ]);
    if let Some(errno) = errno {
        details.insert("errno".to_string(), Value::String(errno));
    }
    if let Some(syscall) = read_syscall(error.as_ref()) {
        details.insert("syscall".to_string(), Value::String(syscall));
    }
    Box::new(HostFsError::with_options(
        code,
        format!("{op} failed: {reason}"),
        Error2Options {
            details: Some(details),
            cause: Some(error),
            name: None,
        },
    ))
}

pub fn is_host_fs_not_found(error: &WireAsyncError) -> bool {
    if let Some(error) = error.downcast_ref::<HostFsError>() {
        return error.code().as_str() == OS_FS_NOT_FOUND;
    }
    let cause = unwrap_error_cause(error.as_ref());
    cause.downcast_ref::<HostFsError>().is_some_and(|error| {
        error.code().as_str() == OS_FS_NOT_FOUND
    }) || read_errno(cause).as_deref() == Some("ENOENT")
}

pub fn is_host_fs_not_directory(error: &WireAsyncError) -> bool {
    if let Some(error) = error.downcast_ref::<HostFsError>() {
        return error.code().as_str() == OS_FS_NOT_DIRECTORY;
    }
    let cause = unwrap_error_cause(error.as_ref());
    cause.downcast_ref::<HostFsError>().is_some_and(|error| {
        error.code().as_str() == OS_FS_NOT_DIRECTORY
    }) || read_errno(cause).as_deref() == Some("ENOTDIR")
}

fn reason(code: HostFsErrorCode) -> &'static str {
    match code {
        OS_FS_NOT_FOUND => "path does not exist",
        OS_FS_IS_DIRECTORY => "path is a directory",
        OS_FS_NOT_DIRECTORY => "a path component is not a directory",
        OS_FS_ALREADY_EXISTS => "path already exists",
        OS_FS_PERMISSION_DENIED => "permission denied",
        OS_FS_NOT_EMPTY => "directory is not empty",
        OS_FS_UNAVAILABLE => "filesystem resource unavailable",
        OS_FS_DIRECTORY_TOO_LARGE => "directory listing exceeds the supported limit",
        _ => "unrecognized filesystem error",
    }
}

fn map_errno(errno: Option<&str>) -> HostFsErrorCode {
    match errno {
        Some("ENOENT") => OS_FS_NOT_FOUND,
        Some("EISDIR") => OS_FS_IS_DIRECTORY,
        Some("ENOTDIR") => OS_FS_NOT_DIRECTORY,
        Some("EEXIST") => OS_FS_ALREADY_EXISTS,
        Some("EACCES" | "EPERM") => OS_FS_PERMISSION_DENIED,
        Some("ENOTEMPTY") => OS_FS_NOT_EMPTY,
        Some(_) => OS_FS_UNKNOWN,
        None => OS_FS_UNKNOWN,
    }
}

fn read_errno(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let io = error.downcast_ref::<std::io::Error>()?;
    match io.kind() {
        std::io::ErrorKind::NotFound => Some("ENOENT".to_string()),
        std::io::ErrorKind::IsADirectory => Some("EISDIR".to_string()),
        std::io::ErrorKind::NotADirectory => Some("ENOTDIR".to_string()),
        std::io::ErrorKind::AlreadyExists => Some("EEXIST".to_string()),
        std::io::ErrorKind::PermissionDenied => Some("EACCES".to_string()),
        std::io::ErrorKind::DirectoryNotEmpty => Some("ENOTEMPTY".to_string()),
        _ => io.raw_os_error().map(|errno| match errno {
            1 => "EPERM".to_string(),
            2 => "ENOENT".to_string(),
            13 => "EACCES".to_string(),
            17 => "EEXIST".to_string(),
            20 => "ENOTDIR".to_string(),
            21 => "EISDIR".to_string(),
            39 => "ENOTEMPTY".to_string(),
            _ => errno.to_string(),
        }),
    }
}

fn read_syscall(_error: &(dyn std::error::Error + 'static)) -> Option<String> {
    None
}
