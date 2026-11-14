use std::collections::HashSet;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use parking_lot::Mutex;

use crate::_base::utils::fs::{atomic_write, sync_dir};
use crate::persistence::interface::storage::{
    IFileSystemStorageService, StorageAppendOptions, StorageByteStream, StorageListOptions,
    StorageReadRange, StorageWriteOptions, to_storage_io_error,
};
use crate::wire::record::WireAsyncError;

const TORN_READ_RETRIES: usize = 3;
const TORN_READ_RETRY_DELAY: Duration = Duration::from_millis(15);
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Clone)]
pub struct FileStorageService {
    base_dir: PathBuf,
    dir_mode: Option<u32>,
    file_mode: Option<u32>,
    synced_dirs: Arc<Mutex<HashSet<PathBuf>>>,
}

impl FileStorageService {
    pub fn new(
        base_dir: impl Into<PathBuf>,
        dir_mode: Option<u32>,
        file_mode: Option<u32>,
    ) -> Self {
        Self {
            base_dir: base_dir.into(),
            dir_mode,
            file_mode,
            synced_dirs: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn file_path(&self, scope: &str, key: &str) -> PathBuf {
        self.base_dir.join(scope).join(key)
    }

    fn scope_path(&self, scope: &str) -> PathBuf {
        self.base_dir.join(scope)
    }

    fn stat(&self, scope: &str, key: &str) -> Result<Option<Metadata>, WireAsyncError> {
        let path = self.file_path(scope, key);
        match fs::metadata(&path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_io(error, &path, "stat")),
        }
    }

    fn sync_dir_once(&self, dir: &Path) -> Result<(), WireAsyncError> {
        if self.synced_dirs.lock().contains(dir) {
            return Ok(());
        }
        match sync_dir(dir) {
            Ok(()) => {
                self.synced_dirs.lock().insert(dir.to_path_buf());
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }
}

#[async_trait]
impl IFileSystemStorageService for FileStorageService {
    async fn read(&self, scope: &str, key: &str) -> Result<Option<Vec<u8>>, WireAsyncError> {
        let path = self.file_path(scope, key);
        for attempt in 0..=TORN_READ_RETRIES {
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(storage_io(error, &path, "read")),
            };
            if attempt >= TORN_READ_RETRIES {
                return Ok(Some(bytes));
            }
            match fs::metadata(&path) {
                Ok(metadata) if metadata.len() != bytes.len() as u64 => {
                    tokio::time::sleep(TORN_READ_RETRY_DELAY).await;
                }
                _ => return Ok(Some(bytes)),
            }
        }
        unreachable!()
    }

    fn read_stream(
        &self,
        scope: &str,
        key: &str,
        range: Option<StorageReadRange>,
    ) -> StorageByteStream {
        let state = FileReadStream {
            path: self.file_path(scope, key),
            file: None,
            range,
            remaining: range.and_then(|range| {
                range
                    .end
                    .checked_sub(range.start)
                    .and_then(|difference| difference.checked_add(1))
            }),
            invalid_range: range.is_some_and(|range| {
                range
                    .end
                    .checked_sub(range.start)
                    .and_then(|difference| difference.checked_add(1))
                    .is_none()
            }),
            done: false,
        };
        Box::pin(futures_util::stream::try_unfold(state, |mut state| async move {
            if state.done {
                return Ok(None);
            }
            if state.invalid_range {
                state.done = true;
                return Err(storage_io(
                    io::Error::new(io::ErrorKind::InvalidInput, "invalid read range"),
                    &state.path,
                    "read",
                ));
            }
            if state.file.is_none() {
                let mut file = match tokio::fs::File::open(&state.path).await {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        state.done = true;
                        return Ok(None);
                    }
                    Err(error) => {
                        state.done = true;
                        return Err(storage_io(error, &state.path, "read"));
                    }
                };
                if let Some(range) = state.range {
                    use tokio::io::AsyncSeekExt;
                    if let Err(error) = file.seek(SeekFrom::Start(range.start)).await {
                        state.done = true;
                        return Err(storage_io(error, &state.path, "read"));
                    }
                }
                state.file = Some(file);
            }
            if state.remaining == Some(0) {
                state.done = true;
                return Ok(None);
            }
            let length = state
                .remaining
                .map_or(STREAM_BUFFER_SIZE, |remaining| {
                    remaining.min(STREAM_BUFFER_SIZE as u64) as usize
                });
            let mut buffer = vec![0; length];
            use tokio::io::AsyncReadExt;
            match state.file.as_mut().unwrap().read(&mut buffer).await {
                Ok(0) => {
                    state.done = true;
                    Ok(None)
                }
                Ok(read) => {
                    buffer.truncate(read);
                    if let Some(remaining) = state.remaining.as_mut() {
                        *remaining = remaining.saturating_sub(read as u64);
                    }
                    Ok(Some((buffer, state)))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    state.done = true;
                    Ok(None)
                }
                Err(error) => {
                    state.done = true;
                    Err(storage_io(error, &state.path, "read"))
                }
            }
        }))
    }

    async fn write(
        &self,
        scope: &str,
        key: &str,
        data: Vec<u8>,
        options: Option<StorageWriteOptions>,
    ) -> Result<(), WireAsyncError> {
        let path = self.file_path(scope, key);
        let options = options.unwrap_or_default();
        let cancellation = options.signal.as_ref().map(|signal| signal.cancellation_token());
        let result = (|| {
            create_dir_all(parent_dir(&path), self.dir_mode)?;
            atomic_write(&path, &data, None, self.file_mode, cancellation.as_ref())?;
            self.sync_dir_once(parent_dir(&path))
        })();
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Some(signal) = &options.signal {
                    signal.throw_if_aborted()?;
                }
                Err(to_storage_io_error(
                    error,
                    &path.to_string_lossy().replace('\\', "/"),
                    "write",
                ))
            }
        }
    }

    async fn write_stream(
        &self,
        scope: &str,
        key: &str,
        mut source: StorageByteStream,
        options: Option<StorageWriteOptions>,
    ) -> Result<(), WireAsyncError> {
        let path = self.file_path(scope, key);
        let options = options.unwrap_or_default();
        let result = async {
            create_dir_all(parent_dir(&path), self.dir_mode)?;
            atomic_write_stream_async(
                &path,
                &mut source,
                self.file_mode,
                options.signal.as_ref(),
            )
            .await?;
            self.sync_dir_once(parent_dir(&path))
        }
        .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Some(signal) = &options.signal {
                    signal.throw_if_aborted()?;
                }
                Err(to_storage_io_error(
                    error,
                    &path.to_string_lossy().replace('\\', "/"),
                    "write",
                ))
            }
        }
    }

    async fn append(
        &self,
        scope: &str,
        key: &str,
        data: Vec<u8>,
        options: Option<StorageAppendOptions>,
    ) -> Result<(), WireAsyncError> {
        let path = self.file_path(scope, key);
        let dir = parent_dir(&path);
        let durable = options.and_then(|options| options.durable) != Some(false);
        let result = (|| {
            create_dir_all(dir, self.dir_mode)?;
            let mut open_options = OpenOptions::new();
            open_options.append(true).create(true);
            #[cfg(unix)]
            if let Some(mode) = self.file_mode {
                use std::os::unix::fs::OpenOptionsExt;
                open_options.mode(mode);
            }
            let mut file = open_options.open(&path)?;
            if !data.is_empty() {
                file.write_all(&data)?;
            }
            if durable {
                file.sync_all()?;
            }
            drop(file);
            self.sync_dir_once(dir)
        })();
        result.map_err(|error| match error.downcast::<io::Error>() {
            Ok(error) => storage_io(error, &path, "append"),
            Err(error) => error,
        })
    }

    async fn list(
        &self,
        scope: &str,
        prefix: Option<&str>,
        options: Option<StorageListOptions>,
    ) -> Result<Vec<String>, WireAsyncError> {
        let scope_path = self.scope_path(scope);
        let recursive = options.and_then(|options| options.recursive) == Some(true);
        let keys = if recursive {
            collect_file_keys(&scope_path, "")
                .map_err(|error| storage_io(error, &scope_path, "list"))?
        } else {
            let entries = match fs::read_dir(&scope_path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(storage_io(error, &scope_path, "list")),
            };
            let mut keys = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => keys.push(entry.file_name().to_string_lossy().into_owned()),
                    Err(error) => return Err(storage_io(error, &scope_path, "list")),
                }
            }
            keys
        };
        Ok(keys
            .into_iter()
            .filter(|key| prefix.is_none_or(|prefix| key.starts_with(prefix)))
            .collect())
    }

    async fn delete(&self, scope: &str, key: &str) -> Result<(), WireAsyncError> {
        let path = self.file_path(scope, key);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(storage_io(error, &path, "delete")),
        }
    }

    async fn size(&self, scope: &str, key: &str) -> Result<Option<u64>, WireAsyncError> {
        Ok(self.stat(scope, key)?.map(|metadata| metadata.len()))
    }

    fn size_sync(&self, scope: &str, key: &str) -> Option<u64> {
        let path = self.file_path(scope, key);
        match fs::metadata(&path) {
            Ok(metadata) => Some(metadata.len()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => panic!("{}", storage_io(error, &path, "stat")),
        }
    }

    async fn mtime(&self, scope: &str, key: &str) -> Result<Option<i64>, WireAsyncError> {
        let path = self.file_path(scope, key);
        let Some(metadata) = self.stat(scope, key)? else {
            return Ok(None);
        };
        let modified = metadata
            .modified()
            .map_err(|error| storage_io(error, &path, "stat"))?;
        let millis = match modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => duration.as_millis() as i64,
            Err(error) => -(error.duration().as_millis() as i64),
        };
        Ok(Some(millis))
    }

    async fn flush(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    async fn close(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    fn path_for(&self, scope: &str, key: &str) -> Option<String> {
        Some(self.file_path(scope, key).to_string_lossy().replace('\\', "/"))
    }
}

struct FileReadStream {
    path: PathBuf,
    file: Option<tokio::fs::File>,
    range: Option<StorageReadRange>,
    remaining: Option<u64>,
    invalid_range: bool,
    done: bool,
}

async fn atomic_write_stream_async(
    path: &Path,
    source: &mut StorageByteStream,
    mode: Option<u32>,
    signal: Option<&crate::_base::utils::abort::AbortSignal>,
) -> Result<(), WireAsyncError> {
    let temporary = unique_tmp_path(path);
    let result = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&temporary).await?;
        while let Some(chunk) = source.next().await {
            if let Some(signal) = signal {
                signal.throw_if_aborted()?;
            }
            let chunk = chunk?;
            if !chunk.is_empty() {
                use tokio::io::AsyncWriteExt;
                file.write_all(&chunk).await?;
            }
        }
        if let Some(signal) = signal {
            signal.throw_if_aborted()?;
        }
        use tokio::io::AsyncWriteExt;
        file.sync_all().await?;
        drop(file);
        replace_file(&temporary, path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(format!(".tmp-{}-{}", std::process::id(), uuid::Uuid::new_v4()));
    PathBuf::from(value)
}

async fn replace_file(from: &Path, to: &Path) -> Result<(), WireAsyncError> {
    match tokio::fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(error) if cfg!(windows) && to.exists() => {
            tokio::fs::remove_file(to).await?;
            tokio::fs::rename(from, to).await?;
            Ok(())
        }
        Err(error) => Err(Box::new(error)),
    }
}

fn collect_file_keys(dir: &Path, relative_dir: &str) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut keys = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if relative_dir.is_empty() {
            name
        } else {
            format!("{relative_dir}/{name}")
        };
        if entry.file_type()?.is_dir() {
            keys.extend(collect_file_keys(&entry.path(), &relative)?);
        } else {
            keys.push(relative);
        }
    }
    Ok(keys)
}

fn create_dir_all(path: &Path, mode: Option<u32>) -> Result<(), WireAsyncError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }
    builder.create(path)?;
    Ok(())
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

fn storage_io(error: io::Error, path: &Path, op: &str) -> WireAsyncError {
    to_storage_io_error(
        Box::new(error),
        &path.to_string_lossy().replace('\\', "/"),
        op,
    )
}
