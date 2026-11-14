use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSION_LOG_BRANCH: &str = "_session";
pub const AGENT_OPENED_TYPE: &str = "agent.opened";
pub const AGENT_CLOSED_TYPE: &str = "agent.closed";
pub const AGENT_SWITCHED_TYPE: &str = "agent.switched";
pub const SESSION_META_UPDATED_TYPE: &str = "session.meta_updated";
pub const COMPACTION_STARTED_TYPE: &str = "compaction.started";
pub const COMPACTION_COMPLETED_TYPE: &str = "compaction.completed";
pub const COMPACTION_CANCELLED_TYPE: &str = "compaction.cancelled";

fn now() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOpened {
    pub agent_id: String,
    pub branch: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn agent_opened(agent_id: &str, branch: &str) -> AgentOpened {
    AgentOpened {
        agent_id: agent_id.to_string(),
        branch: branch.to_string(),
        type_: AGENT_OPENED_TYPE.to_string(),
        time: now(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentClosed {
    pub agent_id: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn agent_closed(agent_id: &str) -> AgentClosed {
    AgentClosed {
        agent_id: agent_id.to_string(),
        type_: AGENT_CLOSED_TYPE.to_string(),
        time: now(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSwitched {
    pub agent_id: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<HashMap<String, f64>>,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn agent_switched(
    agent_id: &str,
    branch: &str,
    reason: Option<String>,
    stats: Option<HashMap<String, f64>>,
) -> AgentSwitched {
    AgentSwitched {
        agent_id: agent_id.to_string(),
        branch: branch.to_string(),
        reason,
        stats,
        type_: AGENT_SWITCHED_TYPE.to_string(),
        time: now(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaUpdated {
    pub meta: Value,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn session_meta_updated(meta: Value) -> SessionMetaUpdated {
    SessionMetaUpdated {
        meta,
        type_: SESSION_META_UPDATED_TYPE.to_string(),
        time: now(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStarted {
    pub agent_id: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn compaction_started(
    agent_id: &str,
    reason: &str,
    instruction: Option<String>,
) -> CompactionStarted {
    CompactionStarted {
        agent_id: agent_id.to_string(),
        reason: reason.to_string(),
        instruction,
        type_: COMPACTION_STARTED_TYPE.to_string(),
        time: now(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCompleted {
    pub agent_id: String,
    pub branch: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn compaction_completed(agent_id: &str, branch: &str) -> CompactionCompleted {
    CompactionCompleted {
        agent_id: agent_id.to_string(),
        branch: branch.to_string(),
        type_: COMPACTION_COMPLETED_TYPE.to_string(),
        time: now(),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCancelled {
    pub agent_id: String,
    pub cause: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(rename = "type")]
    pub type_: String,
    pub time: u64,
}

pub fn compaction_cancelled(
    agent_id: &str,
    cause: &str,
    error_message: Option<String>,
) -> CompactionCancelled {
    CompactionCancelled {
        agent_id: agent_id.to_string(),
        cause: cause.to_string(),
        error_message,
        type_: COMPACTION_CANCELLED_TYPE.to_string(),
        time: now(),
    }
}
