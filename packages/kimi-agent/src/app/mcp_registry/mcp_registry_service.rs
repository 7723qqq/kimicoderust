use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::utils::paths::{canonical_path, canonical_workspace_root};
use crate::app::mcp_management::mcp_management::GlobalMcpServerConfig;
use crate::app::scopes::LifecycleScope;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::mcp_core::errors::MCP_SERVER_NOT_FOUND;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::mcp_registry::{IMcpRegistryService, McpRegistryEntry, McpRegistryPluginOrigin, McpRegistryQuery, McpServerSource};

#[derive(Debug, Clone)]
pub struct McpRegistryPluginEntry {
    pub name: String,
    pub config: McpServerConfig,
    pub plugin_id: String,
    pub server_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct McpServersDetailed {
    pub servers: HashMap<String, McpServerConfig>,
    pub origins: HashMap<String, String>,
}

#[async_trait]
pub trait IMcpRegistryConfigStore: Send + Sync {
    async fn list(&self) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError>;
    fn path(&self) -> String;
}

#[async_trait]
pub trait IMcpRegistryPluginService: Send + Sync {
    async fn mcp_server_entries(&self) -> Result<Vec<McpRegistryPluginEntry>, WireAsyncError>;
}

#[async_trait]
pub trait IMcpRegistryTrustStore: Send + Sync {
    async fn refresh(&self, canonical_root: &str) -> Result<bool, WireAsyncError>;
}

#[async_trait]
pub trait IMcpRegistryConfigLoader: Send + Sync {
    async fn load(&self, cwd: &str, home_dir: &str) -> Result<McpServersDetailed, WireAsyncError>;
}

pub trait IMcpRegistryBootstrap: Send + Sync {
    fn home_dir(&self) -> String;
}

pub struct McpRegistryService {
    store: Arc<dyn IMcpRegistryConfigStore>,
    plugins: Arc<dyn IMcpRegistryPluginService>,
    loader: Arc<dyn IMcpRegistryConfigLoader>,
    bootstrap: Arc<dyn IMcpRegistryBootstrap>,
    trust: Arc<dyn IMcpRegistryTrustStore>,
}

impl McpRegistryService {
    pub fn new(
        store: Arc<dyn IMcpRegistryConfigStore>,
        plugins: Arc<dyn IMcpRegistryPluginService>,
        loader: Arc<dyn IMcpRegistryConfigLoader>,
        bootstrap: Arc<dyn IMcpRegistryBootstrap>,
        trust: Arc<dyn IMcpRegistryTrustStore>,
    ) -> Self {
        Self {
            store,
            plugins,
            loader,
            bootstrap,
            trust,
        }
    }

    async fn user_entries(&self, out: &mut Vec<McpRegistryEntry>) -> Result<(), WireAsyncError> {
        let origin = self.store.path();
        for server in self.store.list().await? {
            out.push(McpRegistryEntry {
                name: server.name,
                config: server.config,
                source: McpServerSource::Global,
                origin: origin.clone(),
                mutable: true,
                plugin: None,
            });
        }
        Ok(())
    }
}

#[async_trait]
impl IMcpRegistryService for McpRegistryService {
    async fn list(&self, query: Option<McpRegistryQuery>) -> Result<Vec<McpRegistryEntry>, WireAsyncError> {
        let query = query.unwrap_or_default();
        let mut out = Vec::new();
        match query.cwd.as_deref() {
            None => self.user_entries(&mut out).await?,
            Some(cwd) => {
                let root = canonical_workspace_root(cwd);
                let trusted = self.trust.refresh(&root).await?;
                if !trusted {
                    self.user_entries(&mut out).await?;
                } else {
                    let detailed = self.loader.load(&canonical_path(cwd), &self.bootstrap.home_dir()).await?;
                    let store_path = self.store.path();
                    for (name, config) in detailed.servers {
                        let origin = detailed.origins.get(&name).cloned().unwrap_or_else(|| store_path.clone());
                        let mutable = origin == store_path;
                        out.push(McpRegistryEntry {
                            name,
                            config,
                            source: McpServerSource::Global,
                            origin,
                            mutable,
                            plugin: None,
                        });
                    }
                }
            }
        }
        for entry in self.plugins.mcp_server_entries().await? {
            out.push(McpRegistryEntry {
                name: entry.name,
                config: entry.config,
                source: McpServerSource::Plugin,
                origin: entry.plugin_id.clone(),
                mutable: false,
                plugin: Some(McpRegistryPluginOrigin {
                    id: entry.plugin_id,
                    name: entry.server_name,
                }),
            });
        }
        Ok(out)
    }

    async fn get(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<McpRegistryEntry, WireAsyncError> {
        if let Some(entry) = self.list(query).await?.into_iter().find(|candidate| candidate.name == name) {
            return Ok(entry);
        }
        Err(Box::new(Error2::new(MCP_SERVER_NOT_FOUND, format!("MCP server \"{name}\" was not found"))))
    }

    async fn resolve_runtime_target(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<Option<McpRegistryEntry>, WireAsyncError> {
        let matches = self.list(query).await?.into_iter().filter(|entry| entry.name == name).collect::<Vec<_>>();
        if let Some(file) = matches.iter().find(|entry| entry.source == McpServerSource::Global) {
            return Ok(Some(file.clone()));
        }
        Ok(matches.into_iter().find(|entry| entry.source == McpServerSource::Plugin && entry.config.enabled != Some(false)))
    }
}

pub fn register_mcp_registry_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnDemand, "mcpRegistry");
}
