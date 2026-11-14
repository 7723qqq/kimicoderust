use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;

use crate::_base::exec_env::decode_text::{
    ByteStream, TextDecodeErrors, read_utf8_lines,
};
use crate::agent::media::media_ref::parse_daemon_file_url;
use crate::server::files::{FileMeta, FileStore};
use crate::os::interface::host_file_system::{
    HostFileHead, HostFileStat, HostLineStream, HostReadLinesOptions, IHostFileSystem,
};
use crate::rpc::types::BoxFuture;
use crate::turn_loop::types::ExecutableToolResult;
use crate::wire::record::WireAsyncError;

pub trait FileReadSource: Send + Sync {
    fn name(&self) -> &str;
    fn local_path(&self) -> Option<&str>;
    fn stat(&self) -> BoxFuture<'static, Result<HostFileStat, WireAsyncError>>;
    fn read_head(&self, max_bytes: u64) -> BoxFuture<'static, Result<HostFileHead, WireAsyncError>>;
    fn read_bytes(&self, n: Option<u64>) -> BoxFuture<'static, Result<Vec<u8>, WireAsyncError>>;
    fn read_lines(&self) -> HostLineStream;
}

pub fn with_attachment_location(
    mut result: ExecutableToolResult,
    source: &dyn FileReadSource,
) -> ExecutableToolResult {
    if !result.is_error || !result.parts.is_empty() {
        return result;
    }
    let Some(local_path) = source.local_path() else {
        return result;
    };
    let path = serde_json::Value::String(local_path.to_string()).to_string();
    result.content = format!("{}\nServer-local attachment path: {path}", result.content);
    result
}

#[derive(Clone)]
pub struct EnvironmentFileSource {
    fs: Arc<dyn IHostFileSystem>,
    path: String,
}

pub fn environment_file_source(
    fs: Arc<dyn IHostFileSystem>,
    path: impl Into<String>,
) -> EnvironmentFileSource {
    EnvironmentFileSource {
        fs,
        path: path.into(),
    }
}

impl FileReadSource for EnvironmentFileSource {
    fn name(&self) -> &str {
        &self.path
    }

    fn local_path(&self) -> Option<&str> {
        None
    }

    fn stat(&self) -> BoxFuture<'static, Result<HostFileStat, WireAsyncError>> {
        let fs = self.fs.clone();
        let path = self.path.clone();
        Box::pin(async move { fs.stat(&path).await })
    }

    fn read_head(&self, max_bytes: u64) -> BoxFuture<'static, Result<HostFileHead, WireAsyncError>> {
        let fs = self.fs.clone();
        let path = self.path.clone();
        Box::pin(async move { fs.read_file_head(&path, max_bytes).await })
    }

    fn read_bytes(&self, n: Option<u64>) -> BoxFuture<'static, Result<Vec<u8>, WireAsyncError>> {
        let fs = self.fs.clone();
        let path = self.path.clone();
        Box::pin(async move { fs.read_bytes(&path, n.unwrap_or(u64::MAX), None).await })
    }

    fn read_lines(&self) -> HostLineStream {
        self.fs.read_lines(
            &self.path,
            Some(HostReadLinesOptions {
                encoding: None,
                errors: Some(TextDecodeErrors::Strict),
            }),
        )
    }
}

#[derive(Clone)]
pub struct AttachmentFileSource {
    reference: String,
    store: Option<FileStore>,
    name: String,
    local_path: Option<String>,
}

impl AttachmentFileSource {
    fn unavailable(&self) -> WireAsyncError {
        io::Error::other(format!(
            "Attachment {} is not available in the current session.",
            serde_json::Value::String(self.reference.clone()).to_string()
        ))
        .into()
    }

    fn open(&self) -> Result<(FileMeta, PathBuf), WireAsyncError> {
        let Some(reference) = parse_daemon_file_url(&self.reference) else {
            return Err(self.unavailable());
        };
        let Some(store) = self.store.as_ref() else {
            return Err(self.unavailable());
        };
        store.get(reference).map_err(|_| self.unavailable())
    }
}

pub fn attachment_file_source(
    reference: impl Into<String>,
    store: Option<&FileStore>,
) -> Result<AttachmentFileSource, WireAsyncError> {
    let mut source = AttachmentFileSource {
        reference: reference.into(),
        store: store.cloned(),
        name: String::new(),
        local_path: None,
    };
    let (initial, path) = source.open()?;
    source.name = initial.name;
    source.local_path = Some(path.to_string_lossy().into_owned());
    Ok(source)
}

impl FileReadSource for AttachmentFileSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn local_path(&self) -> Option<&str> {
        self.local_path.as_deref()
    }

    fn stat(&self) -> BoxFuture<'static, Result<HostFileStat, WireAsyncError>> {
        let source = self.clone();
        Box::pin(async move {
            let (file, _) = source.open()?;
            Ok(HostFileStat {
                is_file: true,
                is_directory: false,
                size: file.size,
                ..HostFileStat::default()
            })
        })
    }

    fn read_head(&self, max_bytes: u64) -> BoxFuture<'static, Result<HostFileHead, WireAsyncError>> {
        let source = self.clone();
        Box::pin(async move {
            let stat = source.stat().await?;
            let bytes = source.read_bytes(Some(max_bytes)).await?;
            Ok(HostFileHead { stat, bytes })
        })
    }

    fn read_bytes(&self, n: Option<u64>) -> BoxFuture<'static, Result<Vec<u8>, WireAsyncError>> {
        let source = self.clone();
        Box::pin(async move {
            let (file, path) = source.open()?;
            let size = n.unwrap_or(file.size).min(file.size);
            if size == 0 {
                return Ok(Vec::new());
            }
            use tokio::io::AsyncReadExt;
            let file = tokio::fs::File::open(path).await?;
            let mut bytes = Vec::new();
            file.take(size).read_to_end(&mut bytes).await?;
            if bytes.len() as u64 != size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Attachment changed or became unavailable while reading.",
                )
                .into());
            }
            Ok(bytes)
        })
    }

    fn read_lines(&self) -> HostLineStream {
        let lines = read_utf8_lines(
            attachment_byte_stream(self.clone()),
            TextDecodeErrors::Strict,
        );
        Box::pin(lines.map(|line| {
            line.map_err(|error| Box::new(error) as WireAsyncError)
        }))
    }
}

struct AttachmentStreamState {
    source: AttachmentFileSource,
    file: Option<tokio::fs::File>,
    expected: Option<u64>,
    read: u64,
}

fn attachment_byte_stream(source: AttachmentFileSource) -> ByteStream {
    Box::pin(futures_util::stream::try_unfold(
        AttachmentStreamState {
            source,
            file: None,
            expected: None,
            read: 0,
        },
        |mut state| async move {
            if state.file.is_none() {
                let (meta, path) = state
                    .source
                    .open()
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let file = tokio::fs::File::open(path).await?;
                state.expected = Some(meta.size);
                state.file = Some(file);
            }
            let mut buffer = vec![0; 64 * 1024];
            let read = {
                use tokio::io::AsyncReadExt;
                state
                    .file
                    .as_mut()
                    .expect("attachment stream file is initialized")
                    .read(&mut buffer)
                    .await?
            };
            if read == 0 {
                if state.read != state.expected.unwrap_or(0) {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Attachment changed or became unavailable while reading.",
                    ));
                }
                return Ok(None);
            }
            state.read += read as u64;
            buffer.truncate(read);
            Ok(Some((buffer, state)))
        },
    ))
}
