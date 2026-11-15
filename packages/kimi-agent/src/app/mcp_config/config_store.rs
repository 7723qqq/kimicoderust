use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, join_all};
use kimi_core::primitives::errors::{
    Error2, Error2Options, domains::config::CONFIG_INVALID, domains::prompt::REQUEST_INVALID,
};
use parking_lot::Mutex;
use serde_json::{Map, Value};

pub use crate::app::mcp_management::mcp_management::GlobalMcpServerConfig;
use crate::app::mcp_management::mcp_management_service::IMcpManagementConfigStore;
use crate::app::mcp_registry::mcp_registry_service::IMcpRegistryConfigStore;
use crate::mcp_core::config_schema::validate_mcp_server_config;
use crate::mcp_core::errors::MCP_SERVER_NOT_FOUND;
use crate::persistence::interface::storage::{IFileSystemStorageService, StorageWriteOptions};
use crate::wire::record::WireAsyncError;
use crate::_base::di::lifecycle::DisposableRef;

const CONFIG_SCOPE: &str = "";
const MCP_CONFIG_KEY: &str = "mcp.json";

pub type McpConfigFuture = BoxFuture<'static, Result<(), WireAsyncError>>;

#[derive(Clone)]
pub struct McpConfigWriteEvent {
    pending: Arc<Mutex<Vec<McpConfigFuture>>>,
}

impl McpConfigWriteEvent {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn wait_until(&self, future: McpConfigFuture) {
        self.pending.lock().push(future);
    }
}

pub type McpConfigWriteListener = Arc<dyn Fn(McpConfigWriteEvent) + Send + Sync>;

#[async_trait]
pub trait IMcpConfigStore: Send + Sync {
    fn path(&self) -> String;
    fn on_did_write(
        &self,
        listener: McpConfigWriteListener,
    ) -> DisposableRef;
    async fn list(&self) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError>;
    async fn get(&self, name: &str) -> Result<GlobalMcpServerConfig, WireAsyncError>;
    async fn add(&self, server: GlobalMcpServerConfig) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError>;
    async fn update(&self, server: GlobalMcpServerConfig) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError>;
    async fn remove(&self, name: &str) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError>;
}

#[derive(Default)]
struct McpConfigFile {
    raw: Map<String, Value>,
    raw_servers: Map<String, Value>,
    servers: Vec<GlobalMcpServerConfig>,
}

pub struct McpConfigStore {
    storage: Arc<dyn IFileSystemStorageService>,
    path: String,
    listeners: Arc<Mutex<BTreeMap<u64, McpConfigWriteListener>>>,
    next_listener_id: AtomicU64,
    mutation_tail: tokio::sync::Mutex<()>,
    write_pending: AtomicBool,
}

impl McpConfigStore {
    pub fn new(storage: Arc<dyn IFileSystemStorageService>, home_dir: &str) -> Self {
        Self {
            storage,
            path: format!("{}/{}", home_dir.trim_end_matches(['/', '\\']), MCP_CONFIG_KEY),
            listeners: Arc::new(Mutex::new(BTreeMap::new())),
            next_listener_id: AtomicU64::new(0),
            mutation_tail: tokio::sync::Mutex::new(()),
            write_pending: AtomicBool::new(false),
        }
    }

    async fn mutate<T>(
        &self,
        work: impl Future<Output = Result<T, WireAsyncError>>,
    ) -> Result<T, WireAsyncError> {
        let result = {
            let _guard = self.mutation_tail.lock().await;
            work.await
        }?;
        if self.write_pending.swap(false, Ordering::SeqCst) {
            self.fire_async_concurrent().await?;
        }
        Ok(result)
    }

    async fn fire_async_concurrent(&self) -> Result<(), WireAsyncError> {
        let event = McpConfigWriteEvent::new();
        for listener in self.listeners.lock().values() {
            listener(event.clone());
        }
        let pending = std::mem::take(&mut *event.pending.lock());
        for result in join_all(pending).await {
            result?;
        }
        Ok(())
    }

    async fn read(&self) -> Result<McpConfigFile, WireAsyncError> {
        let bytes = match self.storage.read(CONFIG_SCOPE, MCP_CONFIG_KEY).await {
            Ok(bytes) => bytes,
            Err(error) => {
                let message = format!("Failed to read {}: {error}", self.path);
                return Err(config_error(message, Some(error)));
            }
        };
        let Some(bytes) = bytes else {
            return Ok(McpConfigFile::default());
        };
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if text.trim().is_empty() {
            return Ok(McpConfigFile::default());
        }
        let parsed: Value = match serde_json::from_str(text) {
            Ok(parsed) => parsed,
            Err(error) => {
                let message = format!("Invalid JSON in {}: {error}", self.path);
                return Err(config_error(message, Some(Box::new(error))));
            }
        };
        let Some(raw) = parsed.as_object().cloned() else {
            return Err(config_error(
                format!("Invalid MCP config in {}: expected a JSON object", self.path),
                None,
            ));
        };
        let raw_servers = match raw.get("mcpServers") {
            None => Map::new(),
            Some(value) => {
                let Some(servers) = value.as_object().cloned() else {
                    return Err(config_error(
                        format!("Invalid MCP config in {}: \"mcpServers\" must be an object", self.path),
                        None,
                    ));
                };
                servers
            }
        };
        let mut servers = Vec::new();
        for (name, value) in &raw_servers {
            servers.push(parse_server(name, value.clone())?);
        }
        Ok(McpConfigFile {
            raw,
            raw_servers,
            servers,
        })
    }

    async fn write(&self, file: &McpConfigFile, raw_servers: Map<String, Value>) -> Result<(), WireAsyncError> {
        let mut raw = file.raw.clone();
        raw.insert("mcpServers".to_string(), Value::Object(raw_servers));
        let mut text = serde_json::to_string_pretty(&Value::Object(raw))?;
        text.push('\n');
        self.storage
            .write(
                CONFIG_SCOPE,
                MCP_CONFIG_KEY,
                text.into_bytes(),
                Some(StorageWriteOptions {
                    atomic: Some(true),
                    ..Default::default()
                }),
            )
            .await?;
        self.write_pending.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl IMcpConfigStore for McpConfigStore {
    fn path(&self) -> String {
        self.path.clone()
    }

    fn on_did_write(
        &self,
        listener: McpConfigWriteListener,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        let listeners = self.listeners.clone();
        crate::_base::di::lifecycle::to_disposable(move || {
            listeners.lock().remove(&id);
        })
    }

    async fn list(&self) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError> {
        Ok(self.read().await?.servers)
    }

    async fn get(&self, name: &str) -> Result<GlobalMcpServerConfig, WireAsyncError> {
        let normalized_name = normalize_server_name(name)?;
        self.read()
            .await?
            .servers
            .into_iter()
            .find(|entry| entry.name == normalized_name)
            .ok_or_else(|| server_not_found(&normalized_name))
    }

    async fn add(&self, server: GlobalMcpServerConfig) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError> {
        self.mutate(async {
            let normalized = parse_server_input(server)?;
            let file = self.read().await?;
            if file.raw_servers.contains_key(&normalized.name) {
                return Err(Box::new(Error2::new(
                    REQUEST_INVALID,
                    format!("MCP server \"{}\" already exists", normalized.name),
                )) as WireAsyncError);
            }
            let mut raw_servers = file.raw_servers.clone();
            raw_servers.insert(normalized.name.clone(), persisted_entry(&normalized)?);
            self.write(&file, raw_servers).await?;
            self.list().await
        })
        .await
    }

    async fn update(&self, server: GlobalMcpServerConfig) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError> {
        self.mutate(async {
            let normalized = parse_server_input(server)?;
            let file = self.read().await?;
            if !file.raw_servers.contains_key(&normalized.name) {
                return Err(server_not_found(&normalized.name));
            }
            let mut raw_servers = file.raw_servers.clone();
            raw_servers.insert(normalized.name.clone(), persisted_entry(&normalized)?);
            self.write(&file, raw_servers).await?;
            self.list().await
        })
        .await
    }

    async fn remove(&self, name: &str) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError> {
        self.mutate(async {
            let normalized_name = normalize_server_name(name)?;
            let file = self.read().await?;
            if !file.raw_servers.contains_key(&normalized_name) {
                return Ok(file.servers);
            }
            let raw_servers = file
                .raw_servers
                .iter()
                .filter(|(entry_name, _)| *entry_name != &normalized_name)
                .map(|(entry_name, value)| (entry_name.clone(), value.clone()))
                .collect();
            self.write(&file, raw_servers).await?;
            self.list().await
        })
        .await
    }
}

#[async_trait]
impl IMcpManagementConfigStore for McpConfigStore {
    async fn add(&self, server: GlobalMcpServerConfig) -> Result<(), WireAsyncError> {
        IMcpConfigStore::add(self, server).await.map(|_| ())
    }

    async fn update(&self, server: GlobalMcpServerConfig) -> Result<(), WireAsyncError> {
        IMcpConfigStore::update(self, server).await.map(|_| ())
    }

    async fn remove(&self, name: &str) -> Result<(), WireAsyncError> {
        IMcpConfigStore::remove(self, name).await.map(|_| ())
    }
}

#[async_trait]
impl IMcpRegistryConfigStore for McpConfigStore {
    async fn list(&self) -> Result<Vec<GlobalMcpServerConfig>, WireAsyncError> {
        IMcpConfigStore::list(self).await
    }

    fn path(&self) -> String {
        self.path.clone()
    }
}

fn parse_server_input(server: GlobalMcpServerConfig) -> Result<GlobalMcpServerConfig, WireAsyncError> {
    let name = normalize_server_name(&server.name)?;
    parse_server(&name, serde_json::to_value(server)?)
}

fn parse_server(name: &str, value: Value) -> Result<GlobalMcpServerConfig, WireAsyncError> {
    let config = validate_mcp_server_config(value).map_err(|error| {
        config_error(
            format!("Invalid MCP server \"{name}\" in global config: {error}"),
            None,
        )
    })?;
    Ok(GlobalMcpServerConfig {
        name: name.to_string(),
        config,
    })
}

fn persisted_entry(server: &GlobalMcpServerConfig) -> Result<Value, WireAsyncError> {
    Ok(serde_json::to_value(&server.config)?)
}

pub fn normalize_server_name(name: &str) -> Result<String, WireAsyncError> {
    let normalized = name.trim();
    if !normalized.is_empty() {
        return Ok(normalized.to_string());
    }
    Err(Box::new(Error2::new(
        REQUEST_INVALID,
        "MCP server name cannot be empty",
    )))
}

fn server_not_found(name: &str) -> WireAsyncError {
    Box::new(Error2::new(
        MCP_SERVER_NOT_FOUND,
        format!("MCP server \"{name}\" was not found"),
    ))
}

fn config_error(message: String, cause: Option<WireAsyncError>) -> WireAsyncError {
    Box::new(Error2::with_options(
        CONFIG_INVALID,
        message,
        Error2Options {
            cause,
            ..Default::default()
        },
    ))
}

pub fn register_mcp_config_store() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::App,
        crate::_base::di::scope::ScopeActivation::OnDemand,
        "mcpConfig",
    );
}
