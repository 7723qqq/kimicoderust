use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;
use crate::features::externalHooks::internal::types::HookDef;
use crate::features::skill::catalog::types::SkillRoot;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::wire::record::WireAsyncError;

use super::types::{
    EnabledPluginSessionStart, EnabledPluginSystemPrompt, PluginAgentRoot, PluginCommandDef,
    PluginInfo, PluginMcpServerEntry, PluginMutationSummary, PluginReloadEvent, PluginSummary,
    PluginUpdateStatus, ReloadSummary,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallPluginInput {
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPluginEnabledInput {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetPluginMcpServerEnabledInput {
    pub id: String,
    pub server: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovePluginInput {
    pub id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetPluginInfoInput {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
}

pub type PluginSuspensionTask =
    Arc<dyn Fn() -> BoxFuture<'static, Result<(), WireAsyncError>> + Send + Sync>;
pub type PluginReloadListener = Arc<dyn Fn(PluginReloadEvent) + Send + Sync>;
pub type PluginMutationListener = Arc<dyn Fn(PluginMutationSummary) + Send + Sync>;

#[async_trait]
pub trait IPluginService: Send + Sync {
    async fn list_plugins(&self) -> Result<Vec<PluginSummary>, WireAsyncError>;
    async fn install_plugin(&self, input: InstallPluginInput) -> Result<PluginSummary, WireAsyncError>;
    async fn set_plugin_enabled(&self, input: SetPluginEnabledInput) -> Result<(), WireAsyncError>;
    async fn set_plugin_mcp_server_enabled(
        &self,
        input: SetPluginMcpServerEnabledInput,
    ) -> Result<(), WireAsyncError>;
    async fn remove_plugin(&self, input: RemovePluginInput) -> Result<(), WireAsyncError>;
    async fn reload_plugins(&self) -> Result<ReloadSummary, WireAsyncError>;
    async fn with_mcp_servers_suspended(
        &self,
        id: &str,
        task: PluginSuspensionTask,
    ) -> Result<(), WireAsyncError>;
    async fn get_plugin_info(&self, input: GetPluginInfoInput) -> Result<PluginInfo, WireAsyncError>;
    async fn list_plugin_commands(&self) -> Result<Vec<PluginCommandDef>, WireAsyncError>;
    async fn check_updates(&self) -> Result<Vec<PluginUpdateStatus>, WireAsyncError>;
    async fn plugin_skill_roots(&self) -> Result<Vec<SkillRoot>, WireAsyncError>;
    async fn plugin_agent_roots(&self) -> Result<Vec<PluginAgentRoot>, WireAsyncError>;
    async fn enabled_session_starts(&self) -> Result<Vec<EnabledPluginSessionStart>, WireAsyncError>;
    async fn enabled_system_prompts(&self) -> Result<Vec<EnabledPluginSystemPrompt>, WireAsyncError>;
    async fn enabled_mcp_servers(&self) -> Result<HashMap<String, McpServerConfig>, WireAsyncError>;
    async fn mcp_server_entries(&self) -> Result<Vec<PluginMcpServerEntry>, WireAsyncError>;
    async fn enabled_hooks(&self) -> Result<Vec<HookDef>, WireAsyncError>;
    fn has_loaded_snapshot(&self) -> bool;
    fn enabled_plugin_ids(&self) -> Option<Vec<String>>;
    fn on_did_reload(&self, listener: PluginReloadListener) -> Box<dyn IDisposable>;
    fn on_did_mutate(&self, listener: PluginMutationListener) -> Box<dyn IDisposable>;
}
