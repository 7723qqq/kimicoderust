use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use kimi_core::primitives::errors::domains::fs::FS_IS_DIRECTORY;
use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::utils::abort::{AbortError, AbortSignal};
use crate::_base::utils::paths::canonical_path;
use crate::wire::record::WireAsyncError;

use super::errors::SESSION_EXPORT_TOO_LARGE;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug)]
pub struct ZipSourceIdentity {
    pub device: u64,
    pub inode: u64,
    pub(crate) handle: Option<Arc<same_file::Handle>>,
}

impl ZipSourceIdentity {
    pub(crate) fn from_path(path: &str) -> io::Result<Self> {
        let file = File::open(Path::new(path))?;
        let metadata = file.metadata()?;
        Self::from_file(&file, &metadata)
    }

    fn from_file(file: &File, metadata: &Metadata) -> io::Result<Self> {
        let handle = same_file::Handle::from_file(file.try_clone()?)?;
        Ok(Self {
            device: metadata_device(metadata),
            inode: metadata_inode(metadata),
            handle: Some(Arc::new(handle)),
        })
    }
}

struct ZipSourceInner {
    file: Mutex<Option<File>>,
}

#[derive(Clone)]
pub struct ZipSource {
    inner: Arc<ZipSourceInner>,
    pub size: u64,
    pub mtime: SystemTime,
    pub mode: u32,
    pub identity: ZipSourceIdentity,
    pub source_path: Option<String>,
}

impl ZipSource {
    pub fn stream(&self, signal: Option<AbortSignal>) -> ZipSourceStream {
        ZipSourceStream {
            source: self.clone(),
            signal,
        }
    }

    pub async fn close(&self) -> Result<(), WireAsyncError> {
        self.close_sync()
    }

    pub(crate) fn close_sync(&self) -> Result<(), WireAsyncError> {
        self.inner.file.lock().take();
        Ok(())
    }
}

pub struct ZipSourceStream {
    source: ZipSource,
    signal: Option<AbortSignal>,
}

impl Read for ZipSourceStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(signal) = &self.signal
            && signal.aborted()
        {
            let message = signal
                .reason()
                .map(|reason| reason.to_string())
                .unwrap_or_else(|| "The operation was aborted.".to_string());
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                AbortError::new(message),
            ));
        }
        let mut file = self.source.inner.file.lock();
        match file.as_mut() {
            Some(file) => file.read(buffer),
            None => Ok(0),
        }
    }
}

impl Drop for ZipSourceStream {
    fn drop(&mut self) {
        let _ = self.source.close_sync();
    }
}

pub async fn open_zip_source(
    source: &str,
    signal: Option<&AbortSignal>,
) -> Result<ZipSource, WireAsyncError> {
    let source = source.to_string();
    let signal = signal.cloned();
    tokio::task::spawn_blocking(move || open_zip_source_sync(&source, signal.as_ref()))
        .await
        .map_err(|error| Box::new(error) as WireAsyncError)?
}

pub(crate) fn open_zip_source_sync(
    source: &str,
    signal: Option<&AbortSignal>,
) -> Result<ZipSource, WireAsyncError> {
    throw_if_aborted(signal)?;
    let file = File::open(Path::new(source)).map_err(|error| Box::new(error) as WireAsyncError)?;
    let metadata = file
        .metadata()
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    if !metadata.is_file() {
        return Err(Box::new(path_error(
            FS_IS_DIRECTORY,
            format!("not a file: {source}"),
            source,
        )));
    }
    let size = metadata.len();
    if size > MAX_SAFE_INTEGER {
        return Err(Box::new(path_error(
            SESSION_EXPORT_TOO_LARGE,
            format!("file is too large to export: {source}"),
            source,
        )));
    }
    throw_if_aborted(signal)?;
    let identity = ZipSourceIdentity::from_file(&file, &metadata)
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    let mtime = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(ZipSource {
        inner: Arc::new(ZipSourceInner {
            file: Mutex::new(Some(file)),
        }),
        size,
        mtime,
        mode: metadata_mode(&metadata),
        identity,
        source_path: Some(canonical_path(source)),
    })
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal
        && signal.aborted()
    {
        let message = signal
            .reason()
            .map(|reason| reason.to_string())
            .unwrap_or_else(|| "The operation was aborted.".to_string());
        return Err(Box::new(AbortError::new(message)));
    }
    Ok(())
}

fn path_error(code: &str, message: String, path: &str) -> Error2 {
    let mut details = Map::new();
    details.insert("path".to_string(), Value::String(path.to_string()));
    Error2::with_options(
        code,
        message,
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    )
}

#[cfg(unix)]
fn metadata_device(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(not(unix))]
fn metadata_device(_metadata: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn metadata_inode(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn metadata_inode(_metadata: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
fn metadata_mode(metadata: &Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode()
}

#[cfg(not(unix))]
fn metadata_mode(metadata: &Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o100444
    } else {
        0o100666
    }
}
