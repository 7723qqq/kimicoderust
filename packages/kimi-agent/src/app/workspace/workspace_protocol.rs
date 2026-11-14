use serde::{Deserialize, Serialize};

pub const WORKSPACE_CREATED_EVENT_TYPE: &str = "event.workspace.created";
pub const WORKSPACE_UPDATED_EVENT_TYPE: &str = "event.workspace.updated";
pub const WORKSPACE_DELETED_EVENT_TYPE: &str = "event.workspace.deleted";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub root: String,
    pub name: String,
    pub created_at: String,
    pub last_opened_at: String,
    pub session_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCreatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub workspace: Workspace,
}

impl WorkspaceCreatedEvent {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            type_: WORKSPACE_CREATED_EVENT_TYPE.to_string(),
            workspace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceUpdatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub workspace: Workspace,
}

impl WorkspaceUpdatedEvent {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            type_: WORKSPACE_UPDATED_EVENT_TYPE.to_string(),
            workspace,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDeletedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub workspace_id: String,
    pub root: String,
}

impl WorkspaceDeletedEvent {
    pub fn new(workspace_id: String, root: String) -> Self {
        Self {
            type_: WORKSPACE_DELETED_EVENT_TYPE.to_string(),
            workspace_id,
            root,
        }
    }
}
