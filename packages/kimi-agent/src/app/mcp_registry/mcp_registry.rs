use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::mcp_core::config_schema::McpServerConfig;
use crate::wire::record::WireAsyncError;

pub use crate::mcp_core::connection_manager::mcp_server_configs_equal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerSource {
    Global,
    Plugin,
    Caller,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpRegistryPluginOrigin {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRegistryEntry {
    pub name: String,
    pub config: McpServerConfig,
    pub source: McpServerSource,
    pub origin: String,
    pub mutable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<McpRegistryPluginOrigin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct McpRegistryQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[async_trait]
pub trait IMcpRegistryService: Send + Sync {
    async fn list(&self, query: Option<McpRegistryQuery>) -> Result<Vec<McpRegistryEntry>, WireAsyncError>;
    async fn get(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<McpRegistryEntry, WireAsyncError>;
    async fn resolve_runtime_target(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<Option<McpRegistryEntry>, WireAsyncError>;
}
