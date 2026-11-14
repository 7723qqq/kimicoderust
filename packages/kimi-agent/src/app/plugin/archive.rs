use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use kimi_core::primitives::errors::Error2;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::wire::record::WireAsyncError;

use super::errors::PLUGIN_LOAD_FAILED;

const MAX_ZIP_DOWNLOAD_BYTES: usize = 256 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_ENTRY_BYTES: usize = 64 * 1024 * 1024;
const MAX_TOTAL_EXTRACTED_BYTES: usize = 256 * 1024 * 1024;

pub async fn download_zip(url: &str, signal: Option<CancellationToken>) -> Result<Vec<u8>, WireAsyncError> {
    let download = download_zip_inner(url);
    match signal {
        Some(signal) => tokio::select! {
            result = download => result,
            _ = signal.cancelled() => Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Zip download aborted",
            ))),
        },
        None => match tokio::time::timeout(Duration::from_secs(5 * 60), download).await {
            Ok(result) => result,
            Err(error) => Err(Box::new(error)),
        },
    }
}

async fn download_zip_inner(url: &str) -> Result<Vec<u8>, WireAsyncError> {
    let response = reqwest::Client::new().get(url).send().await?;
    let status = response.status();
    if !status.is_success() {
        let mut details = Map::new();
        details.insert("url".to_string(), Value::String(url.to_string()));
        details.insert("status".to_string(), Value::from(status.as_u16()));
        return Err(Box::new(
            Error2::new(
                PLUGIN_LOAD_FAILED,
                format!(
                    "Failed to download zip: HTTP {} {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or_default()
                ),
            )
            .with_details(details),
        ));
    }
    let content_length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if content_length.is_some_and(|length| length > MAX_ZIP_DOWNLOAD_BYTES) {
        let length = content_length.unwrap();
        let mut details = Map::new();
        details.insert("url".to_string(), Value::String(url.to_string()));
        details.insert("bytes".to_string(), Value::from(length));
        return Err(Box::new(
            Error2::new(
                PLUGIN_LOAD_FAILED,
                format!(
                    "Zip download too large: {length} bytes exceeds the {MAX_ZIP_DOWNLOAD_BYTES} byte limit"
                ),
            )
            .with_details(details),
        ));
    }
    let mut stream = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if output.len().saturating_add(chunk.len()) > MAX_ZIP_DOWNLOAD_BYTES {
            let total = output.len() + chunk.len();
            let mut details = Map::new();
            details.insert("url".to_string(), Value::String(url.to_string()));
            details.insert("bytes".to_string(), Value::from(total));
            return Err(Box::new(
                Error2::new(
                    PLUGIN_LOAD_FAILED,
                    format!(
                        "Zip download too large: exceeded the {MAX_ZIP_DOWNLOAD_BYTES} byte limit"
                    ),
                )
                .with_details(details),
            ));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

pub async fn extract_zip(buffer: Vec<u8>, dest_dir: &str) -> Result<String, WireAsyncError> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let destination = PathBuf::from(dest_dir);
    let task_destination = destination.clone();
    let result = tokio::task::spawn_blocking(move || extract_archive(buffer, &task_destination)).await;
    let result = match result {
        Ok(result) => result,
        Err(error) => Err(Box::new(error) as WireAsyncError),
    };
    if let Err(error) = result {
        let _ = tokio::fs::remove_dir_all(&destination).await;
        return Err(error);
    }
    Ok(path_string(&detect_plugin_root(&destination).await?))
}

fn extract_archive(buffer: Vec<u8>, dest_dir: &Path) -> Result<(), WireAsyncError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(buffer)).map_err(|error| {
        archive_error(
            format!("Failed to open zip: {error}"),
            None,
            Some(Box::new(error)),
        )
    })?;
    let dest_dir_resolved = absolute_path(dest_dir);
    let mut total_bytes = 0usize;
    for index in 0..archive.len() {
        let entry_count = index + 1;
        if entry_count > MAX_ARCHIVE_ENTRIES {
            let mut details = Map::new();
            details.insert("entries".to_string(), Value::from(entry_count));
            return Err(Box::new(
                Error2::new(
                    PLUGIN_LOAD_FAILED,
                    format!("Zip contains too many entries (limit {MAX_ARCHIVE_ENTRIES})"),
                )
                .with_details(details),
            ));
        }
        let mut entry = archive.by_index(index).map_err(|error| {
            archive_error(
                format!("Failed to open zip: {error}"),
                None,
                Some(Box::new(error)),
            )
        })?;
        let file_name = entry.name().to_string();
        let dest_path = absolute_path(&dest_dir_resolved.join(&file_name));
        if dest_path != dest_dir_resolved && !dest_path.starts_with(&dest_dir_resolved) {
            let mut details = Map::new();
            details.insert("entry".to_string(), Value::String(file_name.clone()));
            return Err(Box::new(
                Error2::new(
                    PLUGIN_LOAD_FAILED,
                    format!("Path traversal detected in zip entry: {file_name}"),
                )
                .with_details(details),
            ));
        }
        if file_name.ends_with('/') {
            std::fs::create_dir_all(&dest_path)?;
            continue;
        }
        let unix_mode = entry.unix_mode();
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = std::fs::File::create(&dest_path)?;
        let mut entry_bytes = 0usize;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let read = entry.read(&mut chunk).map_err(|error| {
                let mut details = Map::new();
                details.insert("entry".to_string(), Value::String(file_name.clone()));
                archive_error(
                    format!("Failed to read {file_name} from archive: {error}"),
                    Some(details),
                    Some(Box::new(error)),
                )
            })?;
            if read == 0 {
                break;
            }
            entry_bytes += read;
            total_bytes += read;
            if entry_bytes > MAX_ENTRY_BYTES {
                let mut details = Map::new();
                details.insert("entry".to_string(), Value::String(file_name.clone()));
                details.insert("bytes".to_string(), Value::from(entry_bytes));
                return Err(Box::new(
                    Error2::new(
                        PLUGIN_LOAD_FAILED,
                        format!(
                            "Zip entry \"{file_name}\" exceeds the {MAX_ENTRY_BYTES} byte single-file limit"
                        ),
                    )
                    .with_details(details),
                ));
            }
            if total_bytes > MAX_TOTAL_EXTRACTED_BYTES {
                let mut details = Map::new();
                details.insert("bytes".to_string(), Value::from(total_bytes));
                return Err(Box::new(
                    Error2::new(
                        PLUGIN_LOAD_FAILED,
                        format!(
                            "Zip extraction exceeds the {MAX_TOTAL_EXTRACTED_BYTES} byte total limit"
                        ),
                    )
                    .with_details(details),
                ));
            }
            output.write_all(&chunk[..read])?;
        }
        output.flush()?;
        drop(output);
        restore_file_permissions(&dest_path, unix_mode)?;
    }
    Ok(())
}

#[cfg(unix)]
fn restore_file_permissions(dest_path: &Path, mode: Option<u32>) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else {
        return Ok(());
    };
    let permissions = mode & 0o777;
    if permissions == 0 {
        return Ok(());
    }
    std::fs::set_permissions(dest_path, std::fs::Permissions::from_mode(permissions))
}

#[cfg(not(unix))]
fn restore_file_permissions(_dest_path: &Path, _mode: Option<u32>) -> std::io::Result<()> {
    Ok(())
}

async fn detect_plugin_root(dir: &Path) -> Result<PathBuf, WireAsyncError> {
    if has_manifest(dir).await {
        return Ok(dir.to_path_buf());
    }
    let mut entries = tokio::fs::read_dir(dir).await?;
    let mut child_dirs = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_type().await?.is_dir() {
            child_dirs.push(entry.path());
        }
    }
    if child_dirs.len() == 1 && has_manifest(&child_dirs[0]).await {
        return Ok(child_dirs.remove(0));
    }
    Ok(dir.to_path_buf())
}

async fn has_manifest(dir: &Path) -> bool {
    is_file(&dir.join("kimi.plugin.json")).await
        || is_file(&dir.join(".kimi-plugin").join("plugin.json")).await
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

fn absolute_path(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .join(path)
    };
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

fn archive_error(
    message: String,
    details: Option<Map<String, Value>>,
    cause: Option<WireAsyncError>,
) -> WireAsyncError {
    let error = Error2::new(PLUGIN_LOAD_FAILED, message);
    let error = match details {
        Some(details) => error.with_details(details),
        None => error,
    };
    let error = match cause {
        Some(cause) => error.with_cause(cause),
        None => error,
    };
    Box::new(error)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
