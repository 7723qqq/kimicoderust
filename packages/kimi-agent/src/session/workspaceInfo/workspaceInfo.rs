use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::ScopeSeed;
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait ISessionWorkspaceInfo: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn additional_dirs(&self) -> Vec<String>;
    fn primary_by_workspace(&self) -> HashMap<String, String>;
    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn IDisposable>;
}

pub fn session_workspace_info_seed(info: Arc<dyn ISessionWorkspaceInfo>) -> Vec<ScopeSeed> {
    vec![ScopeSeed::new::<ISessionWorkspaceInfo>(info)]
}
