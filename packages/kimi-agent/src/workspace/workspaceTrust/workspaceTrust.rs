use std::error::Error;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;

use crate::_base::di::lifecycle::IDisposable;

pub type WorkspaceTrustAsyncError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceTrustChange {
    pub trusted: bool,
}

#[async_trait]
pub trait IWorkspaceTrust: Send + Sync {
    fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>>;
    async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError>;
    fn is_trusted(&self) -> bool;
    async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError>;
    async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError>;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(WorkspaceTrustChange) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
}
