use std::collections::HashSet;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use kimi_core::primitives::errors::{
    Error2, Error2Options, ErrorDetails, domains::storage as storage_errors,
};
use parking_lot::Mutex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::_base::utils::fs::{atomic_write, atomic_write_stream, sync_dir};

const TORN_READ_RETRIES: usize = 3;
const TORN_READ_RETRY_DELAY: Duration = Duration::from_millis(15);

pub type StorageResult<T> = Result<T, Error2>;

#[derive(Clone, Default)]
pub struct StorageWriteOptions {
    pub atomic: Option<bool>,
    pub signal: Option<CancellationToken>,
}

#[derive(Clone, Copy)]
pub struct StorageAppendOptions {
    pub durable: bool,
}

impl Default for StorageAppendOptions {
    fn default() -> Self {
        Self { durable: true }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageReadRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageListOptions {
    pub recursive: bool,
}

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

    pub fn read(&self, scope: &str, key: &str) -> StorageResult<Option<Vec<u8>>> {
        let file_path = self.path_for(scope, key);
        for attempt in 0..=TORN_READ_RETRIES {
            let bytes = match fs::read(&file_path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(to_storage_io_error(error, &file_path, "read")),
            };
            if attempt >= TORN_READ_RETRIES {
                return Ok(Some(bytes));
            }
            match fs::metadata(&file_path) {
                Ok(metadata) if metadata.len() != bytes.len() as u64 => {
                    std::thread::sleep(TORN_READ_RETRY_DELAY);
                }
                _ => return Ok(Some(bytes)),
            }
        }
        unreachable!()
    }

    pub fn read_stream(
        &self,
        scope: &str,
        key: &str,
        range: Option<StorageReadRange>,
    ) -> StorageResult<Vec<u8>> {
        let file_path = self.path_for(scope, key);
        let mut file = match File::open(&file_path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(to_storage_io_error(error, &file_path, "read")),
        };
        let result = (|| {
            let mut bytes = Vec::new();
            if let Some(range) = range {
                let length = range
                    .end
                    .checked_sub(range.start)
                    .and_then(|difference| difference.checked_add(1))
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "invalid read range")
                    })?;
                file.seek(SeekFrom::Start(range.start))?;
                file.take(length).read_to_end(&mut bytes)?;
            } else {
                file.read_to_end(&mut bytes)?;
            }
            Ok(bytes)
        })();
        result.map_err(|error| to_storage_io_error(error, &file_path, "read"))
    }

    pub fn write(&self, scope: &str, key: &str, data: &[u8]) -> StorageResult<()> {
        self.write_with_options(scope, key, data, StorageWriteOptions::default())
    }

    pub fn write_with_options(
        &self,
        scope: &str,
        key: &str,
        data: &[u8],
        options: StorageWriteOptions,
    ) -> StorageResult<()> {
        let file_path = self.path_for(scope, key);
        let result = (|| {
            create_dir_all(parent_dir(&file_path), self.dir_mode)?;
            atomic_write(
                &file_path,
                data,
                None,
                self.file_mode,
                options.signal.as_ref(),
            )?;
            self.sync_dir_once(parent_dir(&file_path))
        })();
        result.map_err(|error| to_storage_io_error(error, &file_path, "write"))
    }

    pub fn write_stream<B>(
        &self,
        scope: &str,
        key: &str,
        source: impl IntoIterator<Item = io::Result<B>>,
    ) -> StorageResult<()>
    where
        B: AsRef<[u8]>,
    {
        self.write_stream_with_options(scope, key, source, StorageWriteOptions::default())
    }

    pub fn write_stream_with_options<B>(
        &self,
        scope: &str,
        key: &str,
        source: impl IntoIterator<Item = io::Result<B>>,
        options: StorageWriteOptions,
    ) -> StorageResult<()>
    where
        B: AsRef<[u8]>,
    {
        let file_path = self.path_for(scope, key);
        let result = (|| {
            create_dir_all(parent_dir(&file_path), self.dir_mode)?;
            atomic_write_stream(&file_path, source, self.file_mode, options.signal.as_ref())?;
            self.sync_dir_once(parent_dir(&file_path))
        })();
        result.map_err(|error| to_storage_io_error(error, &file_path, "write"))
    }

    pub fn append(&self, scope: &str, key: &str, data: &[u8]) -> StorageResult<()> {
        self.append_with_options(scope, key, data, StorageAppendOptions::default())
    }

    pub fn append_with_options(
        &self,
        scope: &str,
        key: &str,
        data: &[u8],
        options: StorageAppendOptions,
    ) -> StorageResult<()> {
        let file_path = self.path_for(scope, key);
        let dir = parent_dir(&file_path);
        let result = (|| {
            create_dir_all(dir, self.dir_mode)?;
            let mut open_options = OpenOptions::new();
            open_options.append(true).create(true);
            #[cfg(unix)]
            if let Some(mode) = self.file_mode {
                use std::os::unix::fs::OpenOptionsExt;
                open_options.mode(mode);
            }
            let mut file = open_options.open(&file_path)?;
            if !data.is_empty() {
                file.write_all(data)?;
            }
            if options.durable {
                file.sync_all()?;
            }
            drop(file);
            self.sync_dir_once(dir)
        })();
        result.map_err(|error| to_storage_io_error(error, &file_path, "append"))
    }

    pub fn list(&self, scope: &str, prefix: Option<&str>) -> StorageResult<Vec<String>> {
        self.list_with_options(scope, prefix, StorageListOptions::default())
    }

    pub fn list_with_options(
        &self,
        scope: &str,
        prefix: Option<&str>,
        options: StorageListOptions,
    ) -> StorageResult<Vec<String>> {
        let scope_path = self.scope_path(scope);
        if options.recursive {
            let keys = collect_file_keys(&scope_path, "")
                .map_err(|error| to_storage_io_error(error, &scope_path, "list"))?;
            return Ok(filter_prefix(keys, prefix));
        }
        let entries = match fs::read_dir(&scope_path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(to_storage_io_error(error, &scope_path, "list")),
        };
        let mut keys = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => keys.push(entry.file_name().to_string_lossy().into_owned()),
                Err(error) => return Err(to_storage_io_error(error, &scope_path, "list")),
            }
        }
        Ok(filter_prefix(keys, prefix))
    }

    pub fn delete(&self, scope: &str, key: &str) -> StorageResult<()> {
        let file_path = self.path_for(scope, key);
        match fs::remove_file(&file_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(to_storage_io_error(error, &file_path, "delete")),
        }
    }

    pub fn size(&self, scope: &str, key: &str) -> StorageResult<Option<u64>> {
        self.size_sync(scope, key)
    }

    pub fn size_sync(&self, scope: &str, key: &str) -> StorageResult<Option<u64>> {
        Ok(self.stat(scope, key)?.map(|metadata| metadata.len()))
    }

    pub fn mtime(&self, scope: &str, key: &str) -> StorageResult<Option<f64>> {
        let file_path = self.path_for(scope, key);
        let Some(metadata) = self.stat(scope, key)? else {
            return Ok(None);
        };
        let modified = metadata
            .modified()
            .map_err(|error| to_storage_io_error(error, &file_path, "stat"))?;
        let duration = modified
            .duration_since(UNIX_EPOCH)
            .map_err(|error| to_storage_io_error(io::Error::other(error), &file_path, "stat"))?;
        Ok(Some(duration.as_secs_f64() * 1000.0))
    }

    pub fn flush(&self) {}

    pub fn close(&self) {}

    pub fn path_for(&self, scope: &str, key: &str) -> PathBuf {
        self.base_dir.join(scope).join(key)
    }

    fn scope_path(&self, scope: &str) -> PathBuf {
        self.base_dir.join(scope)
    }

    fn stat(&self, scope: &str, key: &str) -> StorageResult<Option<Metadata>> {
        let file_path = self.path_for(scope, key);
        match fs::metadata(&file_path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(to_storage_io_error(error, &file_path, "stat")),
        }
    }

    fn sync_dir_once(&self, dir: &Path) -> io::Result<()> {
        if self.synced_dirs.lock().contains(dir) {
            return Ok(());
        }
        match sync_dir(dir) {
            Ok(()) => {
                self.synced_dirs.lock().insert(dir.to_path_buf());
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
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

fn filter_prefix(keys: Vec<String>, prefix: Option<&str>) -> Vec<String> {
    match prefix {
        Some(prefix) => keys
            .into_iter()
            .filter(|key| key.starts_with(prefix))
            .collect(),
        None => keys,
    }
}

fn create_dir_all(path: &Path, mode: Option<u32>) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }
    builder.create(path)
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

pub fn to_storage_io_error(error: io::Error, path: &Path, op: &str) -> Error2 {
    let errno = errno_name(&error);
    let code = match error.kind() {
        io::ErrorKind::NotFound => storage_errors::STORAGE_NOT_FOUND,
        io::ErrorKind::PermissionDenied => storage_errors::STORAGE_PERMISSION_DENIED,
        io::ErrorKind::StorageFull => storage_errors::STORAGE_DISK_FULL,
        _ if errno == Some("ENOSPC") => storage_errors::STORAGE_DISK_FULL,
        _ => storage_errors::STORAGE_IO_FAILED,
    };
    let reason = if error.kind() == io::ErrorKind::Interrupted {
        "operation cancelled"
    } else {
        match code {
            storage_errors::STORAGE_NOT_FOUND => "path does not exist",
            storage_errors::STORAGE_PERMISSION_DENIED => "permission denied",
            storage_errors::STORAGE_DISK_FULL => "no space left on device",
            _ => "unrecognized I/O error",
        }
    };
    let mut details = ErrorDetails::new();
    details.insert(
        "path".to_string(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    details.insert("op".to_string(), Value::String(op.to_string()));
    details.insert(
        "errno".to_string(),
        errno.map_or(Value::Null, |errno| Value::String(errno.to_string())),
    );
    Error2::with_options(
        code,
        format!("storage {op} failed: {reason}"),
        Error2Options {
            details: Some(details),
            cause: Some(Box::new(error)),
            name: Some("StorageError".to_string()),
        },
    )
    .with_retryable(code == storage_errors::STORAGE_IO_FAILED)
}

fn errno_name(error: &io::Error) -> Option<&'static str> {
    if error.kind() == io::ErrorKind::NotFound {
        return Some("ENOENT");
    }
    if error.kind() == io::ErrorKind::PermissionDenied {
        #[cfg(unix)]
        if error.raw_os_error() == Some(1) {
            return Some("EPERM");
        }
        return Some("EACCES");
    }
    if error.kind() == io::ErrorKind::StorageFull {
        return Some("ENOSPC");
    }
    #[cfg(unix)]
    match error.raw_os_error() {
        Some(1) => Some("EPERM"),
        Some(2) => Some("ENOENT"),
        Some(13) => Some("EACCES"),
        Some(17) => Some("EEXIST"),
        Some(20) => Some("ENOTDIR"),
        Some(21) => Some("EISDIR"),
        Some(28) => Some("ENOSPC"),
        _ => None,
    }
    #[cfg(windows)]
    match error.raw_os_error() {
        Some(2) => Some("ENOENT"),
        Some(5) => Some("EACCES"),
        Some(80 | 183) => Some("EEXIST"),
        Some(112) => Some("ENOSPC"),
        Some(267) => Some("ENOTDIR"),
        _ => None,
    }
    #[cfg(not(any(unix, windows)))]
    None
}

#[cfg(test)]
mod tests {
    use std::io;

    use tokio_util::sync::CancellationToken;

    use super::{
        FileStorageService, StorageAppendOptions, StorageListOptions, StorageReadRange,
        StorageWriteOptions,
    };

    #[test]
    fn missing_files_have_absent_read_and_metadata_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);

        assert_eq!(storage.read("scope", "missing").unwrap(), None);
        assert!(
            storage
                .read_stream("scope", "missing", None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(storage.size("scope", "missing").unwrap(), None);
        assert_eq!(storage.size_sync("scope", "missing").unwrap(), None);
        assert_eq!(storage.mtime("scope", "missing").unwrap(), None);
        assert!(storage.list("missing", None).unwrap().is_empty());
        storage.delete("scope", "missing").unwrap();
    }

    #[test]
    fn write_read_append_range_metadata_and_delete_cover_the_main_path() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);

        storage.write("scope", "key.bin", b"hello").unwrap();
        storage.append("scope", "key.bin", b" world").unwrap();
        storage
            .append_with_options(
                "scope",
                "key.bin",
                b"!",
                StorageAppendOptions { durable: false },
            )
            .unwrap();

        assert_eq!(
            storage.read("scope", "key.bin").unwrap(),
            Some(b"hello world!".to_vec())
        );
        assert_eq!(
            storage
                .read_stream(
                    "scope",
                    "key.bin",
                    Some(StorageReadRange { start: 1, end: 3 }),
                )
                .unwrap(),
            b"ell"
        );
        assert_eq!(storage.size("scope", "key.bin").unwrap(), Some(12));
        assert_eq!(storage.size_sync("scope", "key.bin").unwrap(), Some(12));
        assert!(storage.mtime("scope", "key.bin").unwrap().is_some());
        assert_eq!(
            storage.path_for("scope", "key.bin"),
            dir.path().join("scope").join("key.bin")
        );

        storage.flush();
        storage.flush();
        storage.close();
        storage.close();
        storage.delete("scope", "key.bin").unwrap();
        assert_eq!(storage.read("scope", "key.bin").unwrap(), None);
    }

    #[test]
    fn list_supports_flat_prefixes_and_recursive_scope_relative_keys() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        storage.write("scope", "top.bin", b"t").unwrap();
        storage.write("scope", "history/a@v1", b"a").unwrap();
        storage.write("scope", "history/nested/b@v2", b"b").unwrap();
        storage.write("scope", "other/c.bin", b"c").unwrap();

        let mut flat = storage.list("scope", None).unwrap();
        flat.sort();
        assert_eq!(flat, vec!["history", "other", "top.bin"]);
        assert_eq!(
            storage.list("scope", Some("hist")).unwrap(),
            vec!["history"]
        );

        let mut recursive = storage
            .list_with_options(
                "scope",
                Some("history"),
                StorageListOptions { recursive: true },
            )
            .unwrap();
        recursive.sort();
        assert_eq!(recursive, vec!["history/a@v1", "history/nested/b@v2"]);
        assert!(
            storage
                .list_with_options(
                    "scope",
                    Some("missing"),
                    StorageListOptions { recursive: true },
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn write_stream_replaces_the_value_and_cleans_up_source_failures() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        storage.write("scope", "key.bin", b"old").unwrap();
        let chunks = vec![
            Ok::<Vec<u8>, io::Error>(b"aa".to_vec()),
            Ok(b"bbb".to_vec()),
        ];

        storage.write_stream("scope", "key.bin", chunks).unwrap();
        assert_eq!(
            storage.read("scope", "key.bin").unwrap(),
            Some(b"aabbb".to_vec())
        );

        let failing = vec![
            Ok::<Vec<u8>, io::Error>(b"partial".to_vec()),
            Err(io::Error::other("source failed")),
        ];
        let error = storage
            .write_stream("scope", "key.bin", failing)
            .unwrap_err();
        assert_eq!(error.code().as_str(), "storage.io_failed");
        assert!(error.cause().is_some());
        assert_eq!(
            storage.read("scope", "key.bin").unwrap(),
            Some(b"aabbb".to_vec())
        );
        assert!(
            std::fs::read_dir(dir.path().join("scope"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp"))
        );
    }

    #[test]
    fn write_with_options_propagates_cancellation_without_a_target() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);
        let token = CancellationToken::new();
        token.cancel();

        let error = storage
            .write_with_options(
                "scope",
                "key.bin",
                b"value",
                StorageWriteOptions {
                    atomic: Some(true),
                    signal: Some(token),
                },
            )
            .unwrap_err();

        assert_eq!(error.message(), "storage write failed: operation cancelled");
        assert_eq!(storage.read("scope", "key.bin").unwrap(), None);
    }

    #[test]
    fn io_errors_keep_the_coded_message_details_and_cause() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blocked"), b"x").unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);

        let error = storage.write("blocked", "key", b"value").unwrap_err();

        assert_eq!(error.code().as_str(), "storage.io_failed");
        assert_eq!(
            error.message(),
            "storage write failed: unrecognized I/O error"
        );
        let details = error.details().unwrap();
        assert_eq!(details.get("op").unwrap(), "write");
        assert!(details.get("errno").unwrap().is_string());
        assert_eq!(
            details.get("path").unwrap().as_str().unwrap(),
            storage.path_for("blocked", "key").to_string_lossy()
        );
        assert!(error.cause().is_some());
    }

    #[test]
    fn sync_dir_once_ignores_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), None, None);

        storage.sync_dir_once(&dir.path().join("missing")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_applies_directory_and_file_modes() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let storage = FileStorageService::new(dir.path(), Some(0o700), Some(0o600));
        storage.write("scope/nested", "key", b"value").unwrap();

        let directory_mode = std::fs::metadata(dir.path().join("scope/nested"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(storage.path_for("scope/nested", "key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
