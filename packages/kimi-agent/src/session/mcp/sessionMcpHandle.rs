use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::instantiation::create_decorator;
use crate::_base::di::scope::{ScopeSeed, seed_instance};
use crate::mcp_core::connection_manager::McpConnectionView;
use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait ISessionMcpHandle: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn connection_manager(&self) -> Arc<dyn McpConnectionView>;
    fn is_baseline_server(&self, name: &str) -> bool;
}

pub fn session_mcp_handle_seed(handle: Arc<dyn ISessionMcpHandle>) -> ScopeSeed {
    let id = create_decorator::<Arc<dyn ISessionMcpHandle>>("sessionMcpHandle");
    seed_instance(&id, handle)
}
