use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::wire::record::WireAsyncError;

pub type WorkspaceKind = String;

pub const WORKSPACE_KIND_MANAGED: &str = "managed";
pub const WORKSPACE_KIND_IMPLICIT: &str = "implicit";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    pub id: String,
    pub root: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<WorkspaceKind>,
    pub additional_dirs: Vec<String>,
    pub created_at: i64,
    pub last_opened_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceCreate {
    pub root: String,
    pub name: Option<String>,
    pub additional_dirs: Option<Vec<String>>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceUpdate {
    pub name: Option<String>,
    pub additional_dirs: Option<Vec<String>>,
}

#[async_trait]
pub trait IWorkspaceService: Send + Sync {
    async fn list(&self) -> Result<Vec<Workspace>, WireAsyncError>;
    async fn get(&self, id: &str) -> Result<Option<Workspace>, WireAsyncError>;
    async fn create(&self, input: WorkspaceCreate) -> Result<Workspace, WireAsyncError>;

    async fn create_or_touch(&self, root: &str) -> Result<Option<Workspace>, WireAsyncError> {
        self.create_or_touch_with_name(root, None).await
    }

    async fn create_or_touch_with_name(
        &self,
        root: &str,
        name: Option<&str>,
    ) -> Result<Option<Workspace>, WireAsyncError>;

    async fn touch(&self, id: &str) -> Result<Option<Workspace>, WireAsyncError>;
    async fn update(
        &self,
        id: &str,
        patch: WorkspaceUpdate,
    ) -> Result<Option<Workspace>, WireAsyncError>;
    async fn set_primary(
        &self,
        id: &str,
        path: &str,
    ) -> Result<Option<Workspace>, WireAsyncError>;
    async fn delete(&self, id: &str) -> Result<(), WireAsyncError>;
}
