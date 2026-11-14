use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::_base::utils::abort::{AbortError, AbortSignal};
use crate::wire::record::WireAsyncError;

const WIRE_FILENAME: &str = "wire.jsonl";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWireScan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_message_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_user_input: Option<String>,
}

pub async fn scan_session_wire(
    session_dir: &str,
    signal: Option<&AbortSignal>,
) -> Result<SessionWireScan, WireAsyncError> {
    throw_if_aborted(signal)?;
    let wire_files = collect_wire_files(session_dir, signal).await?;
    let mut combined = SessionWireScan::default();
    for file in wire_files {
        throw_if_aborted(signal)?;
        let scan = scan_wire_file(&file, signal).await?;
        combined.first_activity_ms = min_defined(combined.first_activity_ms, scan.first_activity_ms);
        combined.last_activity_ms = max_defined(combined.last_activity_ms, scan.last_activity_ms);
        combined.last_user_message_ms = max_defined(
            combined.last_user_message_ms,
            scan.last_user_message_ms,
        );
        if combined.first_user_input.is_none() {
            combined.first_user_input = scan.first_user_input;
        }
    }
    Ok(combined)
}

async fn collect_wire_files(
    session_dir: &str,
    signal: Option<&AbortSignal>,
) -> Result<Vec<String>, WireAsyncError> {
    let mut files = vec![path_string(&Path::new(session_dir).join(WIRE_FILENAME))];
    let mut pending = vec![Path::new(session_dir).join("agents")];
    while let Some(directory) = pending.pop() {
        let mut entries = match tokio::fs::read_dir(&directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Box::new(error)),
        };
        while let Some(entry) = entries.next_entry().await.map_err(|error| Box::new(error) as WireAsyncError)? {
            throw_if_aborted(signal)?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .map_err(|error| Box::new(error) as WireAsyncError)?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && entry.file_name() == WIRE_FILENAME {
                files.push(path_string(&path));
            }
        }
    }
    Ok(files)
}

async fn scan_wire_file(
    path: &str,
    signal: Option<&AbortSignal>,
) -> Result<SessionWireScan, WireAsyncError> {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(SessionWireScan::default()),
        Err(error) => return Err(Box::new(error)),
    };
    throw_if_aborted(signal)?;
    let metadata = file
        .metadata()
        .await
        .map_err(|error| Box::new(error) as WireAsyncError)?;
    throw_if_aborted(signal)?;
    let mut scan = SessionWireScan::default();
    if metadata.len() != 0 {
        let mut lines = BufReader::new(file).lines();
        while let Some(line) = lines.next_line().await.map_err(|error| Box::new(error) as WireAsyncError)? {
            throw_if_aborted(signal)?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            let Some(record) = parsed.as_object() else {
                continue;
            };
            let time_ms = record
                .get("time")
                .and_then(Value::as_f64)
                .and_then(normalize_timestamp_ms);
            if let Some(time_ms) = time_ms {
                scan.first_activity_ms = min_defined(scan.first_activity_ms, Some(time_ms));
                scan.last_activity_ms = max_defined(scan.last_activity_ms, Some(time_ms));
            }
            if record.get("type").and_then(Value::as_str) == Some("turn_begin") {
                if let Some(time_ms) = time_ms {
                    scan.last_user_message_ms = max_defined(scan.last_user_message_ms, Some(time_ms));
                }
                if scan.first_user_input.is_none()
                    && let Some(user_input) = record.get("userInput").and_then(Value::as_str)
                    && !user_input.trim().is_empty()
                {
                    scan.first_user_input = Some(user_input.to_string());
                }
            }
        }
    }
    Ok(scan)
}

pub fn normalize_timestamp_ms(value: f64) -> Option<i64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let normalized = if value > 1e12 {
        value.floor()
    } else {
        (value * 1000.0).floor()
    };
    Some(normalized as i64)
}

fn min_defined(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (left, right) => left.or(right),
    }
}

fn max_defined(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.max(right)),
        (left, right) => left.or(right),
    }
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

fn path_string(path: &PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}
