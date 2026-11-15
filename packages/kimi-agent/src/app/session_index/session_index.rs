use std::collections::HashMap;

use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::persistence::interface::query_store::Page;
use crate::session::session_metadata::session_metadata::SessionLastTurnReason;
use crate::session::sqlite_store::decode_json_column;
use crate::wire::record::WireAsyncError;

pub const PARENT_SESSION_ID_KEY: &str = "parent_session_id";
pub const CHILD_SESSION_KIND_KEY: &str = "child_session_kind";
pub const CHILD_SESSION_KIND: &str = "child";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<HashMap<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<SessionLastTurnReason>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_archived: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_of: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCountQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_archived: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionIndexState {
    #[default]
    Uninitialized,
    Preparing,
    Ready,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexStatus {
    pub state: SessionIndexState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub degraded_count: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionIndexPrepareOptions {
    pub deadline_ms: Option<u64>,
}

#[async_trait]
pub trait ISessionIndex: Send + Sync {
    async fn prepare(&self, options: Option<SessionIndexPrepareOptions>) -> SessionIndexStatus;
    fn status(&self) -> SessionIndexStatus;
    async fn get(&self, id: &str) -> Result<Option<SessionSummary>, WireAsyncError>;
    async fn list_recent(&self, query: SessionListQuery) -> Result<Page<SessionSummary>, WireAsyncError>;
    async fn count(&self, query: SessionCountQuery) -> Result<i64, WireAsyncError>;
    async fn remove(&self, id: &str) -> Result<(), WireAsyncError>;
}

#[async_trait]
pub trait ISessionIndexMirror: Send + Sync {
    fn record(&self, summary: SessionSummary) -> Result<(), WireAsyncError>;
    fn pending(&self) -> Vec<SessionSummary>;
    async fn evict(&self, id: &str) -> Result<(), WireAsyncError>;
    async fn drain(&self) -> Result<(), WireAsyncError>;
}

impl crate::session::sqlite_store::SessionSummary {
    pub fn normalized_title_kind(&self) -> Option<&'static str> {
        match self.title_kind.as_deref() {
            Some("custom") => Some("custom"),
            Some("generated") => Some("generated"),
            Some("replaceable") => Some("replaceable"),
            _ => {
                if self.title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                    Some("replaceable")
                } else {
                    None
                }
            }
        }
    }

    pub fn is_custom_title(&self) -> bool {
        self.normalized_title_kind() == Some("custom")
    }
}

pub(crate) fn session_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<crate::session::sqlite_store::SessionSummary> {
    let session_id: String = row.get(0)?;
    let mut summary = crate::session::sqlite_store::SessionSummary {
        session_id: session_id.clone(),
        title: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        workspace_id: row.get(4)?,
        archived: row.get::<_, i64>(5)? != 0,
        parent_session_id: row.get(6)?,
        title_kind: row.get(7)?,
        last_prompt: row.get(8)?,
        archived_at: row.get(9)?,
        forked_from: row.get(10)?,
        version: row.get::<_, Option<i64>>(11)?.map(|v| v.max(0) as u32),
        cwd: row.get(12)?,
        environment_id: row.get(13)?,
        environment_cwd: row.get(14)?,
        agents: decode_json_column(row.get(15)?, format!("session {session_id} agents"))?,
        custom: decode_json_column(row.get(16)?, format!("session {session_id} custom"))?,
        last_turn_reason: row.get(17)?,
    };

    if summary.title_kind.is_none() {
        summary.title_kind = summary.normalized_title_kind().map(str::to_string);
    }
    Ok(summary)
}


pub static I_SESSION_INDEX: LazyLock<ServiceIdentifier<Arc<dyn ISessionIndex>>> =
    LazyLock::new(|| create_decorator("sessionIndex"));

pub static I_SESSION_INDEX_MIRROR: LazyLock<ServiceIdentifier<Arc<dyn ISessionIndexMirror>>> =
    LazyLock::new(|| create_decorator("sessionIndexMirror"));
