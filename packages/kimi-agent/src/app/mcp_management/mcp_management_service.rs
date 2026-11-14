use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::environment::environment::EnvironmentResolver;
use crate::app::mcp_registry::mcp_registry::{IMcpRegistryService, McpRegistryEntry, McpRegistryQuery, McpServerSource};
use crate::app::scopes::LifecycleScope;
use crate::mcp_core::configView::to_mcp_server_config_view;
use crate::mcp_core::config_schema::{McpAuth, McpServerConfig, McpTransport, validate_mcp_server_config};
use crate::mcp_core::connection_manager::{McpConnectionManager, McpConnectionManagerOptions, McpDefaults, McpServerStatus};
use crate::mcp_core::errors::MCP_SERVER_NOT_FOUND;
use crate::mcp_core::oauth::provider::OAuthInvalidationScope;
use crate::mcp_core::oauth::service::{AlreadyAuthorizedError, BeginAuthorizationOptions, BeginAuthorizationResult, CompleteAuthorizationOptions, McpOAuthService, McpOAuthTokenState};
use crate::mcp_core::oauth::store::canonical_mcp_oauth_resource;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;
use kimi_core::primitives::errors::domains::config::CONFIG_INVALID;
use kimi_core::primitives::errors::domains::prompt::REQUEST_INVALID;

use super::mcp_management::{GlobalMcpServerConfig, IMcpManagementService, McpAuthStatusQuery, McpManagedServer, McpManagedServerConfig, McpServerAuthBeginResult, McpServerAuthCompletionOptions, McpServerAuthFlowHandle, McpServerAuthState, McpServerAuthStatus, McpServerDescriptor, McpServerInspection, McpServerLocator, McpServerTestResult, McpServerTestTarget};

const DEFAULT_AUTH_TIMEOUT_MS: u64 = 15 * 60_000;
const AUTH_FLOW_IDLE_TIMEOUT_MS: u64 = 15 * 60_000;
const MAX_AUTH_TIMEOUT_MS: u64 = 2_147_483_647;

#[async_trait]
pub trait IMcpManagementConfigStore: Send + Sync {
    async fn add(&self, server: GlobalMcpServerConfig) -> Result<(), WireAsyncError>;
    async fn update(&self, server: GlobalMcpServerConfig) -> Result<(), WireAsyncError>;
    async fn remove(&self, name: &str) -> Result<(), WireAsyncError>;
}

#[async_trait]
pub trait IMcpManagementConfig: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn defaults(&self) -> McpDefaults;
}

#[derive(Clone)]
struct ActiveAuthFlow {
    flow: BeginAuthorizationResult,
    idle_timer: IHostTimerHandle,
}

pub struct McpManagementService {
    registry: Arc<dyn IMcpRegistryService>,
    store: Arc<dyn IMcpManagementConfigStore>,
    oauth: Arc<McpOAuthService>,
    config: Arc<dyn IMcpManagementConfig>,
    identity: Arc<dyn IAgentIdentity>,
    environment_resolver: Option<Arc<dyn EnvironmentResolver>>,
    log: Arc<dyn ILogService>,
    timers: Arc<dyn IHostTimers>,
    auth_flows: Arc<Mutex<HashMap<String, ActiveAuthFlow>>>,
}

impl McpManagementService {
    pub fn new(
        registry: Arc<dyn IMcpRegistryService>,
        store: Arc<dyn IMcpManagementConfigStore>,
        oauth: Arc<McpOAuthService>,
        config: Arc<dyn IMcpManagementConfig>,
        identity: Arc<dyn IAgentIdentity>,
        environment_resolver: Option<Arc<dyn EnvironmentResolver>>,
        log: Arc<dyn ILogService>,
        timers: Arc<dyn IHostTimers>,
    ) -> Self {
        Self {
            registry,
            store,
            oauth,
            config,
            identity,
            environment_resolver,
            log,
            timers,
            auth_flows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn guard_mutation(&self, name: &str, query: McpRegistryQuery) -> Result<(), WireAsyncError> {
        let matches = self.registry.list(Some(query)).await?.into_iter().filter(|entry| entry.name == name).collect::<Vec<_>>();
        for entry in matches {
            if entry.source == McpServerSource::Global && !entry.mutable {
                return Err(throw_read_only_mcp_server(&entry));
            }
        }
        Ok(())
    }

    async fn resolve_test_target(&self, target: &McpServerTestTarget) -> Result<GlobalMcpServerConfig, WireAsyncError> {
        if let Some(server) = &target.server {
            if target.name.as_ref().is_some_and(|name| name != &server.name) {
                return Err(request_invalid("Pass either an MCP server name or an inline server config, not both"));
            }
            let value = serde_json::to_value(&server.config).map_err(|error| Box::new(error) as WireAsyncError)?;
            let config = validate_mcp_server_config(value).map_err(|error| {
                Box::new(Error2::new(CONFIG_INVALID, format!("Invalid MCP server \"{}\": {error}", server.name))) as WireAsyncError
            })?;
            return Ok(GlobalMcpServerConfig {
                name: server.name.clone(),
                config,
            });
        }
        let Some(name) = &target.name else {
            return Err(request_invalid("Pass an MCP server name or an inline server config"));
        };
        let matches = self.registry.list(Some(McpRegistryQuery { cwd: target.cwd.clone() })).await?.into_iter().filter(|entry| &entry.name == name).collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(server_not_found(name));
        }
        let enabled = matches.iter().filter(|entry| entry.config.enabled != Some(false)).collect::<Vec<_>>();
        if enabled.len() > 1 {
            return Err(request_invalid(format!("MCP runtime name \"{name}\" is shared by multiple enabled servers")));
        }
        let entry = enabled.first().copied().or_else(|| matches.first()).expect("nonempty matches");
        Ok(GlobalMcpServerConfig {
            name: entry.name.clone(),
            config: entry.config.clone(),
        })
    }

    fn connection_manager(&self, stdio_cwd: Option<String>, include_environment: bool) -> McpConnectionManager {
        let defaults = self.config.defaults();
        let identity = self.identity.clone();
        let mut options = McpConnectionManagerOptions {
            log: Some(self.log.clone()),
            stdio_cwd: stdio_cwd.map(|cwd| Arc::new(move || cwd.clone()) as Arc<dyn Fn() -> String + Send + Sync>),
            environment_id: include_environment.then(|| "local".to_string()),
            oauth_service: Some(self.oauth.clone()),
            resolve_client_name: Some(Arc::new(move || identity.current().slug)),
            resolve_defaults: Some(Arc::new(move || defaults.clone())),
            ..Default::default()
        };
        if include_environment {
            options.environment_resolver = self.environment_resolver.clone();
        }
        McpConnectionManager::new(options)
    }

    async fn with_probe<T>(
        &self,
        server: GlobalMcpServerConfig,
        cwd: Option<String>,
        inspect: impl FnOnce(&McpConnectionManager) -> T,
    ) -> Result<T, WireAsyncError> {
        self.wait_for_readiness().await?;
        let stdio_cwd = if server.config.transport == McpTransport::Stdio {
            Some(normalize_path(cwd.as_deref()))
        } else {
            cwd
        };
        let manager = self.connection_manager(stdio_cwd, true);
        let mut configs = HashMap::new();
        configs.insert(server.name.clone(), server.config);
        manager.connect_all(configs).await;
        let output = inspect(&manager);
        manager.shutdown().await?;
        Ok(output)
    }

    async fn server_descriptors(&self, query: McpRegistryQuery) -> Result<Vec<McpServerRuntimeDescriptor>, WireAsyncError> {
        Ok(self.registry.list(Some(query)).await?.iter().map(server_descriptor).collect())
    }

    async fn resolve_server(&self, locator: McpServerLocator, query: McpRegistryQuery) -> Result<McpServerRuntimeDescriptor, WireAsyncError> {
        let catalog = self.server_descriptors(query).await?;
        let server = select_server_descriptors(&catalog, Some(vec![locator]))?.into_iter().next().expect("one selected descriptor");
        self.require_unambiguous_runtime_name(&catalog, &server)?;
        Ok(server)
    }

    fn require_unambiguous_runtime_name(&self, catalog: &[McpServerRuntimeDescriptor], server: &McpServerRuntimeDescriptor) -> Result<(), WireAsyncError> {
        let conflict = catalog.iter().find(|candidate| {
            candidate.server_id != server.server_id && candidate.enabled && candidate.runtime_name == server.runtime_name
        });
        if conflict.is_some() {
            return Err(request_invalid(format!("MCP runtime name \"{}\" is shared by multiple enabled servers; use the locator-addressed RPC instead", server.runtime_name)));
        }
        Ok(())
    }

    async fn server_auth_state(&self, entry: &McpRegistryEntry, cwd: Option<String>, verify: Option<bool>) -> Result<McpServerAuthState, WireAsyncError> {
        let server = &entry.config;
        if server.enabled == Some(false) || server.transport == McpTransport::Stdio {
            return Ok(McpServerAuthState::NotApplicable);
        }
        if server.bearer_token_env_var.is_some() {
            return Ok(McpServerAuthState::BearerToken);
        }
        if server.headers.is_some() && server.auth != Some(McpAuth::Oauth) {
            return Ok(McpServerAuthState::NotApplicable);
        }
        if server.transport != McpTransport::Http && server.auth != Some(McpAuth::Oauth) {
            return Ok(McpServerAuthState::NotApplicable);
        }
        let url = require_remote_mcp_config(&entry.name, server)?.url.clone().expect("remote URL");
        let tokens = self.oauth.token_state(&entry.name, &url).await?;
        let offline = || offline_auth_state(server, &tokens);
        if verify == Some(true) {
            return self.with_probe(GlobalMcpServerConfig { name: entry.name.clone(), config: server.clone() }, cwd, |manager| {
                let status = manager.get(&entry.name).map(|entry| entry.status);
                match status {
                    Some(McpServerStatus::Connected) => if tokens.has_tokens { McpServerAuthState::OauthAuthorized } else { McpServerAuthState::NotApplicable },
                    Some(McpServerStatus::NeedsAuth) => if tokens.has_tokens { McpServerAuthState::OauthExpired } else { McpServerAuthState::OauthRequired },
                    _ => offline(),
                }
            }).await;
        }
        if verify == Some(false) || tokens.has_tokens || server.auth == Some(McpAuth::Oauth) {
            return Ok(offline());
        }
        self.with_probe(GlobalMcpServerConfig { name: entry.name.clone(), config: server.clone() }, cwd, |manager| {
            let status = manager.get(&entry.name).map(|entry| entry.status);
            match status {
                Some(McpServerStatus::Connected) => if tokens.has_tokens { McpServerAuthState::OauthAuthorized } else { McpServerAuthState::NotApplicable },
                Some(McpServerStatus::NeedsAuth) => if tokens.has_tokens { McpServerAuthState::OauthExpired } else { McpServerAuthState::OauthRequired },
                _ => offline(),
            }
        }).await
    }

    async fn inspect_server_descriptors(&self, descriptors: Vec<McpServerRuntimeDescriptor>, catalog: &[McpServerRuntimeDescriptor]) -> Result<Vec<McpServerRuntimeInspection>, WireAsyncError> {
        let mut unique = HashMap::new();
        for server in catalog {
            unique.insert(server.server_id.clone(), server);
        }
        let mut runtime_name_counts = HashMap::<String, usize>::new();
        for server in unique.values().filter(|server| server.enabled) {
            *runtime_name_counts.entry(server.runtime_name.clone()).or_default() += 1;
        }
        let mut credential_states = HashMap::<String, McpOAuthTokenState>::new();
        let mut probe_configs = HashMap::<String, McpServerConfig>::new();
        for server in &descriptors {
            if configured_mcp_auth_state(server).is_some() || runtime_name_counts.get(&server.runtime_name) != Some(&1) {
                continue;
            }
            let config = require_remote_mcp_config(&server.runtime_name, &server.config)?;
            credential_states.insert(server.server_id.clone(), self.oauth.token_state(&server.runtime_name, config.url.as_deref().expect("remote URL")).await?);
            probe_configs.insert(server.runtime_name.clone(), server.config.clone());
        }
        let manager = if probe_configs.is_empty() {
            None
        } else {
            let manager = self.connection_manager(None, false);
            manager.connect_all(probe_configs).await;
            Some(manager)
        };
        let checked_at = chrono::Utc::now().timestamp_millis();
        let inspections = descriptors.into_iter().map(|server| {
            if let Some(configured) = configured_mcp_auth_state(&server) {
                return McpServerRuntimeInspection {
                    descriptor: server,
                    auth_status: configured,
                    checked_at: None,
                    error: None,
                };
            }
            if runtime_name_counts.get(&server.runtime_name) != Some(&1) {
                return McpServerRuntimeInspection {
                    auth_status: McpServerAuthState::Unavailable,
                    checked_at: Some(checked_at),
                    error: Some(format!("MCP runtime name \"{}\" is not unique", server.runtime_name)),
                    descriptor: server,
                };
            }
            let tokens = credential_states.get(&server.server_id);
            let entry = manager.as_ref().and_then(|manager| manager.get(&server.runtime_name));
            match entry.as_ref().map(|entry| entry.status) {
                Some(McpServerStatus::Connected) => McpServerRuntimeInspection {
                    auth_status: if tokens.is_some_and(|tokens| tokens.has_tokens) { McpServerAuthState::OauthAuthorized } else { McpServerAuthState::NotApplicable },
                    checked_at: Some(checked_at),
                    error: None,
                    descriptor: server,
                },
                Some(McpServerStatus::NeedsAuth) => McpServerRuntimeInspection {
                    auth_status: if tokens.is_some_and(|tokens| tokens.has_tokens) { McpServerAuthState::OauthExpired } else { McpServerAuthState::OauthRequired },
                    checked_at: Some(checked_at),
                    error: None,
                    descriptor: server,
                },
                status => McpServerRuntimeInspection {
                    auth_status: McpServerAuthState::Unavailable,
                    checked_at: Some(checked_at),
                    error: Some(entry.and_then(|entry| entry.error).unwrap_or_else(|| format!("MCP server finished with status {}", status.map(|status| status.as_str()).unwrap_or("unknown")))),
                    descriptor: server,
                },
            }
        }).collect::<Vec<_>>();
        if let Some(manager) = manager {
            manager.shutdown().await?;
        }
        Ok(inspections)
    }

    async fn wait_for_readiness(&self) -> Result<(), WireAsyncError> {
        self.config.ready().await?;
        self.identity.resolved().await?;
        Ok(())
    }

    pub fn dispose(&self) {
        let flows = self.auth_flows.lock().drain().map(|(_, flow)| flow).collect::<Vec<_>>();
        for active in flows {
            self.timers.clear_timeout(active.idle_timer);
            tokio::spawn(async move {
                let _ = (active.flow.cancel)().await;
            });
        }
    }
}

#[async_trait]
impl IMcpManagementService for McpManagementService {
    async fn list_servers(&self, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError> {
        Ok(self.registry.list(Some(query.unwrap_or_default())).await?.iter().map(to_managed_server).collect())
    }

    async fn get_server(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<McpManagedServer, WireAsyncError> {
        Ok(to_managed_server(&self.registry.get(name, Some(query.unwrap_or_default())).await?))
    }

    async fn add_server(&self, server: GlobalMcpServerConfig, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError> {
        let query = query.unwrap_or_default();
        let name = normalize_server_name(&server.name)?;
        self.guard_mutation(&name, query.clone()).await?;
        self.store.add(GlobalMcpServerConfig { name, config: server.config }).await?;
        self.list_servers(Some(query)).await
    }

    async fn update_server(&self, server: GlobalMcpServerConfig, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError> {
        let query = query.unwrap_or_default();
        let name = normalize_server_name(&server.name)?;
        self.guard_mutation(&name, query.clone()).await?;
        self.store.update(GlobalMcpServerConfig { name, config: server.config }).await?;
        self.list_servers(Some(query)).await
    }

    async fn remove_server(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<Vec<McpManagedServer>, WireAsyncError> {
        let query = query.unwrap_or_default();
        let normalized = normalize_server_name(name)?;
        self.guard_mutation(&normalized, query.clone()).await?;
        self.store.remove(&normalized).await?;
        self.list_servers(Some(query)).await
    }

    async fn test_server(&self, target: McpServerTestTarget) -> Result<McpServerTestResult, WireAsyncError> {
        self.wait_for_readiness().await?;
        let resolved = self.resolve_test_target(&target).await?;
        let name = resolved.name.clone();
        self.with_probe(resolved, target.cwd, move |manager| standalone_test_result(&name, manager)).await
    }

    async fn list_auth_statuses(&self, query: Option<McpAuthStatusQuery>) -> Result<Vec<McpServerAuthStatus>, WireAsyncError> {
        self.wait_for_readiness().await?;
        let query = query.unwrap_or_default();
        let entries = self.registry.list(Some(query.registry.clone())).await?;
        let mut output = Vec::with_capacity(entries.len());
        for entry in entries {
            let name = entry.name.clone();
            let auth_status = self.server_auth_state(&entry, query.registry.cwd.clone(), query.verify).await?;
            output.push(McpServerAuthStatus { name, auth_status });
        }
        Ok(output)
    }

    async fn inspect_servers(&self, targets: Option<Vec<McpServerLocator>>, query: Option<McpRegistryQuery>) -> Result<Vec<McpServerInspection>, WireAsyncError> {
        self.wait_for_readiness().await?;
        let catalog = self.server_descriptors(query.unwrap_or_default()).await?;
        let descriptors = select_server_descriptors(&catalog, targets)?;
        let inspections = self.inspect_server_descriptors(descriptors, &catalog).await?;
        Ok(inspections.into_iter().map(|inspection| {
            let runtime = inspection.descriptor;
            McpServerInspection {
                descriptor: McpServerDescriptor {
                    server_id: runtime.server_id,
                    locator: runtime.locator,
                    runtime_name: runtime.runtime_name,
                    canonical_url: runtime.canonical_url,
                    origin: runtime.origin,
                    config: to_mcp_server_config_view(runtime.config),
                    enabled: runtime.enabled,
                    editable: runtime.editable,
                },
                auth_status: inspection.auth_status,
                checked_at: inspection.checked_at,
                error: inspection.error,
            }
        }).collect())
    }

    async fn resolve_server_by_name(&self, name: &str, query: Option<McpRegistryQuery>) -> Result<McpServerLocator, WireAsyncError> {
        let query = query.unwrap_or_default();
        self.registry.get(name, Some(query.clone())).await?;
        let catalog = self.server_descriptors(query).await?;
        let matches = catalog.iter().filter(|candidate| candidate.runtime_name == name).collect::<Vec<_>>();
        let descriptor = matches.iter().copied().find(|candidate| candidate.enabled).or_else(|| matches.first().copied()).expect("registry get ensures a match");
        self.require_unambiguous_runtime_name(&catalog, descriptor)?;
        Ok(descriptor.locator.clone())
    }

    async fn begin_server_auth(&self, locator: McpServerLocator, query: Option<McpRegistryQuery>) -> Result<McpServerAuthBeginResult, WireAsyncError> {
        self.wait_for_readiness().await?;
        let server = self.resolve_server(locator, query.unwrap_or_default()).await?;
        let config = require_oauth_mcp_config(&server.runtime_name, &server.config)?;
        let flow = match self.oauth.begin_authorization(&server.runtime_name, config.url.as_deref().expect("remote URL"), BeginAuthorizationOptions { client_label: None }).await {
            Ok(flow) => flow,
            Err(error) if error.downcast_ref::<AlreadyAuthorizedError>().is_some() => return Ok(McpServerAuthBeginResult::AlreadyAuthorized),
            Err(error) => return Err(error),
        };
        let flow_id = Uuid::new_v4().to_string();
        let callback_id = flow_id.clone();
        let callback_flows = self.auth_flows.clone();
        let idle_timer = self.timers.set_timeout(Arc::new(move || {
            let expired = callback_flows.lock().remove(&callback_id);
            if let Some(expired) = expired {
                tokio::spawn(async move {
                    let _ = (expired.flow.cancel)().await;
                });
            }
        }), AUTH_FLOW_IDLE_TIMEOUT_MS);
        let authorization_url = flow.authorization_url.clone();
        self.auth_flows.lock().insert(flow_id.clone(), ActiveAuthFlow { flow, idle_timer });
        Ok(McpServerAuthBeginResult::AuthorizationRequired { flow_id, authorization_url })
    }

    async fn complete_server_auth(&self, handle: McpServerAuthFlowHandle, options: Option<McpServerAuthCompletionOptions>) -> Result<(), WireAsyncError> {
        if handle.timeout_ms.is_some_and(|timeout| timeout < 1 || timeout > MAX_AUTH_TIMEOUT_MS) {
            return Err(request_invalid(format!("MCP OAuth timeoutMs must be an integer between 1 and {MAX_AUTH_TIMEOUT_MS}")));
        }
        let active = self.auth_flows.lock().get(&handle.flow_id).cloned().ok_or_else(|| request_invalid(format!("Unknown MCP OAuth flow: {}", handle.flow_id)))?;
        self.timers.clear_timeout(active.idle_timer);
        let result = (active.flow.complete)(CompleteAuthorizationOptions {
            signal: options.and_then(|options| options.signal),
            timeout_ms: Some(handle.timeout_ms.unwrap_or(DEFAULT_AUTH_TIMEOUT_MS)),
        }).await;
        self.auth_flows.lock().remove(&handle.flow_id);
        result
    }

    async fn cancel_server_auth(&self, flow_id: &str) -> Result<(), WireAsyncError> {
        let active = self.auth_flows.lock().remove(flow_id);
        let Some(active) = active else {
            return Ok(());
        };
        self.timers.clear_timeout(active.idle_timer);
        (active.flow.cancel)().await
    }

    async fn reset_server_auth(&self, locator: McpServerLocator, query: Option<McpRegistryQuery>) -> Result<(), WireAsyncError> {
        self.wait_for_readiness().await?;
        let server = self.resolve_server(locator, query.unwrap_or_default()).await?;
        let config = require_remote_mcp_config(&server.runtime_name, &server.config)?;
        self.oauth.invalidate(&server.runtime_name, config.url.as_deref().expect("remote URL"), OAuthInvalidationScope::Tokens).await
    }
}

fn request_invalid(message: impl Into<String>) -> WireAsyncError {
    Box::new(Error2::new(REQUEST_INVALID, message))
}

fn server_not_found(name: &str) -> WireAsyncError {
    Box::new(Error2::new(MCP_SERVER_NOT_FOUND, format!("MCP server \"{name}\" was not found")))
}

fn normalize_server_name(name: &str) -> Result<String, WireAsyncError> {
    let normalized = name.trim();
    if normalized.is_empty() {
        return Err(request_invalid("MCP server name cannot be empty"));
    }
    Ok(normalized.to_string())
}

fn throw_read_only_mcp_server(entry: &McpRegistryEntry) -> WireAsyncError {
    request_invalid(format!("MCP server \"{}\" is read-only: it is defined in {} — edit that file instead", entry.name, entry.origin))
}

fn to_managed_server(entry: &McpRegistryEntry) -> McpManagedServer {
    McpManagedServer {
        name: entry.name.clone(),
        config: if entry.mutable {
            McpManagedServerConfig::Config(entry.config.clone())
        } else {
            McpManagedServerConfig::View(to_mcp_server_config_view(entry.config.clone()))
        },
        source: entry.source,
        origin: entry.origin.clone(),
        mutable: entry.mutable,
        plugin: entry.plugin.clone(),
    }
}

fn require_remote_mcp_config<'a>(name: &str, config: &'a McpServerConfig) -> Result<&'a McpServerConfig, WireAsyncError> {
    if config.transport != McpTransport::Stdio {
        return Ok(config);
    }
    Err(request_invalid(format!("MCP server \"{name}\" does not use a remote transport")))
}

fn require_oauth_mcp_config<'a>(name: &str, input: &'a McpServerConfig) -> Result<&'a McpServerConfig, WireAsyncError> {
    let config = require_remote_mcp_config(name, input)?;
    if config.bearer_token_env_var.is_some() {
        return Err(request_invalid(format!("MCP server \"{name}\" uses a static bearer token")));
    }
    if config.headers.is_some() && config.auth != Some(McpAuth::Oauth) {
        return Err(request_invalid(format!("MCP server \"{name}\" uses static headers and is not marked for OAuth")));
    }
    Ok(config)
}

pub fn mcp_server_id(locator: &McpServerLocator) -> String {
    match locator {
        McpServerLocator::Global { name } => format!("global:{}", encode_uri_component(name)),
        McpServerLocator::Plugin { plugin_id, server_name } => format!("plugin:{}:{}", encode_uri_component(plugin_id), encode_uri_component(server_name)),
    }
}

pub fn describe_mcp_server_locator(locator: &McpServerLocator) -> String {
    match locator {
        McpServerLocator::Global { name } => name.clone(),
        McpServerLocator::Plugin { plugin_id, server_name } => format!("{plugin_id}/{server_name}"),
    }
}

#[derive(Debug, Clone)]
struct McpServerRuntimeDescriptor {
    server_id: String,
    locator: McpServerLocator,
    runtime_name: String,
    canonical_url: Option<String>,
    origin: McpServerSource,
    config: McpServerConfig,
    enabled: bool,
    editable: bool,
}

struct McpServerRuntimeInspection {
    descriptor: McpServerRuntimeDescriptor,
    auth_status: McpServerAuthState,
    checked_at: Option<i64>,
    error: Option<String>,
}

fn server_descriptor(entry: &McpRegistryEntry) -> McpServerRuntimeDescriptor {
    let locator = if entry.source == McpServerSource::Plugin {
        entry.plugin.as_ref().map(|plugin| McpServerLocator::Plugin { plugin_id: plugin.id.clone(), server_name: plugin.name.clone() }).unwrap_or_else(|| McpServerLocator::Global { name: entry.name.clone() })
    } else {
        McpServerLocator::Global { name: entry.name.clone() }
    };
    McpServerRuntimeDescriptor {
        server_id: mcp_server_id(&locator),
        locator,
        runtime_name: entry.name.clone(),
        canonical_url: if entry.config.transport == McpTransport::Stdio { None } else { entry.config.url.as_deref().and_then(|url| canonical_mcp_oauth_resource(url).ok()) },
        origin: entry.source,
        config: entry.config.clone(),
        enabled: entry.config.enabled != Some(false),
        editable: entry.mutable,
    }
}

fn select_server_descriptors(catalog: &[McpServerRuntimeDescriptor], targets: Option<Vec<McpServerLocator>>) -> Result<Vec<McpServerRuntimeDescriptor>, WireAsyncError> {
    let Some(targets) = targets else {
        return Ok(catalog.to_vec());
    };
    let by_id = catalog.iter().map(|server| (server.server_id.clone(), server)).collect::<HashMap<_, _>>();
    targets.into_iter().map(|target| {
        if let Some(server) = by_id.get(&mcp_server_id(&target)) {
            return Ok((*server).clone());
        }
        Err(Box::new(Error2::new(MCP_SERVER_NOT_FOUND, format!("MCP server \"{}\" was not found", describe_mcp_server_locator(&target)))) as WireAsyncError)
    }).collect()
}

fn configured_mcp_auth_state(server: &McpServerRuntimeDescriptor) -> Option<McpServerAuthState> {
    if !server.enabled || server.config.enabled == Some(false) || server.config.transport == McpTransport::Stdio {
        return Some(McpServerAuthState::NotApplicable);
    }
    if server.config.bearer_token_env_var.is_some() {
        return Some(McpServerAuthState::BearerToken);
    }
    if server.config.headers.is_some() && server.config.auth != Some(McpAuth::Oauth) {
        return Some(McpServerAuthState::NotApplicable);
    }
    None
}

fn offline_auth_state(server: &McpServerConfig, tokens: &McpOAuthTokenState) -> McpServerAuthState {
    if tokens.has_tokens {
        return if !tokens.expired || tokens.has_refresh_token { McpServerAuthState::OauthAuthorized } else { McpServerAuthState::OauthExpired };
    }
    if server.auth == Some(McpAuth::Oauth) {
        McpServerAuthState::OauthRequired
    } else {
        McpServerAuthState::NotApplicable
    }
}

fn standalone_test_result(name: &str, manager: &McpConnectionManager) -> McpServerTestResult {
    let entry = manager.get(name);
    let status = entry.as_ref().map(|entry| entry.status);
    if status != Some(McpServerStatus::Connected) {
        return McpServerTestResult {
            success: false,
            output: entry.and_then(|entry| entry.error).unwrap_or_else(|| format!("MCP server \"{name}\" finished with status {}", status.map(|status| status.as_str()).unwrap_or("unknown"))),
        };
    }
    let tools = manager.resolved(name).map(|resolved| resolved.raw_tools).unwrap_or_default();
    let mut lines = vec![format!("Connected to MCP server \"{name}\"."), format!("Available tools: {}", tools.len())];
    lines.extend(tools.into_iter().map(|tool| {
        if tool.description.is_empty() {
            format!("- {}", tool.name)
        } else {
            format!("- {}: {}", tool.name, tool.description)
        }
    }));
    McpServerTestResult {
        success: true,
        output: lines.join("\n"),
    }
}

fn encode_uri_component(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn normalize_path(cwd: Option<&str>) -> String {
    let path = cwd.map(str::to_string).unwrap_or_else(|| std::env::current_dir().unwrap_or_default().to_string_lossy().into_owned());
    path.replace('\\', "/")
}

pub fn register_mcp_management_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnDemand, "mcpManagement");
}
