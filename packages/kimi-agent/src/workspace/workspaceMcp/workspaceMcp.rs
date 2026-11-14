use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::mcp_core::config_schema::McpServerConfig;
use crate::mcp_core::connection_manager::McpConnectionManager;
use crate::session::mcp::session_mcp_handle::ISessionMcpHandle;
use crate::workspace::workspace_mcp_config::workspace_mcp_config::{
    McpConfigAsyncError, McpConfigFuture,
};

#[async_trait]
pub trait ISessionMcpOverlay: Send + Sync {
    fn handle(&self) -> Arc<dyn ISessionMcpHandle>;
    async fn shutdown(&self) -> Result<(), McpConfigAsyncError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionMcpOverlayOptions {
    pub stdio_cwd: Option<String>,
    pub session_id: Option<String>,
}

pub trait IWorkspaceMcpService: Send + Sync {
    fn ready(&self) -> McpConfigFuture;
    fn connection_manager(&self) -> Arc<McpConnectionManager>;
    fn session_handle(&self) -> Arc<dyn ISessionMcpHandle>;
    fn session_overlay(
        &self,
        servers: HashMap<String, McpServerConfig>,
        opts: Option<SessionMcpOverlayOptions>,
    ) -> Arc<dyn ISessionMcpOverlay>;
}
