use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::utils::abort::AbortSignal;
use crate::mcp_core::connection_manager::{
    McpResolved, McpServerEntry, McpStatusListener,
};
use crate::mcp_core::oauth::service::McpOAuthService;
use crate::wire::record::WireAsyncError;

pub const MCP_INITIAL_LOAD_WAIT_MS: u64 = 2000;

pub type McpResolvedServer = McpResolved;

#[async_trait]
pub trait IAgentMcpService: Send + Sync {
    fn register(&self) -> Result<(), WireAsyncError>;
    fn oauth_service(&self) -> Option<Arc<McpOAuthService>>;
    async fn wait_for_initial_load(
        &self,
        signal: Option<AbortSignal>,
        budget_ms: Option<u64>,
    ) -> Result<(), WireAsyncError>;
    fn initial_load_duration_ms(&self) -> u64;
    fn list(&self) -> Vec<McpServerEntry>;
    fn resolved(&self, name: &str) -> Option<McpResolvedServer>;
    fn get_remote_server_url(&self, name: &str) -> Option<String>;
    async fn reconnect(
        &self,
        name: &str,
        signal: Option<AbortSignal>,
    ) -> Result<(), WireAsyncError>;
    fn on_status_change(&self, listener: McpStatusListener) -> Box<dyn IDisposable>;
}
