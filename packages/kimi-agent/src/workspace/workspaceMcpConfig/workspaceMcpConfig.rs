use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;
use crate::mcp_core::config_schema::McpServerConfig;

pub type McpConfigAsyncError = Box<dyn Error + Send + Sync>;
pub type McpConfigFuture = BoxFuture<'static, Result<(), McpConfigAsyncError>>;
pub type WaitUntil = Arc<dyn Fn(McpConfigFuture) + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct McpServersChange {
    pub upsert: HashMap<String, McpServerConfig>,
    pub remove: Vec<String>,
}

#[derive(Clone)]
pub struct McpServersChangeEvent {
    pub change: McpServersChange,
    pub wait_until: WaitUntil,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTunables {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_deferred: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_defer_tool_threshold: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_tool_schemas: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_initial_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconnect_max_delay_ms: Option<u64>,
}

pub trait IWorkspaceMcpConfigService: Send + Sync {
    fn ready(&self) -> McpConfigFuture;
    fn servers(&self) -> HashMap<String, McpServerConfig>;
    fn tunables(&self) -> McpTunables;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(McpServersChangeEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn on_did_change_tunables(
        &self,
        listener: Arc<dyn Fn(McpTunables) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
}
