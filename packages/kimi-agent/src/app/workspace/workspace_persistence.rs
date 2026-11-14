use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;
use crate::wire::record::WireAsyncError;

use super::workspace::Workspace;

pub type WorkspaceChangeListener = Arc<dyn Fn() + Send + Sync>;
pub type WorkspaceCatalogMutation = Arc<
    dyn Fn(WorkspaceCatalog) -> BoxFuture<'static, Result<WorkspaceCatalog, WireAsyncError>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWorkspaceEntry {
    pub root: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_dirs: Option<Vec<String>>,
    pub created_at: String,
    pub last_opened_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWorkspaceFile {
    pub version: u64,
    pub workspaces: IndexMap<String, PersistedWorkspaceEntry>,
    pub deleted_workspace_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_keys: Option<IndexMap<String, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceCatalog {
    pub workspaces: Vec<Workspace>,
    pub deleted_ids: Vec<String>,
    pub idempotency_keys: Option<IndexMap<String, String>>,
}

#[async_trait]
pub trait IWorkspacePersistence: Send + Sync {
    fn on_did_change(&self, listener: WorkspaceChangeListener) -> Box<dyn IDisposable>;
    async fn load(&self) -> Result<Option<WorkspaceCatalog>, WireAsyncError>;
    async fn save(&self, catalog: WorkspaceCatalog) -> Result<(), WireAsyncError>;
    async fn update(
        &self,
        mutate: WorkspaceCatalogMutation,
    ) -> Result<WorkspaceCatalog, WireAsyncError>;
    async fn stamp(&self) -> Result<Option<String>, WireAsyncError>;
}

impl dyn IWorkspacePersistence {
    pub fn on_did_change<F>(&self, listener: F) -> Box<dyn IDisposable>
    where
        F: Fn() + Send + Sync + 'static,
    {
        <dyn IWorkspacePersistence as IWorkspacePersistence>::on_did_change(
            self,
            Arc::new(listener),
        )
    }
}
