use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

pub const INITIALIZE_METHOD: &str = "initialize";
pub const PING_METHOD: &str = "ping";
pub const FS_READ_FILE_METHOD: &str = "fs/readFile";
pub const FS_WRITE_FILE_METHOD: &str = "fs/writeFile";
pub const FS_CREATE_DIRECTORY_METHOD: &str = "fs/createDirectory";
pub const FS_GET_METADATA_METHOD: &str = "fs/getMetadata";
pub const FS_CANONICALIZE_METHOD: &str = "fs/canonicalize";
pub const FS_READ_DIRECTORY_METHOD: &str = "fs/readDirectory";
pub const FS_WALK_METHOD: &str = "fs/walk";
pub const FS_REMOVE_METHOD: &str = "fs/remove";
pub const FS_RENAME_METHOD: &str = "fs/rename";
pub const PROCESS_START_METHOD: &str = "process/start";
pub const PROCESS_OUTPUT_METHOD: &str = "process/output";
pub const PROCESS_EXITED_METHOD: &str = "process/exited";
pub const PROCESS_CLOSED_METHOD: &str = "process/closed";
pub const PROCESS_WRITE_METHOD: &str = "process/write";
pub const PROCESS_SIGNAL_METHOD: &str = "process/signal";
pub const PROCESS_FLOW_METHOD: &str = "process/flow";

pub static SERVER_NOTIFICATION_METHODS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    HashSet::from([PROCESS_OUTPUT_METHOD, PROCESS_EXITED_METHOD, PROCESS_CLOSED_METHOD])
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_name: String,
    pub client_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEnvironmentInfo {
    pub os_kind: String,
    pub os_arch: String,
    pub os_version: String,
    pub shell_name: String,
    pub shell_path: String,
    pub path_class: String,
    pub home_dir: String,
    pub cwd: String,
    pub temp_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_process_lifecycle: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    pub executor_version: String,
    pub environment: RemoteEnvironmentInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_metadata: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataResult {
    pub is_directory: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadFileResult {
    pub data_base64: String,
    pub eof: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<FsGetMetadataResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsWriteMode {
    Truncate,
    Append,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWriteFileParams {
    pub path: String,
    pub data_base64: String,
    pub mode: FsWriteMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCreateDirectoryParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recursive: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsGetMetadataParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_symlinks: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCanonicalizeParams {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCanonicalizeResult {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryParams {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryEntry {
    pub file_name: String,
    pub is_directory: bool,
    pub is_file: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_symlink: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsReadDirectoryResult {
    pub entries: Vec<FsReadDirectoryEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWalkParams {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsWalkKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWalkEntry {
    pub path: String,
    pub kind: FsWalkKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsWalkResult {
    pub entries: Vec<FsWalkEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRemoveParams {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsRenameParams {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStartParams {
    pub process_id: String,
    pub argv: Vec<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStartResult {
    pub pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessOutputNotification {
    pub process_id: String,
    pub stream: ProcessOutputStream,
    pub chunk_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessExitedNotification {
    pub process_id: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessClosedNotification {
    pub process_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessWriteParams {
    pub process_id: String,
    pub chunk_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eof: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessSignalKind {
    Interrupt,
    Terminate,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSignalParams {
    pub process_id: String,
    pub signal: ProcessSignalKind,
}

pub const MIN_EXECUTOR_VERSION: &str = "2.5.0";

pub fn compare_versions(a: &str, b: &str) -> i32 {
    let pa = a.split('.').map(parse_int_prefix).collect::<Vec<_>>();
    let pb = b.split('.').map(parse_int_prefix).collect::<Vec<_>>();
    for index in 0..pa.len().max(pb.len()) {
        let diff = pa.get(index).copied().unwrap_or(0.0) - pb.get(index).copied().unwrap_or(0.0);
        if diff != 0.0 {
            return if diff < 0.0 { -1 } else { 1 };
        }
    }
    0
}

fn parse_int_prefix(value: &str) -> f64 {
    let value = value.trim_start();
    let mut end = 0;
    for (index, ch) in value.char_indices() {
        if index == 0 && matches!(ch, '+' | '-') {
            end = 1;
            continue;
        }
        if ch.is_ascii_digit() {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 || (end == 1 && value.as_bytes().get(1).is_none_or(|byte| !byte.is_ascii_digit())) {
        return 0.0;
    }
    value[..end].parse().unwrap_or(0.0)
}

pub const FS_READ_FILE_MAX_BYTES: u64 = 1024 * 1024;
pub const FS_READ_FILE_WHOLE_MAX_BYTES: u64 = 32 * 1024 * 1024;
pub const FS_WRITE_FILE_CHUNK_BYTES: u64 = 1024 * 1024;
pub const FS_READ_DIRECTORY_MAX_ENTRIES: usize = 50_000;
pub const FS_WALK_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const FS_WALK_ENTRY_OVERHEAD_BYTES: usize = 48;
pub const FS_WALK_DEFAULT_TIMEOUT_MS: u64 = 55_000;
pub const FS_WALK_MAX_TIMEOUT_MS: u64 = 60_000;
pub const MAX_IN_FLIGHT_CALLS: usize = 256;
pub const MAX_PENDING_SEND_BYTES: usize = 64 * 1024 * 1024;
