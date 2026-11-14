use std::sync::Arc;

use async_trait::async_trait;
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{
    ServiceIdentifier, ServicesAccessor, create_decorator,
};
use crate::_base::di::lifecycle::IDisposable;
use crate::wire::record::WireAsyncError;

pub trait IWorkspaceScopeHandle: Send + Sync {
    fn id(&self) -> &str;
    fn accessor(&self) -> Arc<dyn ServicesAccessor>;
    fn dispose(&self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceRef {
    WorkspaceId {
        workspace_id: String,
        root: Option<String>,
    },
    Root {
        root: String,
    },
}

pub trait WorkspaceHandlerRegistry: Send + Sync {
    fn list(&self) -> Vec<Arc<dyn IWorkspaceScopeHandle>>;
}

pub trait WorkspaceSessionRegistry: Send + Sync {
    fn list(&self, workspace_id: &str) -> Vec<String>;
}

#[async_trait]
pub trait IWorkspaceLifecycleService: Send + Sync {
    fn on_did_materialize_handler(
        &self,
        listener: Arc<dyn Fn(Arc<dyn IWorkspaceScopeHandle>) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    async fn handler_for(
        &self,
        reference: WorkspaceRef,
    ) -> Result<Arc<dyn IWorkspaceScopeHandle>, WireAsyncError>;
    fn handlers(&self) -> Arc<dyn WorkspaceHandlerRegistry>;
    fn sessions(&self) -> Arc<dyn WorkspaceSessionRegistry>;
}

pub static I_WORKSPACE_LIFECYCLE_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("workspaceLifecycleService"));
