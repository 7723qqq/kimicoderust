use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, future::BoxFuture};

use crate::_base::exec_env::decode_text::{decode_text_with_errors, read_utf8_lines};
use crate::os::interface::host_file_system::{
    HostDirEntry, HostFileHead, HostFileStat, HostLineStream, HostReadLinesOptions,
    HostReadTextOptions, HostWalkEntry, HostWalkMatcher, HostWalkOptions, HostWalkResult,
    HostWalkVisitor, IHostFileSystem,
};
use crate::os::interface::host_fs_errors::{
    HostFsError, OS_FS_DIRECTORY_TOO_LARGE, to_host_fs_error,
};
use crate::wire::record::WireAsyncError;

const READ_CHUNK_SIZE: usize = 64 * 1024;
const WALK_MAX_DEPTH: usize = 64;

pub async fn walk_directory_tree(
    fs: Arc<dyn IHostFileSystem>,
    root: &str,
    options: HostWalkOptions,
    visit: HostWalkVisitor,
) -> Result<HostWalkResult, WireAsyncError> {
    let matcher = match options.matcher {
        Some(matcher) => Some(matcher),
        None => options.ignore.as_ref().map(|patterns| {
            Arc::new(IgnoreMatcher::new(root, patterns)) as Arc<dyn HostWalkMatcher>
        }),
    };
    let deadline = options.deadline_ms;
    let truncated = !walk_directory(fs, root.to_string(), String::new(), 0, matcher, deadline, visit).await?;
    Ok(HostWalkResult { truncated })
}

fn walk_directory(
    fs: Arc<dyn IHostFileSystem>,
    root: String,
    relative_dir: String,
    depth: usize,
    matcher: Option<Arc<dyn HostWalkMatcher>>,
    deadline_ms: Option<i64>,
    visit: HostWalkVisitor,
) -> BoxFuture<'static, Result<bool, WireAsyncError>> {
    async move {
        if depth > WALK_MAX_DEPTH {
            return Ok(true);
        }
        if expired(deadline) {
            return Ok(false);
        }
        let path = if relative_dir.is_empty() {
            root.clone()
        } else {
            Path::new(&root).join(&relative_dir).to_string_lossy().into_owned()
        };
        let entries = match fs.readdir(&path).await {
            Ok(entries) => entries,
            Err(error) => {
                if error.downcast_ref::<HostFsError>().is_some_and(|error| {
                    error.code().as_str() == OS_FS_DIRECTORY_TOO_LARGE
                }) {
                    return Err(error);
                }
                return Ok(true);
            }
        };
        for entry in entries {
            let path = if relative_dir.is_empty() {
                entry.name.clone()
            } else {
                format!("{relative_dir}/{}", entry.name)
            };
            let is_dir = entry.is_directory && !entry.is_symbolic_link;
            if matcher.as_ref().is_some_and(|matcher| {
                matcher.ignores(if is_dir { format!("{path}/") } else { path.clone() }.as_str())
            }) {
                continue;
            }
            if expired(deadline) {
                return Ok(false);
            }
            let kind = if entry.is_symbolic_link {
                crate::os::interface::host_file_system::HostWalkKind::Symlink
            } else if is_dir {
                crate::os::interface::host_file_system::HostWalkKind::Directory
            } else {
                crate::os::interface::host_file_system::HostWalkKind::File
            };
            if !visit(HostWalkEntry {
                path: path.clone(),
                kind,
            })
            .await
            {
                return Ok(false);
            }
            if is_dir
                && !walk_directory(
                    fs.clone(),
                    root.clone(),
                    path,
                    depth + 1,
                    matcher.clone(),
                    deadline_ms,
                    visit.clone(),
                )
                .await?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
    .boxed()
}

struct IgnoreMatcher {
    matcher: ignore::gitignore::Gitignore,
}

impl IgnoreMatcher {
    fn new(root: &str, patterns: &[String]) -> Self {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        for pattern in patterns {
            builder.add_line(None, pattern).ok();
        }
        Self {
            matcher: builder.build().unwrap_or_else(|_| ignore::gitignore::Gitignore::empty()),
        }
    }
}

impl HostWalkMatcher for IgnoreMatcher {
    fn ignores(&self, path: &str) -> bool {
        self.matcher.matched(path, path.ends_with('/')).is_ignore()
    }
}

fn expired(deadline_ms: Option<i64>) -> bool {
    deadline_ms.is_some_and(|deadline| chrono::Utc::now().timestamp_millis() >= deadline)
}

pub struct HostFileSystem;

impl HostFileSystem {
    pub fn new() -> Self {
        Self
    }

    pub async fn append_bytes(&self, path: &str, data: &[u8]) -> Result<(), WireAsyncError> {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(|error| host_error(error, path, "append"))?;
        std::io::Write::write_all(&mut file, data)
            .map_err(|error| host_error(error, path, "append"))?;
        Ok(())
    }

    pub async fn write_bytes_stream(
        &self,
        path: &str,
        mut source: crate::persistence::interface::storage::StorageByteStream,
    ) -> Result<(), WireAsyncError> {
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|error| host_error(error, path, "write"))?;
        while let Some(chunk) = source.next().await {
            let chunk = chunk?;
            use tokio::io::AsyncWriteExt;
            file.write_all(&chunk)
                .await
                .map_err(|error| host_error(error, path, "write"))?;
        }
        Ok(())
    }
}

impl Default for HostFileSystem {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IHostFileSystem for HostFileSystem {
    async fn read_text(
        &self,
        path: &str,
        options: Option<HostReadTextOptions>,
    ) -> Result<String, WireAsyncError> {
        let bytes = std::fs::read(path).map_err(|error| host_error(error, path, "read"))?;
        let options = options.unwrap_or_default();
        decode_text_with_errors(
            &bytes,
            &options.encoding.unwrap_or_else(|| "utf-8".to_string()),
            options.errors.unwrap_or_default(),
            options.preserve_bom.unwrap_or(false),
        )
        .map_err(|error| to_host_fs_error(error, path, "read"))
    }

    async fn write_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError> {
        std::fs::write(path, data).map_err(|error| host_error(error, path, "write"))?;
        Ok(())
    }

    async fn append_text(&self, path: &str, data: &str) -> Result<(), WireAsyncError> {
        self.append_bytes(path, data.as_bytes()).await
    }

    async fn read_bytes(
        &self,
        path: &str,
        n: u64,
        offset: Option<u64>,
    ) -> Result<Vec<u8>, WireAsyncError> {
        let offset = offset.unwrap_or(0);
        if n == u64::MAX && offset == 0 {
            return std::fs::read(path).map_err(|error| host_error(error, path, "read"));
        }
        let mut file = std::fs::File::open(path).map_err(|error| host_error(error, path, "read"))?;
        let length = if n == u64::MAX {
            file.metadata()
                .map_err(|error| host_error(error, path, "read"))?
                .len()
                .saturating_sub(offset)
        } else {
            n
        };
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| host_error(error, path, "read"))?;
        let mut buffer = vec![0; length.min(usize::MAX as u64) as usize];
        let read = file
            .read(&mut buffer)
            .map_err(|error| host_error(error, path, "read"))?;
        buffer.truncate(read);
        Ok(buffer)
    }

    async fn read_file_head(&self, path: &str, max_bytes: u64) -> Result<HostFileHead, WireAsyncError> {
        let stat = self.stat(path).await?;
        let bytes = if stat.is_file {
            self.read_bytes(path, max_bytes, None).await?
        } else {
            Vec::new()
        };
        Ok(HostFileHead { stat, bytes })
    }

    async fn write_bytes(&self, path: &str, data: &[u8]) -> Result<(), WireAsyncError> {
        std::fs::write(path, data).map_err(|error| host_error(error, path, "write"))?;
        Ok(())
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
            let path = path.to_string();
            return Box::pin(futures_util::stream::once(async move {
                let bytes = std::fs::read(&path).map_err(|error| host_error(error, &path, "read"))?;
                let text = decode_text_with_errors(&bytes, &encoding, errors, false)
                    .map_err(|error| to_host_fs_error(error, &path, "read"))?;
                Ok(text)
            })
            .flat_map(|result| -> HostLineStream { match result {
                Err(error) => Box::pin(futures_util::stream::once(async { Err(error) })),
                Ok(text) => Box::pin(futures_util::stream::iter(
                    split_lines_keeping_terminator(&text)
                        .into_iter()
                        .map(|line| Ok::<String, WireAsyncError>(line)),
                )),
            }}));
        }
        let path = path.to_string();
        let bytes = byte_stream(path);
        read_utf8_lines(bytes, errors)
    }

    async fn create_exclusive(&self, path: &str, data: &[u8]) -> Result<bool, WireAsyncError> {
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(host_error(error, path, "create")),
        };
        std::io::Write::write_all(&mut file, data)
            .map_err(|error| host_error(error, path, "create"))?;
        file.sync_all().map_err(|error| host_error(error, path, "create"))?;
        Ok(true)
    }

    async fn stat(&self, path: &str) -> Result<HostFileStat, WireAsyncError> {
        let stat = std::fs::metadata(path).map_err(|error| host_error(error, path, "stat"))?;
        Ok(file_stat(stat))
    }

    async fn lstat(&self, path: &str) -> Result<HostFileStat, WireAsyncError> {
        let stat = std::fs::symlink_metadata(path).map_err(|error| host_error(error, path, "lstat"))?;
        Ok(file_stat(stat))
    }

    async fn readdir(&self, path: &str) -> Result<Vec<HostDirEntry>, WireAsyncError> {
        let entries = std::fs::read_dir(path).map_err(|error| host_error(error, path, "readdir"))?;
        let mut output = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| host_error(error, path, "readdir"))?;
            let file_type = entry
                .file_type()
                .map_err(|error| host_error(error, path, "readdir"))?;
            output.push(HostDirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_file: file_type.is_file(),
                is_directory: file_type.is_dir(),
                is_symbolic_link: file_type.is_symlink(),
            });
        }
        Ok(output)
    }

    async fn walk(
        &self,
        path: &str,
        ignore: Option<&[String]>,
        deadline_ms: Option<i64>,
        visit: HostWalkVisitor,
    ) -> Result<HostWalkResult, WireAsyncError> {
        walk_directory_tree(
            Arc::new(Self::new()),
            path,
            HostWalkOptions {
                ignore: ignore.map(<[String]>::to_vec),
                matcher: None,
                deadline_ms,
            },
            visit,
        )
        .await
    }

    async fn mkdir(&self, path: &str, recursive: bool) -> Result<(), WireAsyncError> {
        if recursive {
            std::fs::create_dir_all(path)
        } else {
            std::fs::create_dir(path)
        }
        .map_err(|error| host_error(error, path, "mkdir"))?;
        Ok(())
    }

    async fn remove(&self, path: &str) -> Result<(), WireAsyncError> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(host_error(error, path, "remove")),
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        }
        .map_err(|error| host_error(error, path, "remove"))?;
        Ok(())
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), WireAsyncError> {
        std::fs::rename(from, to).map_err(|error| host_error(error, from, "rename"))?;
        Ok(())
    }

    async fn realpath(&self, path: &str) -> Result<String, WireAsyncError> {
        Ok(std::fs::canonicalize(path)
            .map_err(|error| host_error(error, path, "realpath"))?
            .to_string_lossy()
            .into_owned())
    }
}

fn file_stat(stat: std::fs::Metadata) -> HostFileStat {
    #[cfg(unix)]
    let (ino, mode) = {
        use std::os::unix::fs::MetadataExt;
        (Some(stat.ino()), Some(stat.mode() & 0o7777))
    };
    #[cfg(not(unix))]
    let (ino, mode) = (None, None);
    HostFileStat {
        is_file: stat.is_file(),
        is_directory: stat.is_dir(),
        is_symbolic_link: stat.file_type().is_symlink(),
        size: stat.len(),
        mtime_ms: stat
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64() * 1000.0),
        ino,
        mode,
    }
}

fn split_lines_keeping_terminator(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n')
        .map(str::to_string)
        .collect::<Vec<_>>()
}

fn byte_stream(path: String) -> crate::persistence::interface::storage::StorageByteStream {
    Box::pin(futures_util::stream::try_unfold(
        (path, None, false),
        |(path, file, done)| async move {
            if done {
                return Ok(None);
            }
            let mut file = match file {
                Some(file) => file,
                None => tokio::fs::File::open(&path)
                    .await
                    .map_err(|error| host_error(error, &path, "read"))?,
            };
            let mut buffer = vec![0; READ_CHUNK_SIZE];
            use tokio::io::AsyncReadExt;
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| host_error(error, &path, "read"))?;
            if read == 0 {
                return Ok(None);
            }
            buffer.truncate(read);
            Ok(Some((buffer, (path, Some(file), false))))
        },
    ))
}

fn host_error(error: std::io::Error, path: &str, op: &str) -> WireAsyncError {
    to_host_fs_error(Box::new(error), path, op)
}
