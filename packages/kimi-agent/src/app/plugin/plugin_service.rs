use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, join_all};
use indexmap::IndexMap;
use kimi_core::primitives::errors::Error2;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::log::log::ILogService;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::mcp_registry::mcp_registry_service::{
    IMcpRegistryPluginService, McpRegistryPluginEntry,
};
use crate::app::scopes::LifecycleScope;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::features::externalHooks::internal::types::HookDef;
use crate::features::skill::catalog::skill_discovery::ISkillDiscovery;
use crate::features::skill::catalog::types::SkillRoot;
use crate::llm_adapter::provider::provider::IProviderService;
use crate::mcp_core::config_schema::{McpServerConfig, McpTransport};
use crate::region::KIMI_CODE_PROVIDER_NAME;
use crate::wire::record::WireAsyncError;

use super::availability::PluginAvailabilityStore;
use super::errors::{PLUGIN_LOAD_FAILED, PLUGIN_NOT_FOUND};
use super::manager::{DiscoverSkills, PluginManager, PluginManagerOptions, PluginProvenance};
use super::marketplace::{PluginClient, plugin_client_for_identity};
use super::plugin::{
    GetPluginInfoInput, IPluginService, InstallPluginInput, PluginMutationListener,
    PluginReloadListener, PluginSuspensionTask, RemovePluginInput, SetPluginEnabledInput,
    SetPluginMcpServerEnabledInput,
};
use super::types::{
    EnabledPluginSessionStart, EnabledPluginSystemPrompt, PluginAgentRoot, PluginCommandDef,
    PluginInfo, PluginMcpServerEntry, PluginMutation, PluginMutationKind, PluginMutationSummary,
    PluginOrigin, PluginReloadEvent, PluginState, PluginSummary, PluginUpdateStatus, ReloadSummary,
    normalize_plugin_id,
};

const KIMI_CODE_BASE_URL_ENV: &str = "KIMI_CODE_BASE_URL";
const KIMI_CODE_OAUTH_HOST_ENV: &str = "KIMI_CODE_OAUTH_HOST";
const KIMI_OAUTH_HOST_ENV: &str = "KIMI_OAUTH_HOST";

struct PluginReloadNotification {
    summary: ReloadSummary,
    delivery: BoxFuture<'static, Result<(), WireAsyncError>>,
}

struct PluginMutationOutcome<T> {
    result: T,
    notification: PluginReloadNotification,
}

#[derive(Default)]
struct PluginServiceState {
    snapshot_loaded: bool,
    load_error: Option<String>,
    mcp_suspensions: HashMap<String, usize>,
}

struct ChangeListeners<T> {
    next_id: AtomicU64,
    listeners: Mutex<IndexMap<u64, Arc<dyn Fn(T) + Send + Sync>>>,
}

impl<T> Default for ChangeListeners<T> {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            listeners: Mutex::new(IndexMap::new()),
        }
    }
}

impl<T> ChangeListeners<T> {
    fn add(self: &Arc<Self>, listener: Arc<dyn Fn(T) + Send + Sync>) -> DisposableRef {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        Arc::new(ChangeSubscription {
            owner: self.clone(),
            id,
        })
    }

    fn fire(&self, event: T)
    where
        T: Clone,
    {
        let listeners = self.listeners.lock().values().cloned().collect::<Vec<_>>();
        for listener in listeners {
            listener(event.clone());
        }
    }
}

struct ChangeSubscription<T> {
    owner: Arc<ChangeListeners<T>>,
    id: u64,
}

impl<T> IDisposable for ChangeSubscription<T>
where
    T: Send + Sync,
{
    fn dispose(&self) {
        self.owner.listeners.lock().shift_remove(&self.id);
    }
}

pub struct PluginService {
    home_dir: String,
    env_base_url: Option<String>,
    env_oauth_host: Option<String>,
    manager: PluginManager,
    availability: PluginAvailabilityStore,
    client: PluginClient,
    platform: String,
    providers: Arc<dyn IProviderService>,
    telemetry: Arc<dyn ITelemetryService>,
    initial_load: OnceCell<()>,
    state: Mutex<PluginServiceState>,
    mutation_queue: AsyncMutex<()>,
    reload_listeners: Arc<ChangeListeners<PluginReloadEvent>>,
    mutation_listeners: Arc<ChangeListeners<PluginMutationSummary>>,
}

impl PluginService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        discovery: Arc<dyn ISkillDiscovery>,
        providers: Arc<dyn IProviderService>,
        telemetry: Arc<dyn ITelemetryService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        let home_dir = bootstrap.home_dir();
        let env_base_url = bootstrap.get_env(KIMI_CODE_BASE_URL_ENV);
        let env_oauth_host = bootstrap
            .get_env(KIMI_CODE_OAUTH_HOST_ENV)
            .or_else(|| bootstrap.get_env(KIMI_OAUTH_HOST_ENV));
        let discover_skills: DiscoverSkills = Arc::new(move |roots| {
            let discovery = discovery.clone();
            Box::pin(async move { discovery.discover(roots).await })
        });
        let manager = PluginManager::new(PluginManagerOptions {
            kimi_home_dir: home_dir.clone(),
            discover_skills: Some(discover_skills),
            log: Some(log),
            plugin_node_runner: bootstrap.args().plugin_node_runner.clone(),
        });
        let client = plugin_client_for_identity(&bootstrap.client_identity().platform);
        let platform = bootstrap.platform();
        Self {
            home_dir: home_dir.clone(),
            env_base_url,
            env_oauth_host,
            manager,
            availability: PluginAvailabilityStore::new(&home_dir),
            client,
            platform,
            providers,
            telemetry,
            initial_load: OnceCell::new(),
            state: Mutex::new(PluginServiceState::default()),
            mutation_queue: AsyncMutex::new(()),
            reload_listeners: Arc::new(ChangeListeners::default()),
            mutation_listeners: Arc::new(ChangeListeners::default()),
        }
    }

    async fn reload_and_notify(
        &self,
        mutation: Option<PluginMutation>,
    ) -> Result<PluginReloadNotification, WireAsyncError> {
        let summary = self.manager.reload().await?;
        {
            let mut state = self.state.lock();
            state.snapshot_loaded = true;
            state.load_error = None;
        }
        let event = PluginReloadEvent::new(summary.clone());
        self.reload_listeners.fire(event.clone());
        let pending = event.take_pending();
        let delivery = Box::pin(async move {
            for result in join_all(pending).await {
                result?;
            }
            Ok(())
        });
        if let Some(mutation) = mutation {
            self.mutation_listeners
                .fire(PluginMutationSummary::new(summary.clone(), mutation));
        }
        Ok(PluginReloadNotification { summary, delivery })
    }

    async fn run_notified_mutation<T, F, Fut>(&self, operation: F) -> Result<T, WireAsyncError>
    where
        T: Send,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<PluginMutationOutcome<T>, WireAsyncError>> + Send,
    {
        self.start_initial_load().await;
        let outcome = {
            let _guard = self.mutation_queue.lock().await;
            self.assert_loaded()?;
            operation().await?
        };
        outcome.notification.delivery.await?;
        Ok(outcome.result)
    }

    async fn run_serialized_operation<T, F, Fut>(&self, operation: F) -> Result<T, WireAsyncError>
    where
        T: Send,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, WireAsyncError>> + Send,
    {
        self.start_initial_load().await;
        let _guard = self.mutation_queue.lock().await;
        self.assert_loaded()?;
        operation().await
    }

    async fn run_management_read<T, F, Fut>(&self, operation: F) -> Result<T, WireAsyncError>
    where
        T: Send,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, WireAsyncError>> + Send,
    {
        self.wait_for_pending_mutations().await;
        self.assert_loaded()?;
        operation().await
    }

    async fn run_consumption_read<T, F, Fut>(
        &self,
        fallback: T,
        operation: F,
    ) -> Result<T, WireAsyncError>
    where
        T: Send,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, WireAsyncError>> + Send,
    {
        self.wait_for_pending_mutations().await;
        if !self.state.lock().snapshot_loaded {
            return Ok(fallback);
        }
        operation().await
    }

    async fn wait_for_pending_mutations(&self) {
        self.start_initial_load().await;
        let _guard = self.mutation_queue.lock().await;
    }

    async fn start_initial_load(&self) {
        self.initial_load
            .get_or_init(|| async {
                let _guard = self.mutation_queue.lock().await;
                self.load_once().await;
            })
            .await;
    }

    async fn load_once(&self) {
        match self.manager.load().await {
            Ok(()) => {
                let mut state = self.state.lock();
                state.snapshot_loaded = true;
                state.load_error = None;
            }
            Err(error) => {
                self.state.lock().load_error = Some(error.to_string());
            }
        }
    }

    fn assert_loaded(&self) -> Result<(), WireAsyncError> {
        let Some(load_error) = self.state.lock().load_error.clone() else {
            return Ok(());
        };
        let mut details = Map::new();
        details.insert(
            "kimiHomeDir".to_string(),
            Value::String(self.home_dir.clone()),
        );
        Err(Box::new(
            Error2::new(
                PLUGIN_LOAD_FAILED,
                format!(
                    "Plugin state failed to load: {load_error}. Fix the file at {}/plugins/installed.json and run /plugins reload.",
                    self.home_dir
                ),
            )
            .with_details(details)
            .with_cause(Box::new(std::io::Error::other(load_error))),
        ))
    }

    async fn managed_kimi_code_env_for_plugins(&self) -> Result<HashMap<String, String>, WireAsyncError> {
        self.providers.ready().await?;
        let provider = self.providers.get(KIMI_CODE_PROVIDER_NAME);
        let has_env_override = self.env_base_url.is_some() || self.env_oauth_host.is_some();
        let base_url = self.env_base_url.as_ref().map(|url| url.trim_end_matches('/').to_string()).or_else(|| {
            provider.as_ref().and_then(|provider| provider.base_url.clone())
        });
        let oauth_host = if has_env_override {
            self.env_oauth_host.clone()
        } else {
            provider
                .as_ref()
                .and_then(|provider| provider.oauth.as_ref())
                .and_then(|oauth| oauth.oauth_host.clone())
        };
        let mut env = HashMap::new();
        if let Some(base_url) = base_url {
            env.insert(KIMI_CODE_BASE_URL_ENV.to_string(), base_url);
        }
        if let Some(oauth_host) = oauth_host {
            env.insert(KIMI_CODE_OAUTH_HOST_ENV.to_string(), oauth_host);
        }
        Ok(env)
    }
}

#[async_trait]
impl IPluginService for PluginService {
    async fn list_plugins(&self) -> Result<Vec<PluginSummary>, WireAsyncError> {
        self.run_management_read(|| async {
            let records = self.manager.list();
            let summaries = self.manager.summaries();
            let _ = self.availability.refresh().await;
            let visible = records
                .iter()
                .filter(|record| self.availability.supports(record, self.client, &self.platform))
                .map(|record| record.id.clone())
                .collect::<std::collections::HashSet<_>>();
            Ok(summaries
                .into_iter()
                .filter(|summary| visible.contains(&summary.id))
                .collect())
        })
        .await
    }

    async fn install_plugin(&self, input: InstallPluginInput) -> Result<PluginSummary, WireAsyncError> {
        self.run_notified_mutation(|| async {
            let _ = self.availability.refresh().await;
            let match_ = self.availability.match_by_artifact(&input.source);
            let provenance = match_.map(|match_| PluginProvenance {
                origin: PluginOrigin::Marketplace,
                catalog_id: Some(match_.catalog_id),
                marketplace_source: Some(match_.marketplace_source),
            });
            let record = self.manager.install(&input.source, provenance).await?;
            let info = self.manager.info(&record.id, None).ok_or_else(|| {
                Box::new(BugIndicatingError::new(format!(
                    "Plugin \"{}\" missing right after install",
                    record.id
                ))) as WireAsyncError
            })?;
            let notification = self
                .reload_and_notify(Some(PluginMutation {
                    kind: PluginMutationKind::Install,
                    id: record.id,
                }))
                .await?;
            Ok(PluginMutationOutcome {
                result: plugin_info_into_summary(info),
                notification,
            })
        })
        .await
    }

    async fn set_plugin_enabled(&self, input: SetPluginEnabledInput) -> Result<(), WireAsyncError> {
        self.run_notified_mutation(|| async {
            let id = normalize_plugin_id(&input.id);
            self.manager.set_enabled(&id, input.enabled).await?;
            let notification = self
                .reload_and_notify(Some(PluginMutation {
                    kind: if input.enabled {
                        PluginMutationKind::Enable
                    } else {
                        PluginMutationKind::Disable
                    },
                    id: id.clone(),
                }))
                .await?;
            self.telemetry.track2(
                "plugin_toggle",
                json!({
                    "plugin_id": id,
                    "enabled": input.enabled,
                    "enabled_plugins": self.enabled_plugin_ids().map(|ids| ids.join(",")),
                }),
            );
            Ok(PluginMutationOutcome {
                result: (),
                notification,
            })
        })
        .await
    }

    async fn set_plugin_mcp_server_enabled(
        &self,
        input: SetPluginMcpServerEnabledInput,
    ) -> Result<(), WireAsyncError> {
        self.run_notified_mutation(|| async {
            self.manager
                .set_mcp_server_enabled(&input.id, &input.server, input.enabled)
                .await?;
            let notification = self
                .reload_and_notify(Some(PluginMutation {
                    kind: PluginMutationKind::McpServer,
                    id: input.id.clone(),
                }))
                .await?;
            Ok(PluginMutationOutcome {
                result: (),
                notification,
            })
        })
        .await
    }

    async fn remove_plugin(&self, input: RemovePluginInput) -> Result<(), WireAsyncError> {
        self.run_notified_mutation(|| async {
            self.manager.remove(&input.id).await?;
            let notification = self
                .reload_and_notify(Some(PluginMutation {
                    kind: PluginMutationKind::Remove,
                    id: input.id.clone(),
                }))
                .await?;
            Ok(PluginMutationOutcome {
                result: (),
                notification,
            })
        })
        .await
    }

    async fn reload_plugins(&self) -> Result<ReloadSummary, WireAsyncError> {
        let notification = {
            let _guard = self.mutation_queue.lock().await;
            let _ = self.initial_load.set(());
            match self.reload_and_notify(None).await {
                Ok(notification) => notification,
                Err(error) => {
                    let message = error.to_string();
                    self.state.lock().load_error = Some(message.clone());
                    let mut details = Map::new();
                    details.insert(
                        "kimiHomeDir".to_string(),
                        Value::String(self.home_dir.clone()),
                    );
                    return Err(Box::new(
                        Error2::new(
                            PLUGIN_LOAD_FAILED,
                            format!("Failed to reload plugins: {message}"),
                        )
                        .with_details(details)
                        .with_cause(error),
                    ));
                }
            }
        };
        let summary = notification.summary.clone();
        notification.delivery.await?;
        Ok(summary)
    }

    async fn with_mcp_servers_suspended(
        &self,
        id: &str,
        task: PluginSuspensionTask,
    ) -> Result<(), WireAsyncError> {
        let key = normalize_plugin_id(id);
        let increment_key = key.clone();
        self.run_notified_mutation(|| async {
            {
                let mut state = self.state.lock();
                *state.mcp_suspensions.entry(increment_key).or_insert(0) += 1;
            }
            let notification = self.reload_and_notify(None).await?;
            Ok(PluginMutationOutcome {
                result: (),
                notification,
            })
        })
        .await?;
        let task_result = task().await;
        let decrement_key = key;
        self.run_notified_mutation(|| async {
            {
                let mut state = self.state.lock();
                let remaining = state
                    .mcp_suspensions
                    .get(&decrement_key)
                    .copied()
                    .unwrap_or(1)
                    - 1;
                if remaining == 0 {
                    state.mcp_suspensions.remove(&decrement_key);
                } else {
                    state.mcp_suspensions.insert(decrement_key.clone(), remaining);
                }
            }
            let notification = self.reload_and_notify(None).await?;
            Ok(PluginMutationOutcome {
                result: (),
                notification,
            })
        })
        .await?;
        task_result
    }

    async fn get_plugin_info(&self, input: GetPluginInfoInput) -> Result<PluginInfo, WireAsyncError> {
        self.run_management_read(|| async {
            let Some(info) = self.manager.info(&input.id, input.locale.as_deref()) else {
                let mut details = Map::new();
                details.insert("id".to_string(), Value::String(input.id.clone()));
                return Err(Box::new(
                    Error2::new(
                        PLUGIN_NOT_FOUND,
                        format!("Plugin \"{}\" is not installed", input.id),
                    )
                    .with_details(details),
                ) as WireAsyncError);
            };
            Ok(info)
        })
        .await
    }

    async fn list_plugin_commands(&self) -> Result<Vec<PluginCommandDef>, WireAsyncError> {
        self.run_serialized_operation(|| async { Ok(self.manager.enabled_commands().await) })
            .await
    }

    async fn check_updates(&self) -> Result<Vec<PluginUpdateStatus>, WireAsyncError> {
        self.run_management_read(|| async { self.manager.check_updates().await })
            .await
    }

    async fn plugin_skill_roots(&self) -> Result<Vec<SkillRoot>, WireAsyncError> {
        self.run_consumption_read(Vec::new(), || async { Ok(self.manager.plugin_skill_roots()) })
            .await
    }

    async fn plugin_agent_roots(&self) -> Result<Vec<PluginAgentRoot>, WireAsyncError> {
        self.run_consumption_read(Vec::new(), || async { Ok(self.manager.plugin_agent_roots()) })
            .await
    }

    async fn enabled_session_starts(&self) -> Result<Vec<EnabledPluginSessionStart>, WireAsyncError> {
        self.run_consumption_read(Vec::new(), || async { Ok(self.manager.enabled_session_starts()) })
            .await
    }

    async fn enabled_system_prompts(&self) -> Result<Vec<EnabledPluginSystemPrompt>, WireAsyncError> {
        self.run_consumption_read(Vec::new(), || async { Ok(self.manager.enabled_system_prompts()) })
            .await
    }

    async fn enabled_mcp_servers(&self) -> Result<HashMap<String, McpServerConfig>, WireAsyncError> {
        self.run_consumption_read(HashMap::new(), || async {
            let mut plugin_servers = self.manager.enabled_mcp_servers();
            let suspended = self.state.lock().mcp_suspensions.clone();
            for entry in self.manager.mcp_server_entries() {
                if suspended.contains_key(&entry.plugin_id) {
                    plugin_servers.remove(&entry.name);
                }
            }
            if !plugin_servers
                .values()
                .any(|server| server.transport == McpTransport::Stdio)
            {
                return Ok(plugin_servers);
            }
            let managed_env = self.managed_kimi_code_env_for_plugins().await?;
            Ok(with_managed_kimi_plugin_env(plugin_servers, &managed_env))
        })
        .await
    }

    async fn mcp_server_entries(&self) -> Result<Vec<PluginMcpServerEntry>, WireAsyncError> {
        self.run_management_read(|| async {
            let suspended = self.state.lock().mcp_suspensions.clone();
            let mut entries = self
                .manager
                .mcp_server_entries()
                .into_iter()
                .map(|entry| {
                    if suspended.contains_key(&entry.plugin_id) {
                        PluginMcpServerEntry {
                            config: McpServerConfig {
                                enabled: Some(false),
                                ..entry.config.clone()
                            },
                            ..entry
                        }
                    } else {
                        entry
                    }
                })
                .collect::<Vec<_>>();
            if !entries
                .iter()
                .any(|entry| entry.config.transport == McpTransport::Stdio)
            {
                return Ok(entries);
            }
            let managed_env = self.managed_kimi_code_env_for_plugins().await?;
            entries = with_managed_kimi_plugin_env_on_entries(entries, &managed_env);
            Ok(entries)
        })
        .await
    }

    async fn enabled_hooks(&self) -> Result<Vec<HookDef>, WireAsyncError> {
        self.run_consumption_read(Vec::new(), || async { Ok(self.manager.enabled_hooks()) })
            .await
    }

    fn has_loaded_snapshot(&self) -> bool {
        self.state.lock().snapshot_loaded
    }

    fn enabled_plugin_ids(&self) -> Option<Vec<String>> {
        if !self.state.lock().snapshot_loaded {
            return None;
        }
        let mut ids = self
            .manager
            .summaries()
            .into_iter()
            .filter(|plugin| plugin.enabled && plugin.state == PluginState::Ok)
            .map(|plugin| plugin.id)
            .collect::<Vec<_>>();
        ids.sort();
        Some(ids)
    }

    fn on_did_reload(&self, listener: PluginReloadListener) -> DisposableRef {
        self.reload_listeners.add(listener)
    }

    fn on_did_mutate(&self, listener: PluginMutationListener) -> DisposableRef {
        self.mutation_listeners.add(listener)
    }
}

#[async_trait]
impl IMcpRegistryPluginService for PluginService {
    async fn mcp_server_entries(&self) -> Result<Vec<McpRegistryPluginEntry>, WireAsyncError> {
        Ok(IPluginService::mcp_server_entries(self)
            .await?
            .into_iter()
            .map(|entry| McpRegistryPluginEntry {
                name: entry.name,
                config: entry.config,
                plugin_id: entry.plugin_id,
                server_name: entry.server_name,
            })
            .collect())
    }
}

fn with_managed_kimi_plugin_env(
    plugin_servers: HashMap<String, McpServerConfig>,
    managed_env: &HashMap<String, String>,
) -> HashMap<String, McpServerConfig> {
    if managed_env.is_empty() {
        return plugin_servers;
    }
    plugin_servers
        .into_iter()
        .map(|(name, server)| {
            let server = if server.transport == McpTransport::Stdio {
                let mut env = server.env.clone().unwrap_or_default();
                env.extend(managed_env.clone());
                McpServerConfig {
                    env: Some(env),
                    ..server
                }
            } else {
                server
            };
            (name, server)
        })
        .collect()
}

fn with_managed_kimi_plugin_env_on_entries(
    entries: Vec<PluginMcpServerEntry>,
    managed_env: &HashMap<String, String>,
) -> Vec<PluginMcpServerEntry> {
    if managed_env.is_empty() {
        return entries;
    }
    entries
        .into_iter()
        .map(|entry| {
            let PluginMcpServerEntry {
                name,
                config,
                plugin_id,
                server_name,
            } = entry;
            if config.transport == McpTransport::Stdio {
                let mut env = config.env.clone().unwrap_or_default();
                env.extend(managed_env.clone());
                PluginMcpServerEntry {
                    name,
                    config: McpServerConfig {
                        env: Some(env),
                        ..config
                    },
                    plugin_id,
                    server_name,
                }
            } else {
                PluginMcpServerEntry {
                    name,
                    config,
                    plugin_id,
                    server_name,
                }
            }
        })
        .collect()
}

fn plugin_info_into_summary(info: PluginInfo) -> PluginSummary {
    PluginSummary {
        id: info.id,
        display_name: info.display_name,
        description: info.description,
        short_description: info.short_description,
        long_description: info.long_description,
        version: info.version,
        enabled: info.enabled,
        state: info.state,
        skill_count: info.skill_count,
        mcp_server_count: info.mcp_server_count,
        enabled_mcp_server_count: info.enabled_mcp_server_count,
        hook_count: info.hook_count,
        command_count: info.command_count,
        has_errors: info.has_errors,
        source: info.source,
        origin: info.origin,
        catalog_id: info.catalog_id,
        marketplace_source: info.marketplace_source,
        original_source: info.original_source,
        github: info.github,
    }
}
