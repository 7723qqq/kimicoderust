use std::sync::Arc;

use serde::Serialize;
use serde_json::json;

use crate::app::event::event2::{Event2, Event2Class};

use super::workspace::Workspace;
use super::workspace_protocol::{
    WORKSPACE_CREATED_EVENT_TYPE, WORKSPACE_DELETED_EVENT_TYPE, WORKSPACE_UPDATED_EVENT_TYPE,
};

pub const WORKSPACE_CREATED_TYPE: &str = WORKSPACE_CREATED_EVENT_TYPE;
pub const WORKSPACE_UPDATED_TYPE: &str = WORKSPACE_UPDATED_EVENT_TYPE;
pub const WORKSPACE_DELETED_TYPE: &str = WORKSPACE_DELETED_EVENT_TYPE;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCreatedPayload {
    pub workspace: Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCreated {
    pub payload: WorkspaceCreatedPayload,
    pub time: i64,
}

impl WorkspaceCreated {
    pub const TYPE: &'static str = WORKSPACE_CREATED_TYPE;

    pub fn new(payload: WorkspaceCreatedPayload) -> Self {
        Self::with_time(payload, None)
    }

    pub fn with_time(payload: WorkspaceCreatedPayload, time: Option<i64>) -> Self {
        Self {
            payload,
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: WORKSPACE_CREATED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: WORKSPACE_CREATED_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceUpdatedPayload {
    pub workspace: Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceUpdated {
    pub payload: WorkspaceUpdatedPayload,
    pub time: i64,
}

impl WorkspaceUpdated {
    pub const TYPE: &'static str = WORKSPACE_UPDATED_TYPE;

    pub fn new(payload: WorkspaceUpdatedPayload) -> Self {
        Self::with_time(payload, None)
    }

    pub fn with_time(payload: WorkspaceUpdatedPayload, time: Option<i64>) -> Self {
        Self {
            payload,
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: WORKSPACE_UPDATED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: WORKSPACE_UPDATED_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDeletedPayload {
    pub workspace_id: String,
    pub root: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceDeleted {
    pub payload: WorkspaceDeletedPayload,
    pub time: i64,
}

impl WorkspaceDeleted {
    pub const TYPE: &'static str = WORKSPACE_DELETED_TYPE;

    pub fn new(payload: WorkspaceDeletedPayload) -> Self {
        Self::with_time(payload, None)
    }

    pub fn with_time(payload: WorkspaceDeletedPayload, time: Option<i64>) -> Self {
        Self {
            payload,
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: WORKSPACE_DELETED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: WORKSPACE_DELETED_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}
