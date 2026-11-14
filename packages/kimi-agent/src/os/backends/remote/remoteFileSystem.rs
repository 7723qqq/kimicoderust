use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::_base::exec_env::decode_text::{decode_text_with_errors, read_utf8_lines};
use crate::os::interface::host_file_system::{
    HostDirEntry, HostFileHead, HostFileStat, HostLineStream, HostReadLinesOptions,
    HostReadTextOptions, HostWalkEntry, HostWalkResult, HostWalkVisitor,
    IHostFileSystem,
};
use crate::os::interface::host_fs_errors::{
    HostFsError, OS_FS_ALREADY_EXISTS, OS_FS_DIRECTORY_TOO_LARGE, OS_FS_NOT_FOUND,
    OS_FS_UNKNOWN,
};
use crate::os::backends::remote::rpc_domain_error::rpc_domain_error;
use crate::remote::protocol::channel::ExecChannel;
use crate::remote::protocol::codec::{decode_base64, encode_base64};
use crate::remote::protocol::errors::{RPC_NOT_FOUND, RpcError};
use crate::remote::protocol::methods::*;
use crate::wire::record::WireAsyncError;

static HOST_FS_CODES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    HashSet::from([
        crate::os::interface::host_fs_errors::OS_FS_NOT_FOUND,
        crate::os::interface::host_fs_errors::OS_FS_IS_DIRECTORY,
        crate::os::interface::host_fs_errors::OS_FS_NOT_DIRECTORY,
        crate::os::interface::host_fs_errors::OS_FS_ALREADY_EXISTS,
        crate::os::interface::host_fs_errors::OS_FS_PERMISSION_DENIED,
        crate::os::interface::host_fs_errors::OS_FS_NOT_EMPTY,
        crate::os::interface::host_fs_errors::OS_FS_DIRECTORY_TOO_LARGE,
        crate::os::interface::host_fs_errors::OS_FS_UNAVAILABLE,
        crate::os::interface::host_fs_errors::OS_FS_UNKNOWN,
    ])
});

pub fn to_remote_fs_error(error: WireAsyncError) -> WireAsyncError {
    let Some(rpc) = error.downcast_ref::<RpcError>() else {
        return error;
    };
    if let Some(domain) = rpc_domain_error(rpc, &HOST_FS_CODES) {
        return Box::new(HostFsError::new(
            domain.domain_code,
            rpc.message.clone(),
            Some(domain.details),
        ));
    }
    let fallback = if rpc.code == RPC_NOT_FOUND {
        OS_FS_NOT_FOUND
    } else {
        OS_FS_UNKNOWN
    };
    Box::new(HostFsError::new(
        fallback,
        rpc.message.clone(),
        Some(json!({ "rpcCode": rpc.code })),
    ))
}

fn to_host_file_stat(metadata: FsGetMetadataResult) -> HostFileStat {
    HostFileStat {
        is_file: metadata.is_file,
        is_directory: metadata.is_directory,
        is_symbolic_link: metadata.is_symlink,
        size: metadata.size,
        mtime_ms: Some(metadata.modified_at_ms as f64),
        ino: None,
        mode: metadata.mode,
    }
}

pub struct RemoteFileSystem {
    connection: Arc<dyn ExecChannel>,
}

impl RemoteFileSystem {
    pub fn new(connection: Arc<dyn ExecChannel>) -> Self {
        Self { connection }
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, WireAsyncError> {
        let result = self
            .connection
            .call(method, Some(params))
            .await
            .map_err(to_remote_fs_error)?;
        Ok(serde_json::from_value(result)?)
    }

    async fn read_range(
        &self,
        path: &str,
        offset: u64,
        max_bytes: Option<u64>,
    ) -> Result<FsReadFileResult, WireAsyncError> {
        let mut params = Map::from_iter([
            ("path".to_string(), Value::String(path.to_string())),
            ("offset".to_string(), Value::from(offset)),
        ]);
        if let Some(max_bytes) = max_bytes {
            params.insert("maxBytes".to_string(), Value::from(max_bytes));
        }
        self.call(FS_READ_FILE_METHOD, Value::Object(params)).await
    }

    async fn write_mode(
        &self,
        path: &str,
        data: &[u8],
        mode: FsWriteMode,
    ) -> Result<(), WireAsyncError> {
        self.call::<Value>(
            FS_WRITE_FILE_METHOD,
            json!({ "path": path, "dataBase64": encode_base64(data), "mode": mode }),
        )
        .await?;
        Ok(())
    }

    async fn write_chunked(
        &self,
        path: &str,
        data: &[u8],
        mode: FsWriteMode,
    ) -> Result<(), WireAsyncError> {
        if data.len() <= FS_WRITE_FILE_CHUNK_BYTES as usize {
            return self.write_mode(path, data, mode).await;
        }
        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + FS_WRITE_FILE_CHUNK_BYTES as usize).min(data.len());
            self.write_mode(
                path,
                &data[offset..end],
                if offset == 0 { mode } else { FsWriteMode::Append },
            )
            .await?;
            offset = end;
        }
        Ok(())
    }

    fn read_chunks(&self, path: &str, offset: u64) -> crate::persistence::interface::storage::StorageByteStream {
        let this = Self {
            connection: self.connection.clone(),
        };
        let path = path.to_string();
        Box::pin(futures_util::stream::try_unfold(
            (this, path, offset, false),
            |(this, path, offset, done)| async move {
                if done {
                    return Ok(None);
                }
                let result = this
                    .read_range(&path, offset, Some(FS_READ_FILE_MAX_BYTES))
                    .await?;
                if result.data_base64.is_empty() {
                    return Ok(None);
                }
                let chunk = decode_base64(&result.data_base64);
                let next_offset = offset + chunk.len() as u64;
                Ok(Some((chunk, (this, path, next_offset, result.eof))))
            },
        ))
    }

    async fn metadata(&self, path: &str, follow_symlinks: bool) -> Result<HostFileStat, WireAsyncError> {
        let result = self
            .call::<FsGetMetadataResult>(
                FS_GET_METADATA_METHOD,
                json!({ "path": path, "followSymlinks": follow_symlinks }),
            )
            .await?;
        Ok(to_host_file_stat(result))
    }
}

#[async_trait]
impl IHostFileSystem for RemoteFileSystem {
    async fn read_text(
        &self,
        path: &str,
        options: Option<HostReadTextOptions>,
    ) -> Result<String, WireAsyncError> {
        let bytes = self.read_bytes(path, u64::MAX, None).await?;
        let options = options.unwrap_or_default();
        decode_text_with_errors(
            &bytes,
            &options.encoding.unwrap_or_else(|| "utf-8".to_string()),
            options.errors.unwrap_or_default(),
            options.preserve_bom.unwrap_or(false),
        )
    }

    async fn write_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError> {
        self.write_chunked(path, data.as_bytes(), FsWriteMode::Truncate).await
    }

    async fn append_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError> {
        self.write_chunked(path, data.as_bytes(), FsWriteMode::Append).await
    }

    async fn read_bytes(
        &self,
        path: &str,
        n: u64,
        offset: Option<u64>,
    ) -> Result<Vec<u8>, WireAsyncError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        let mut chunks = self.read_chunks(path, offset.unwrap_or(0));
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            let length = if n == u64::MAX {
                chunk.len()
            } else {
                chunk.len().min((n as usize).saturating_sub(output.len()))
            };
            if length == 0 {
                break;
            }
            output.extend(&chunk[..length]);
            if output.len() == n as usize {
                break;
            }
        }
        Ok(output)
    }

    async fn read_file_head(&self, path: &str, max_bytes: u64) -> Result<HostFileHead, WireAsyncError> {
        let result = self
            .call::<FsReadFileResult>(
                FS_READ_FILE_METHOD,
                json!({
                    "path": path,
                    "maxBytes": max_bytes.min(FS_READ_FILE_MAX_BYTES),
                    "includeMetadata": true,
                }),
            )
            .await?;
        Ok(HostFileHead {
            stat: to_host_file_stat(result.metadata.ok_or_else(|| {
                std::io::Error::other("remote readFile response omitted metadata")
            })?),
            bytes: decode_base64(&result.data_base64),
        })
    }

    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), WireAsyncError> {
        self.write_chunked(path, data, FsWriteMode::Truncate).await
    }

    fn read_lines(
        &self,
        path: &str,
        options: Option<HostReadLinesOptions>,
    ) -> HostLineStream {
        let options = options.unwrap_or_default();
        let encoding = options.encoding.unwrap_or_else(|| "utf-8".to_string());
        let errors = options.errors.unwrap_or_default();
        if encoding != "utf-8" && encoding != "utf8" {
            let this = Self {
                connection: self.connection.clone(),
            };
            let path = path.to_string();
            return Box::pin(futures_util::stream::once(async move {
                let options = HostReadTextOptions {
                    encoding: Some(encoding),
                    errors: Some(errors),
                    preserve_bom: None,
                };
                this.read_text(&path, Some(options)).await
            })
            .flat_map(|result| -> HostLineStream {
                match result {
                    Err(error) => Box::pin(futures_util::stream::once(async { Err(error) })),
                    Ok(text) => Box::pin(futures_util::stream::iter(
                        text.split_inclusive('\n')
                            .map(|line| Ok::<String, WireAsyncError>(line.to_string()))
                            .collect::<Vec<_>>(),
                    )),
                }
            }));
        }
        read_utf8_lines(self.read_chunks(path, 0), errors)
    }

    async fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, WireAsyncError> {
        match self.write_chunked(path, data, FsWriteMode::Exclusive).await {
            Ok(()) => Ok(true),
            Err(error) if error.downcast_ref::<HostFsError>().is_some_and(|error| {
                error.code().as_str() == OS_FS_ALREADY_EXISTS
            }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    async fn stat(&self, path: &str) -> Result<HostFileStat, WireAsyncError> {
        self.metadata(path, true).await
    }

    async fn lstat(&self, path: &str) -> Result<HostFileStat, WireAsyncError> {
        self.metadata(path, false).await
    }

    async fn readdir(&self, path: &str) -> Result<Vec<HostDirEntry>, WireAsyncError> {
        let result = self
            .call::<FsReadDirectoryResult>(FS_READ_DIRECTORY_METHOD, json!({ "path": path }))
            .await?;
        if result.truncated {
            return Err(Box::new(HostFsError::new(
                OS_FS_DIRECTORY_TOO_LARGE,
                format!("readdir {path} exceeds the {FS_READ_DIRECTORY_MAX_ENTRIES}-entry limit"),
                Some(json!({ "path": path, "op": "readdir", "limit": FS_READ_DIRECTORY_MAX_ENTRIES })),
            )));
        }
        Ok(result
            .entries
            .into_iter()
            .map(|entry| HostDirEntry {
                name: entry.file_name,
                is_file: entry.is_file,
                is_directory: entry.is_directory,
                is_symbolic_link: entry.is_symlink.unwrap_or(false),
            })
            .collect())
    }

    async fn walk(
        &self,
        path: &str,
        ignore: Option<&[String]>,
        deadline_ms: Option<i64>,
        visit: HostWalkVisitor,
    ) -> Result<HostWalkResult, WireAsyncError> {
        let remaining_ms = deadline_ms.map_or(FS_WALK_DEFAULT_TIMEOUT_MS as i64, |deadline| {
            deadline - chrono::Utc::now().timestamp_millis()
        });
        if remaining_ms <= 0 {
            return Ok(HostWalkResult { truncated: true });
        }
        let result = self
            .call::<FsWalkResult>(
                FS_WALK_METHOD,
                json!({
                    "path": path,
                    "ignore": ignore,
                    "timeoutMs": (remaining_ms as u64).min(FS_WALK_DEFAULT_TIMEOUT_MS),
                }),
            )
            .await?;
        for entry in result.entries {
            if deadline_ms.is_some_and(|deadline| {
                chrono::Utc::now().timestamp_millis() >= deadline
            }) {
                return Ok(HostWalkResult { truncated: true });
            }
            let kind = match entry.kind {
                FsWalkKind::File => crate::os::interface::host_file_system::HostWalkKind::File,
                FsWalkKind::Directory => crate::os::interface::host_file_system::HostWalkKind::Directory,
                FsWalkKind::Symlink => crate::os::interface::host_file_system::HostWalkKind::Symlink,
            };
            if !visit(HostWalkEntry {
                path: entry.path,
                kind,
            })
            .await
            {
                return Ok(HostWalkResult { truncated: true });
            }
        }
        Ok(HostWalkResult {
            truncated: result.truncated,
        })
    }

    async fn mkdir(&self, path: &str, recursive: bool) -> Result<(), WireAsyncError> {
        self.call::<Value>(
            FS_CREATE_DIRECTORY_METHOD,
            json!({ "path": path, "recursive": recursive }),
        )
        .await?;
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<(), WireAsyncError> {
        self.call::<Value>(FS_REMOVE_METHOD, json!({ "path": path })).await?;
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), WireAsyncError> {
        self.call::<Value>(FS_RENAME_METHOD, json!({ "from": from, "to": to }))
            .await?;
        Ok(())
    }

    async fn realpath(&self, path: &str) -> Result<String, WireAsyncError> {
        let result = self
            .call::<Value>(FS_CANONICALIZE_METHOD, json!({ "path": path }))
            .await?;
        Ok(result
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

impl RemoteFileSystem {
    pub async fn write_bytes_stream(
        &self,
        path: &str,
        mut source: crate::persistence::interface::storage::StorageByteStream,
    ) -> Result<(), WireAsyncError> {
        let mut mode = FsWriteMode::Truncate;
        let mut pending = Vec::with_capacity(FS_WRITE_FILE_CHUNK_BYTES as usize);
        while let Some(chunk) = source.next().await {
            let chunk = chunk?;
            let mut offset = 0usize;
            while offset < chunk.len() {
                let length = (FS_WRITE_FILE_CHUNK_BYTES as usize - pending.len())
                    .min(chunk.len() - offset);
                pending.extend(&chunk[offset..offset + length]);
                offset += length;
                if pending.len() == FS_WRITE_FILE_CHUNK_BYTES as usize {
                    self.write_mode(path, &pending, mode).await?;
                    mode = FsWriteMode::Append;
                    pending.clear();
                }
            }
        }
        if !pending.is_empty() || mode == FsWriteMode::Truncate {
            self.write_mode(path, &pending, mode).await?;
        }
        Ok(())
    }
}
