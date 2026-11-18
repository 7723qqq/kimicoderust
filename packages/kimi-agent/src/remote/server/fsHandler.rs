use base64::Engine;
use futures_util::FutureExt;
use serde_json::{Map, Value};

use crate::os::backends::host::host_fs_service::{HostFileSystem, walk_directory_tree};
use crate::os::interface::host_file_system::{HostFileStat, HostWalkOptions, IHostFileSystem};
use crate::os::interface::host_fs_errors::{
    HostFsError, OS_FS_ALREADY_EXISTS, OS_FS_DIRECTORY_TOO_LARGE, OS_FS_NOT_FOUND,
    OS_FS_PERMISSION_DENIED, to_host_fs_error,
};
use crate::remote::protocol::errors::{
    RPC_INTERNAL_ERROR, RPC_INVALID_PARAMS, RPC_INVALID_REQUEST, RPC_NOT_FOUND, RpcError,
};
use crate::remote::protocol::methods::*;
use crate::wire::record::WireAsyncError;

pub fn require_params(value: Value) -> Result<Map<String, Value>, RpcError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| RpcError::new(RPC_INVALID_PARAMS, "params must be an object", None))
}

pub fn require_string(params: &Map<String, Value>, name: &str) -> Result<String, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            RpcError::new(
                RPC_INVALID_PARAMS,
                format!("{name} must be a non-empty string"),
                None,
            )
        })
}

pub fn require_absolute_path(params: &Map<String, Value>, name: &str) -> Result<String, RpcError> {
    let value = require_string(params, name)?;
    if cfg!(windows) {
        if !value.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            || !value.as_bytes().get(1).is_some_and(|byte| *byte == b':')
            || !matches!(value.as_bytes().get(2), Some(b'/' | b'\\'))
        {
            return Err(RpcError::new(
                RPC_INVALID_REQUEST,
                format!("{name} must be an absolute Windows drive path such as C:\\work"),
                None,
            ));
        }
    } else if !std::path::Path::new(&value).is_absolute() {
        return Err(RpcError::new(
            RPC_INVALID_REQUEST,
            format!("{name} must be an absolute path"),
            None,
        ));
    }
    Ok(value)
}

pub fn optional_boolean(params: &Map<String, Value>, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(RpcError::new(
            RPC_INVALID_PARAMS,
            format!("{name} must be a boolean"),
            None,
        )),
    }
}

fn optional_integer(
    params: &Map<String, Value>,
    name: &str,
    min: u64,
    max: u64,
) -> Result<Option<u64>, RpcError> {
    match params.get(name) {
        None => Ok(None),
        Some(value) => {
            let value = value.as_f64().filter(|value| {
                value.is_finite() && value.fract() == 0.0 && *value >= min as f64 && *value <= max as f64
            });
            value
                .map(|value| Some(value as u64))
                .ok_or_else(|| {
                    RpcError::new(
                        RPC_INVALID_PARAMS,
                        format!("{name} must be an integer in [{min}, {max}]"),
                        None,
                    )
                })
        }
    }
}

fn optional_string_array(
    params: &Map<String, Value>,
    name: &str,
) -> Result<Option<Vec<String>>, RpcError> {
    match params.get(name) {
        None => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| {
                        RpcError::new(
                            RPC_INVALID_PARAMS,
                            format!("{name} must be an array of strings"),
                            None,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(RpcError::new(
            RPC_INVALID_PARAMS,
            format!("{name} must be an array of strings"),
            None,
        )),
    }
}

fn to_metadata(stat: &HostFileStat) -> FsGetMetadataResult {
    FsGetMetadataResult {
        is_directory: stat.is_directory,
        is_file: stat.is_file,
        is_symlink: stat.is_symbolic_link,
        size: stat.size,
        modified_at_ms: stat.mtime_ms.unwrap_or(0.0) as i64,
        mode: stat.mode,
    }
}

fn fs_domain_error(error: WireAsyncError, path: &str, op: &str) -> RpcError {
    let host_error = to_host_fs_error(error, path, op);
    let message = host_error.to_string();
    let host = host_error.downcast_ref::<HostFsError>();
    let domain_code = host.map(|error| error.code().as_str()).unwrap_or("");
    let code = if domain_code == OS_FS_NOT_FOUND {
        RPC_NOT_FOUND
    } else if domain_code == OS_FS_PERMISSION_DENIED {
        RPC_INVALID_REQUEST
    } else {
        RPC_INTERNAL_ERROR
    };
    let mut data = Map::new();
    data.insert("domainCode".to_string(), domain_code.into());
    if let Some(details) = host.and_then(|error| error.details()) {
        data.extend(details.clone());
    }
    RpcError::new(code, message, Some(Value::Object(data)))
}

pub struct FsHandler {
    fs: HostFileSystem,
}

impl FsHandler {
    pub fn new(fs: HostFileSystem) -> Self {
        Self { fs }
    }

    pub async fn read_file(&self, raw_params: Value) -> Result<FsReadFileResult, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        let offset = optional_integer(&params, "offset", 0, 9_007_199_254_740_991)?.unwrap_or(0);
        let max_bytes = optional_integer(&params, "maxBytes", 1, FS_READ_FILE_MAX_BYTES)?;
        let include_metadata = optional_boolean(&params, "includeMetadata")?.unwrap_or(false);
        let result = async {
            let stat = self.fs.stat(&path).await?;
            let metadata = include_metadata.then(|| to_metadata(&stat));
            if metadata.is_some() && !stat.is_file {
                return Ok(FsReadFileResult {
                    data_base64: String::new(),
                    eof: true,
                    metadata,
                });
            }
            let length = max_bytes.unwrap_or(stat.size.saturating_sub(offset));
            if length > FS_READ_FILE_WHOLE_MAX_BYTES {
                return Err(Box::new(RpcError::new(
                    RPC_INVALID_REQUEST,
                    format!("fs/readFile without maxBytes is limited to {FS_READ_FILE_WHOLE_MAX_BYTES} bytes; use offset/maxBytes"),
                    None,
                )) as WireAsyncError);
            }
            if offset >= stat.size {
                return Ok(FsReadFileResult {
                    data_base64: String::new(),
                    eof: true,
                    metadata,
                });
            }
            let data = self
                .fs
                .read_bytes(&path, length.min(stat.size - offset), Some(offset))
                .await?;
            Ok(FsReadFileResult {
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                eof: offset + data.len() as u64 >= stat.size,
                metadata,
            })
        }
        .await;
        match result {
            Ok(result) => Ok(result),
            Err(error) if error.downcast_ref::<RpcError>().is_some() => Err(error),
            Err(error) => Err(Box::new(fs_domain_error(error, &path, "read"))),
        }
    }

    pub async fn write_file(&self, raw_params: Value) -> Result<Value, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        let data_base64 = params.get("dataBase64").and_then(Value::as_str).ok_or_else(|| {
            RpcError::new(RPC_INVALID_PARAMS, "dataBase64 must be a string", None)
        })?;
        let mode = params.get("mode").and_then(Value::as_str).ok_or_else(|| {
            RpcError::new(RPC_INVALID_PARAMS, "mode must be truncate, append or exclusive", None)
        })?;
        if !matches!(mode, "truncate" | "append" | "exclusive") {
            return Err(RpcError::new(RPC_INVALID_PARAMS, "mode must be truncate, append or exclusive", None).into());
        }
        let data = crate::remote::protocol::codec::decode_base64(data_base64);
        let result = async {
            match mode {
                "truncate" => self.fs.write_bytes(&path, &data).await?,
                "append" => self.fs.append_bytes(&path, &data).await?,
                _ => {
                    if !self.fs.create_exclusive(&path, &data).await? {
                        return Err(Box::new(HostFsError::new(
                            OS_FS_ALREADY_EXISTS,
                            format!("{path} already exists"),
                            Some(serde_json::json!({ "path": path, "op": "create" })),
                        )) as WireAsyncError);
                    }
                }
            }
            Ok(Value::Object(Map::new()))
        }
        .await;
        match result {
            Ok(result) => Ok(result),
            Err(error) if error.downcast_ref::<RpcError>().is_some() => Err(error),
            Err(error) => Err(Box::new(fs_domain_error(error, &path, "write"))),
        }
    }

    pub async fn create_directory(&self, raw_params: Value) -> Result<Value, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        let recursive = optional_boolean(&params, "recursive")?.unwrap_or(false);
        self.fs
            .mkdir(&path, recursive)
            .await
            .map(|_| Value::Object(Map::new()))
            .map_err(|error| Box::new(fs_domain_error(error, &path, "mkdir")).into())
    }

    pub async fn get_metadata(&self, raw_params: Value) -> Result<FsGetMetadataResult, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        let follow_symlinks = optional_boolean(&params, "followSymlinks")?.unwrap_or(true);
        let stat = if follow_symlinks {
            self.fs.stat(&path).await
        } else {
            self.fs.lstat(&path).await
        };
        stat
            .map(|stat| to_metadata(&stat))
            .map_err(|error| Box::new(fs_domain_error(error, &path, "stat")).into())
    }

    pub async fn canonicalize(&self, raw_params: Value) -> Result<FsCanonicalizeResult, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        self.fs
            .realpath(&path)
            .await
            .map(|path| FsCanonicalizeResult { path })
            .map_err(|error| Box::new(fs_domain_error(error, &path, "realpath")).into())
    }

    pub async fn read_directory(&self, raw_params: Value) -> Result<FsReadDirectoryResult, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        let entries = self
            .fs
            .readdir(&path)
            .await
            .map_err(|error| Box::new(fs_domain_error(error, &path, "readdir")) as WireAsyncError)?;
        let truncated = entries.len() > FS_READ_DIRECTORY_MAX_ENTRIES;
        Ok(FsReadDirectoryResult {
            entries: entries
                .into_iter()
                .take(FS_READ_DIRECTORY_MAX_ENTRIES)
                .map(|entry| FsReadDirectoryEntry {
                    file_name: entry.name,
                    is_directory: entry.is_directory,
                    is_file: entry.is_file,
                    is_symlink: Some(entry.is_symbolic_link),
                })
                .collect(),
            truncated,
        })
    }

    pub async fn walk(&self, raw_params: Value) -> Result<FsWalkResult, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        let ignore = optional_string_array(&params, "ignore")?;
        let timeout_ms = optional_integer(&params, "timeoutMs", 1, FS_WALK_MAX_TIMEOUT_MS)?
            .unwrap_or(FS_WALK_DEFAULT_TIMEOUT_MS);
        let entries: std::sync::Arc<parking_lot::Mutex<Vec<FsWalkEntry>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let response_bytes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let entries_sink = entries.clone();
        let response_bytes_sink = response_bytes.clone();
        let fs: std::sync::Arc<dyn IHostFileSystem> = std::sync::Arc::new(self.fs.clone());
        let result = walk_directory_tree(
            fs,
            &path,
            HostWalkOptions {
                ignore,
                matcher: None,
                deadline_ms: Some(chrono::Utc::now().timestamp_millis() + timeout_ms as i64),
            },
            std::sync::Arc::new(move |entry| {
                let added = entry.path.len() + FS_WALK_ENTRY_OVERHEAD_BYTES;
                if response_bytes_sink.fetch_add(added, std::sync::atomic::Ordering::Relaxed) + added > FS_WALK_MAX_RESPONSE_BYTES {
                    return async { false }.boxed();
                }
                let kind = match entry.kind {
                    crate::os::interface::host_file_system::HostWalkKind::File => FsWalkKind::File,
                    crate::os::interface::host_file_system::HostWalkKind::Directory => FsWalkKind::Directory,
                    crate::os::interface::host_file_system::HostWalkKind::Symlink => FsWalkKind::Symlink,
                };
                entries_sink.lock().push(FsWalkEntry { path: entry.path, kind });
                async { true }.boxed()
            }),
        )
        .await;
        match result {
            Ok(result) => Ok(FsWalkResult {
                entries: std::sync::Arc::try_unwrap(entries).unwrap().into_inner(),
                truncated: result.truncated,
            }),
            Err(error) => Err(Box::new(fs_domain_error(error, &path, "readdir")).into()),
        }
    }

    pub async fn remove(&self, raw_params: Value) -> Result<Value, WireAsyncError> {
        let params = require_params(raw_params)?;
        let path = require_absolute_path(&params, "path")?;
        self.fs
            .remove(&path)
            .await
            .map(|_| Value::Object(Map::new()))
            .map_err(|error| Box::new(fs_domain_error(error, &path, "remove")).into())
    }

    pub async fn rename(&self, raw_params: Value) -> Result<Value, WireAsyncError> {
        let params = require_params(raw_params)?;
        let from = require_absolute_path(&params, "from")?;
        let to = require_absolute_path(&params, "to")?;
        self.fs
            .rename(&from, &to)
            .await
            .map(|_| Value::Object(Map::new()))
            .map_err(|error| Box::new(fs_domain_error(error, &from, "rename")).into())
    }
}
