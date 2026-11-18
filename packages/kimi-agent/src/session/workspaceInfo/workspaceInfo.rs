use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::instantiation::create_decorator;
use crate::_base::di::scope::{ScopeSeed, seed_instance};
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait ISessionWorkspaceInfo: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn additional_dirs(&self) -> Vec<String>;
    fn primary_by_workspace(&self) -> HashMap<String, String>;
    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef;
}

pub fn session_workspace_info_seed(info: Arc<dyn ISessionWorkspaceInfo>) -> ScopeSeed {
    let id = create_decorator::<Arc<dyn ISessionWorkspaceInfo>>("sessionWorkspaceInfo");
    seed_instance(&id, info)
}
