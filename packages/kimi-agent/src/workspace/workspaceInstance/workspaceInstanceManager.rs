use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::IDisposable;
use crate::wire::record::WireAsyncError;

use super::workspace_instance::{WorkspaceInstance, WorkspaceInstanceSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceInstanceRef {
    WorkspaceId {
        workspace_id: String,
        root: Option<String>,
    },
    Root {
        root: String,
    },
}

#[derive(Clone)]
pub struct WorkspaceInstanceChange {
    pub workspace_id: String,
    pub instance: Option<Arc<WorkspaceInstance>>,
}

#[derive(Clone)]
pub struct WorkspaceInstancesSnapshot {
    pub workspaces: Vec<WorkspaceInstanceSnapshot>,
}

pub struct WorkspaceInstanceLease {
    pub instance: Arc<WorkspaceInstance>,
    pub dispose: Box<dyn FnOnce() + Send>,
}

impl WorkspaceInstanceLease {
    pub fn dispose(self) {
        (self.dispose)()
    }
}

#[async_trait]
pub trait IWorkspaceInstanceManager: Send + Sync {
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(WorkspaceInstanceChange) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    async fn acquire(&self, reference: WorkspaceInstanceRef) -> Result<WorkspaceInstanceLease, WireAsyncError>;
    fn get(&self, workspace_id: &str) -> Option<Arc<WorkspaceInstance>>;
    fn find_by_root(&self, root: &str) -> Option<Arc<WorkspaceInstance>>;
    fn find_containing(&self, cwd: &str) -> Option<Arc<WorkspaceInstance>>;
    fn list(&self) -> Vec<Arc<WorkspaceInstance>>;
    fn snapshot(&self) -> WorkspaceInstancesSnapshot;
}
