use indexmap::IndexMap;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub const AGENT_FILE_HISTORY_SERVICE_ID: &str = "agentFileHistoryService";
pub const FILE_HISTORY_BLOB_PREFIX: &str = "file-history";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBackupEntry {
    pub key: Option<String>,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileHistoryCheckpointPhase {
    #[default]
    Start,
    End,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryCheckpointRecord {
    pub turn_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<FileHistoryCheckpointPhase>,
    pub entries: IndexMap<String, FileBackupEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryState {
    pub checkpoints: Vec<FileHistoryCheckpointRecord>,
    pub tracked: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileHistoryChangeStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryChange {
    pub path: String,
    pub status: FileHistoryChangeStatus,
    pub additions: usize,
    pub deletions: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<bool>,
}

impl From<crate::session::sqlite_store::FileHistoryChange> for FileHistoryChange {
    fn from(change: crate::session::sqlite_store::FileHistoryChange) -> Self {
        Self {
            path: change.path,
            status: match change.status.as_str() {
                "added" => FileHistoryChangeStatus::Added,
                "deleted" => FileHistoryChangeStatus::Deleted,
                _ => FileHistoryChangeStatus::Modified,
            },
            additions: change.additions,
            deletions: change.deletions,
            binary: change.binary,
            oversize: change.oversize,
            unreadable: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileHistoryContent {
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
}

impl From<crate::session::sqlite_store::FileHistoryContent> for FileHistoryContent {
    fn from(content: crate::session::sqlite_store::FileHistoryContent) -> Self {
        Self {
            version: content.version as u64,
            content: content.content,
            binary: content.binary,
        }
    }
}

#[async_trait]
pub trait IAgentFileHistoryService: Send + Sync {
    async fn history(&self) -> FileHistoryState;
    async fn settled(&self);
    async fn capture_for_active_turn(&self, path: &str) -> Result<(), String>;
    async fn changes(&self, turn_id: u64) -> Result<Vec<FileHistoryChange>, String>;
    async fn turn_recorded(&self, turn_id: u64) -> Result<bool, String>;
    async fn content_at(
        &self,
        turn_id: u64,
        path: &str,
        phase: FileHistoryCheckpointPhase,
    ) -> Result<Option<FileHistoryContent>, String>;
}
