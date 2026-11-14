use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::_base::utils::abort::AbortSignal;
use crate::app::mcp_registry::mcp_registry::{McpRegistryPluginOrigin, McpRegistryQuery, McpServerSource};
use crate::mcp_core::configView::McpServerConfigView;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalMcpServerConfig {
    pub name: String,
    #[serde(flatten)]
    pub config: McpServerConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpManagedServerConfig {
    Config(McpServerConfig),
    View(McpServerConfigView),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpManagedServer {
    pub name: String,
    pub config: McpManagedServerConfig,
    pub source: McpServerSource,
    pub origin: String,
    pub mutable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<McpRegistryPluginOrigin>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpServerTestTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<GlobalMcpServerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerTestResult {
    pub success: bool,
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum McpServerLocator {
    Global { name: String },
    Plugin {
        #[serde(rename = "pluginId")]
        plugin_id: String,
        #[serde(rename = "serverName")]
        server_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerDescriptor {
    pub server_id: String,
    pub locator: McpServerLocator,
    pub runtime_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    pub origin: McpServerSource,
    pub config: McpServerConfigView,
    pub enabled: bool,
    pub editable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerAuthState {
    #[serde(rename = "not-applicable")]
    NotApplicable,
    #[serde(rename = "bearer-token")]
    BearerToken,
    #[serde(rename = "oauth-required")]
    OauthRequired,
    #[serde(rename = "oauth-authorized")]
    OauthAuthorized,
    #[serde(rename = "oauth-expired")]
    OauthExpired,
    #[serde(rename = "unavailable")]
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerInspection {
    #[serde(flatten)]
    pub descriptor: McpServerDescriptor,
    pub auth_status: McpServerAuthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerAuthStatus {
    pub name: String,
    pub auth_status: McpServerAuthState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum McpServerAuthBeginResult {
    #[serde(rename = "authorization-required", rename_all = "camelCase")]
    AuthorizationRequired {
        flow_id: String,
        authorization_url: String,
    },
    #[serde(rename = "already-authorized")]
    AlreadyAuthorized,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerAuthFlowHandle {
    pub flow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpAuthStatusQuery {
    #[serde(flatten)]
    pub registry: McpRegistryQuery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<bool>,
}

#[derive(Clone, Default)]
pub struct McpServerAuthCompletionOptions {
    pub signal: Option<AbortSignal>,
}

#[async_trait]
pub trait IMcpManagementService: Send + Sync {
    async fn list_servers(&self, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError>;
    async fn get_server(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<McpManagedServer, WireAsyncError>;
    async fn add_server(&self, server: GlobalMcpServerConfig, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError>;
    async fn update_server(&self, server: GlobalMcpServerConfig, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError>;
    async fn remove_server(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError>;
    async fn test_server(&self, target: McpServerTestTarget) -> Result<McpServerTestResult, WireAsyncError>;
    async fn list_auth_statuses(&self, query: Option<McpAuthStatusQuery>) -> Result<Vec<McpServerAuthStatus>, WireAsyncError>;
    async fn inspect_servers(&self, targets: Option<Vec<McpServerLocator>>, query: Option<McpRegistryQuery>) -> Result<Vec<McpServerInspection>, WireAsyncError>;
    async fn resolve_server_by_name(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<McpServerLocator, WireAsyncError>;
    async fn begin_server_auth(&self, locator: McpServerLocator, query: Option<McpRegistryQuery>) -> Result<McpServerAuthBeginResult, WireAsyncError>;
    async fn complete_server_auth(&self, handle: McpServerAuthFlowHandle, options: Option<McpServerAuthCompletionOptions>) -> Result<(), WireAsyncError>;
    async fn cancel_server_auth(&self, flow_id: &str) -> Result<(), WireAsyncError>;
    async fn reset_server_auth(&self, locator: McpServerLocator, query: Option<McpRegistryQuery>) -> Result<(), WireAsyncError>;
}
