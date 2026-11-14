use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::features::goal::types::GoalSnapshot;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWireFields {
    pub id: String,
    pub workspace_id: String,
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<Map<String, Value>>,
}

#[async_trait]
pub trait ISessionLegacyService: Send + Sync {
    async fn goal(&self, session_id: &str) -> Result<Option<GoalSnapshot>, WireAsyncError>;
}
