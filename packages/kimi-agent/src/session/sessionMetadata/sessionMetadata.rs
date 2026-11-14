use std::collections::HashMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homedir: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<AgentType>,
    #[serde(rename = "parentAgentId", default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<Option<String>>,
    #[serde(rename = "forkedFrom", default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<HashMap<String, String>>,
    #[serde(rename = "swarmItem", default, skip_serializing_if = "Option::is_none")]
    pub swarm_item: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Main,
    Sub,
    Independent,
}

pub const SESSION_META_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionTitleKind {
    Replaceable,
    Generated,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "titleKind", default, skip_serializing_if = "Option::is_none")]
    pub title_kind: Option<SessionTitleKind>,
    #[serde(rename = "lastPrompt", default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "updatedAt")]
    pub updated_at: i64,
    pub archived: bool,
    #[serde(rename = "archivedAt", default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(rename = "environmentId", default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(rename = "environmentCwd", default, skip_serializing_if = "Option::is_none")]
    pub environment_cwd: Option<String>,
    #[serde(rename = "forkedFrom", default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<HashMap<String, AgentMeta>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<HashMap<String, Value>>,
    #[serde(rename = "lastTurnReason", default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<SessionLastTurnReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionLastTurnReason {
    Completed,
    Cancelled,
    Failed,
}

impl Default for SessionMeta {
    fn default() -> Self {
        Self {
            id: String::new(),
            version: None,
            title: None,
            title_kind: None,
            last_prompt: None,
            created_at: 0,
            updated_at: 0,
            archived: false,
            archived_at: None,
            cwd: None,
            environment_id: None,
            environment_cwd: None,
            forked_from: None,
            agents: None,
            custom: None,
            last_turn_reason: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "titleKind", default, skip_serializing_if = "Option::is_none")]
    pub title_kind: Option<SessionTitleKind>,
    #[serde(rename = "lastPrompt", default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    #[serde(rename = "archivedAt", default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(rename = "environmentId", default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(rename = "environmentCwd", default, skip_serializing_if = "Option::is_none")]
    pub environment_cwd: Option<String>,
    #[serde(rename = "forkedFrom", default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<HashMap<String, AgentMeta>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<HashMap<String, Value>>,
    #[serde(rename = "lastTurnReason", default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<SessionLastTurnReason>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMetadataChangedEvent {
    pub changed: Vec<String>,
}

#[async_trait]
pub trait ISessionMetadata: Send + Sync {
    fn ready(&self) -> futures_util::future::BoxFuture<'static, Result<(), WireAsyncError>>;
    fn effective_cwd(&self) -> String;
    fn on_did_change_metadata(
        &self,
        listener: std::sync::Arc<dyn Fn(SessionMetadataChangedEvent) + Send + Sync>,
    ) -> Box<dyn crate::_base::di::lifecycle::IDisposable>;
    async fn read(&self) -> Result<SessionMeta, WireAsyncError>;
    async fn update(
        &self,
        patch: SessionMetaPatch,
        touch_updated_at: Option<bool>,
    ) -> Result<(), WireAsyncError>;
    async fn apply_prompt_metadata(&self, last_prompt: &str, title: &str) -> Result<bool, WireAsyncError>;
    async fn set_title(&self, title: &str) -> Result<(), WireAsyncError>;
    async fn set_generated_title_if_uncustomized(
        &self,
        title: &str,
        force: Option<bool>,
    ) -> Result<bool, WireAsyncError>;
    async fn set_archived(&self, archived: bool) -> Result<(), WireAsyncError>;
    async fn register_agent(&self, agent_id: &str, meta: AgentMeta) -> Result<(), WireAsyncError>;
}
