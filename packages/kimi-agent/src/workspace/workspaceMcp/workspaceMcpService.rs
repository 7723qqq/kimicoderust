use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;

use crate::_base::di::instantiation::LiveRef;
use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::log::log::ILogService;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::environment::environment::{EnvironmentResolver, IEnvironmentService};
use crate::app::session_manager::session_manager::ISessionManager;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::mcp_core::config_schema::McpServerConfig;
use crate::mcp_core::connection_manager::{
    McpConnectionManager, McpConnectionManagerOptions, McpConnectionView, McpDefaults,
    McpServerEntry, McpServerStatus,
};
use crate::mcp_core::oauth::service::{McpOAuthEvent, McpOAuthService};
use crate::mcp_core::oauth::store::canonical_mcp_oauth_resource;
use crate::session::mcp::ephemeral_mcp_servers::ISessionEphemeralMcpServers;
use crate::session::mcp::merged_connection_view::MergedMcpConnectionView;
use crate::session::mcp::session_mcp_handle::ISessionMcpHandle;
use crate::session::session_context::session_context::ISessionContext;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;
use crate::workspace::workspace_mcp_config::workspace_mcp_config::{
    IWorkspaceMcpConfigService, McpConfigAsyncError, McpConfigFuture, McpServersChange,
    McpTunables,
};

use super::workspace_mcp::{
    ISessionMcpOverlay, IWorkspaceMcpService, SessionMcpOverlayOptions,
};

#[derive(Debug, Clone)]
struct SharedMcpError(Arc<String>);

impl SharedMcpError {
    fn new(error: McpConfigAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> McpConfigAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedMcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedMcpError {}

type UnitFuture = Shared<BoxFuture<'static, ()>>;
type MutationFuture = Shared<BoxFuture<'static, Result<(), SharedMcpError>>>;
type ShutdownFuture = Shared<BoxFuture<'static, Result<(), SharedMcpError>>>;

struct SessionMcpHandleImpl {
    ready: UnitFuture,
    connection_manager: Arc<dyn McpConnectionView>,
    is_baseline_server: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

#[async_trait]
impl ISessionMcpHandle for SessionMcpHandleImpl {
    async fn ready(&self) -> Result<(), crate::wire::record::WireAsyncError> {
        self.ready.clone().await;
        Ok(())
    }

    fn connection_manager(&self) -> Arc<dyn McpConnectionView> {
        self.connection_manager.clone()
    }

    fn is_baseline_server(&self, name: &str) -> bool {
        (self.is_baseline_server)(name)
    }
}

struct SessionMcpOverlayImpl {
    handle: Arc<dyn ISessionMcpHandle>,
    manager: Arc<McpConnectionManager>,
    _oauth_subscription: Mutex<Option<Box<dyn IDisposable>>>,
}

#[async_trait]
impl ISessionMcpOverlay for SessionMcpOverlayImpl {
    fn handle(&self) -> Arc<dyn ISessionMcpHandle> {
        self.handle.clone()
    }

    async fn shutdown(&self) -> Result<(), McpConfigAsyncError> {
        if let Some(subscription) = self._oauth_subscription.lock().take() {
            subscription.dispose();
        }
        self.manager.shutdown().await
    }
}

struct WorkspaceMcpState {
    applied_primary: Option<String>,
    mutation_tail: Option<MutationFuture>,
    session_lifecycle_attached: bool,
    shutdown: Option<ShutdownFuture>,
}

struct WorkspaceMcpServiceInner {
    manager: Arc<McpConnectionManager>,
    oauth_service: Arc<McpOAuthService>,
    workspace_id: String,
    dirs: Arc<dyn IWorkspaceDirs>,
    environment_resolver: Arc<dyn EnvironmentResolver>,
    mcp_config: Arc<dyn IWorkspaceMcpConfigService>,
    log: Arc<dyn ILogService>,
    telemetry: Arc<dyn ITelemetryService>,
    identity: Arc<dyn IAgentIdentity>,
    session_lifecycle: LiveRef<ISessionManager>,
    state: Arc<Mutex<WorkspaceMcpState>>,
    ready: UnitFuture,
    _subscriptions: Mutex<Vec<Box<dyn IDisposable>>>,
}

#[derive(Clone)]
pub struct WorkspaceMcpService {
    inner: Arc<WorkspaceMcpServiceInner>,
}

impl WorkspaceMcpService {
    pub fn new(
        workspace: Arc<dyn IWorkspaceContext>,
        dirs: Arc<dyn IWorkspaceDirs>,
        environment_resolver: Arc<dyn EnvironmentResolver>,
        mcp_config: Arc<dyn IWorkspaceMcpConfigService>,
        oauth_service: Arc<McpOAuthService>,
        log: Arc<dyn ILogService>,
        telemetry: Arc<dyn ITelemetryService>,
        identity: Arc<dyn IAgentIdentity>,
        session_lifecycle: LiveRef<ISessionManager>,
    ) -> Self {
        let manager_dirs = dirs.clone();
        let manager_config = mcp_config.clone();
        let manager_identity = identity.clone();
        let manager = Arc::new(McpConnectionManager::new(McpConnectionManagerOptions {
            log: Some(log.clone()),
            oauth_service: Some(oauth_service.clone()),
            telemetry: Some(telemetry.clone()),
            stdio_cwd: Some(Arc::new(move || manager_dirs.primary())),
            environment_resolver: Some(environment_resolver.clone()),
            environment_id: Some("local".to_string()),
            reconnect_retry_enabled: Some(true),
            resolve_defaults: Some(Arc::new(move || {
                mcp_defaults(manager_config.tunables())
            })),
            resolve_client_name: Some(Arc::new(move || {
                manager_identity.current().slug
            })),
            ..Default::default()
        }));
        let state = Arc::new(Mutex::new(WorkspaceMcpState {
            applied_primary: None,
            mutation_tail: None,
            session_lifecycle_attached: false,
            shutdown: None,
        }));
        let this = Self {
            inner: Arc::new_cyclic(|weak| {
                let weak = weak.clone();
                let ready = async move {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let this = Self { inner };
                    if let Err(error) = this.initialize().await {
                        this.inner.log.error(
                            "mcp initial load failed",
                            Some(serde_json::json!({ "error": error.to_string() })),
                        );
                    }
                }
                .boxed()
                .shared();
                WorkspaceMcpServiceInner {
                    manager,
                    oauth_service,
                    workspace_id: workspace.workspace_id(),
                    dirs,
                    environment_resolver,
                    mcp_config,
                    log,
                    telemetry,
                    identity,
                    session_lifecycle,
                    state,
                    ready,
                    _subscriptions: Mutex::new(Vec::new()),
                }
            }),
        };
        this.register_config();
        this.register_dirs();
        let oauth_subscription = this.oauth_event_subscription(this.inner.manager.clone());
        this.inner._subscriptions.lock().push(oauth_subscription);
        this.attach_session_lifecycle();
        this.register_lifecycle_ref();
        tokio::spawn(this.inner.ready.clone());
        this
    }

    pub fn dispose(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            if let Err(error) = this.dispose_async().await {
                this.inner.log.error(
                    "mcp disposal failed",
                    Some(serde_json::json!({ "error": error.to_string() })),
                );
            }
        });
    }

    pub fn dispose_async(&self) -> McpConfigFuture {
        if let Some(shutdown) = self.inner.state.lock().shutdown.clone() {
            return async move { shutdown.await.map_err(SharedMcpError::boxed) }.boxed();
        }
        let this = self.clone();
        let shutdown = async move {
            let subscriptions = {
                let mut subscriptions = this.inner._subscriptions.lock();
                std::mem::take(&mut *subscriptions)
            };
            for subscription in subscriptions {
                subscription.dispose();
            }
            this.inner.manager.shutdown().await.map_err(SharedMcpError::new)
        }
        .boxed()
        .shared();
        self.inner.state.lock().shutdown = Some(shutdown.clone());
        tokio::spawn(shutdown.clone());
        async move { shutdown.await.map_err(SharedMcpError::boxed) }.boxed()
    }

    fn register_config(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.mcp_config.on_did_change(move |event| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            let change = event.change.clone();
            (event.wait_until)(Box::pin(this.schedule_apply(change)));
        });
        self.inner._subscriptions.lock().push(subscription);
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.mcp_config.on_did_change_tunables(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.manager.reschedule_retries();
            }
        });
        self.inner._subscriptions.lock().push(subscription);
    }

    fn register_dirs(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.dirs.on_did_change(move || {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.schedule_primary_rebind();
            }
        });
        self.inner._subscriptions.lock().push(subscription);
    }

    fn register_lifecycle_ref(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.session_lifecycle.on_did_change(move || {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.attach_session_lifecycle();
            }
        });
        self.inner._subscriptions.lock().push(subscription);
    }

    fn attach_session_lifecycle(&self) {
        if self.inner.state.lock().session_lifecycle_attached {
            return;
        }
        let Some(lifecycle) = self.inner.session_lifecycle.current() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let Some(subscription) = lifecycle.on_will_create_session(move |event| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            let context = event.read_seed::<ISessionContext>();
            if context.workspace_id() != this.inner.workspace_id {
                return;
            }
            let servers = event.read_seed::<ISessionEphemeralMcpServers>();
            if servers.is_empty() {
                return;
            }
            let overlay = this.session_overlay(
                servers,
                Some(SessionMcpOverlayOptions {
                    stdio_cwd: Some(this.inner.dirs.primary()),
                    session_id: Some(context.session_id()),
                }),
            );
            event.contribute_seed::<ISessionMcpHandle>(overlay.handle());
            event.on_session_dispose(move || {
                tokio::spawn(async move {
                    let _ = overlay.shutdown().await;
                });
            });
        }) else {
            return;
        };
        self.inner.state.lock().session_lifecycle_attached = true;
        self.inner._subscriptions.lock().push(subscription);
    }

    pub fn ready(&self) -> McpConfigFuture {
        unit_result(self.inner.ready.clone())
    }

    pub fn connection_manager(&self) -> Arc<McpConnectionManager> {
        self.inner.manager.clone()
    }

    pub fn session_handle(&self) -> Arc<dyn ISessionMcpHandle> {
        Arc::new(SessionMcpHandleImpl {
            ready: self.inner.ready.clone(),
            connection_manager: self.inner.manager.clone(),
            is_baseline_server: self.session_baseline(
                self.inner.manager.clone(),
                self.inner.ready.clone(),
                None,
            ),
        })
    }

    pub fn session_overlay(
        &self,
        servers: HashMap<String, McpServerConfig>,
        opts: Option<SessionMcpOverlayOptions>,
    ) -> Arc<dyn ISessionMcpOverlay> {
        let opts = opts.unwrap_or_default();
        let overlay_dirs = self.inner.dirs.clone();
        let cwd = opts.stdio_cwd.clone();
        let overlay_config = self.inner.mcp_config.clone();
        let overlay_identity = self.inner.identity.clone();
        let session_manager = Arc::new(McpConnectionManager::new(McpConnectionManagerOptions {
            log: Some(self.inner.log.clone()),
            oauth_service: Some(self.inner.oauth_service.clone()),
            telemetry: Some(self.inner.telemetry.clone()),
            stdio_cwd: Some(Arc::new(move || {
                cwd.clone().unwrap_or_else(|| overlay_dirs.primary())
            })),
            environment_resolver: Some(self.inner.environment_resolver.clone()),
            environment_id: Some("local".to_string()),
            require_stdio_environment_id: Some(true),
            reconnect_retry_enabled: Some(true),
            session_id: opts.session_id.clone(),
            resolve_defaults: Some(Arc::new(move || {
                mcp_defaults(overlay_config.tunables())
            })),
            resolve_client_name: Some(Arc::new(move || {
                overlay_identity.current().slug
            })),
            ..Default::default()
        }));
        let connect_manager = session_manager.clone();
        let connect_servers = servers.clone();
        let config_ready = self.inner.mcp_config.ready();
        let identity_ready = self.inner.identity.resolved();
        let log = self.inner.log.clone();
        let connect = async move {
            if let Err(error) = async move {
                let (config, identity) = tokio::join!(config_ready, identity_ready);
                config?;
                identity?;
                connect_manager.connect_all(connect_servers).await
            }
            .await
            {
                log.error(
                    "session mcp overlay initial load failed",
                    Some(serde_json::json!({ "error": error.to_string() })),
                );
            }
        }
        .boxed()
        .shared();
        tokio::spawn(connect.clone());
        let oauth_subscription = self.oauth_event_subscription(session_manager.clone());
        let view = Arc::new(MergedMcpConnectionView::new(
            self.inner.manager.clone(),
            session_manager.clone(),
            servers.keys().cloned().collect(),
        ));
        let ready = {
            let base = self.inner.ready.clone();
            let connect = connect.clone();
            async move {
                base.await;
                connect.await;
            }
            .boxed()
            .shared()
        };
        tokio::spawn(ready.clone());
        let handle = Arc::new(SessionMcpHandleImpl {
            ready: ready.clone(),
            connection_manager: view,
            is_baseline_server: self.session_baseline(
                self.inner.manager.clone(),
                ready,
                Some(servers.keys().cloned().collect()),
            ),
        });
        Arc::new(SessionMcpOverlayImpl {
            handle,
            manager: session_manager,
            _oauth_subscription: Mutex::new(Some(oauth_subscription)),
        })
    }

    fn oauth_event_subscription(
        &self,
        manager: Arc<McpConnectionManager>,
    ) -> Box<dyn IDisposable> {
        let weak = Arc::downgrade(&self.inner);
        self.inner.oauth_service.on_event(move |event| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            let manager = manager.clone();
            tokio::spawn(async move {
                if let Err(error) = this.handle_mcp_oauth_event(manager, event).await {
                    this.inner.log.warn(
                        &format!("mcp oauth event handling failed: {error}"),
                        None,
                    );
                }
            });
        })
    }

    async fn handle_mcp_oauth_event(
        &self,
        manager: Arc<McpConnectionManager>,
        event: McpOAuthEvent,
    ) -> Result<(), McpConfigAsyncError> {
        if event.type_ == "tokens-invalidated" && event.scope != "tokens" && event.scope != "all" {
            return Ok(());
        }
        let Some(entry) = manager.get(&event.server_name) else {
            return Ok(());
        };
        let Some(server_url) = manager.get_remote_server_url(&event.server_name) else {
            return Ok(());
        };
        if canonical_mcp_oauth_resource(&server_url) != event.server_url {
            return Ok(());
        }
        if event.type_ == "tokens-invalidated" {
            self.inner
                .oauth_service
                .forget_provider(&event.server_name, &event.server_url);
            if entry.status == McpServerStatus::NeedsAuth {
                return Ok(());
            }
        }
        if matches!(entry.status, McpServerStatus::Disabled | McpServerStatus::Removed) {
            return Ok(());
        }
        if entry.status == McpServerStatus::Pending {
            return self.wait_pending_reconnect(manager, event.server_name).await;
        }
        if event.type_ == "tokens-saved"
            && !matches!(entry.status, McpServerStatus::NeedsAuth | McpServerStatus::Failed)
        {
            return Ok(());
        }
        if event.type_ == "refresh-failed" && entry.status != McpServerStatus::Connected {
            return Ok(());
        }
        manager.reconnect_and_join(&event.server_name).await
    }

    async fn wait_pending_reconnect(
        &self,
        manager: Arc<McpConnectionManager>,
        server_name: String,
    ) -> Result<(), McpConfigAsyncError> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = Arc::new(Mutex::new(Some(sender)));
        let subscription = Arc::new(Mutex::new(None::<Box<dyn IDisposable>>));
        let sender_sink = sender.clone();
        let subscription_sink = subscription.clone();
        let manager_sink = manager.clone();
        let name = server_name.clone();
        let handle = manager.on_status_change(move |next| {
            if sender_sink.lock().is_none() {
                return;
            }
            if next.name != name || next.status == McpServerStatus::Pending {
                return;
            }
            let Some(sender) = sender_sink.lock().take() else {
                return;
            };
            if let Some(subscription) = subscription_sink.lock().take() {
                subscription.dispose();
            }
            let terminal = matches!(next.status, McpServerStatus::Disabled | McpServerStatus::Removed);
            if terminal {
                let _ = sender.send(Ok(()));
                return;
            }
            let manager = manager_sink.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let _ = sender.send(manager.reconnect_after_current(&name).await);
            });
        });
        *subscription.lock() = Some(handle);
        let initial = manager.get(&server_name);
        if initial.as_ref().is_none_or(|entry| entry.status != McpServerStatus::Pending) {
            if let Some(sender) = sender.lock().take() {
                let terminal = initial.as_ref().is_none_or(|entry| {
                    matches!(entry.status, McpServerStatus::Disabled | McpServerStatus::Removed)
                });
                if terminal {
                    let _ = sender.send(Ok(()));
                } else {
                    tokio::spawn(async move {
                        let _ = sender.send(manager.reconnect_after_current(&server_name).await);
                    });
                }
            }
        }
        receiver.await.unwrap_or(Ok(()))
    }

    fn session_baseline(
        &self,
        view: Arc<dyn McpConnectionView>,
        ready: UnitFuture,
        extra: Option<HashSet<String>>,
    ) -> Arc<dyn Fn(&str) -> bool + Send + Sync> {
        struct Baseline {
            names: Option<HashSet<String>>,
            frozen: bool,
        }
        let state = Arc::new(Mutex::new(Baseline {
            names: None,
            frozen: false,
        }));
        let snapshot_state = state.clone();
        let snapshot_view = view.clone();
        let snapshot_extra = extra.clone();
        let snapshot = Arc::new(move || {
            let mut state = snapshot_state.lock();
            if state.names.is_none() {
                let mut names = snapshot_extra.clone().unwrap_or_default();
                for entry in snapshot_view.list() {
                    names.insert(entry.name);
                }
                state.names = Some(names);
            }
        });
        let ready_state = state.clone();
        let ready_snapshot = snapshot.clone();
        tokio::spawn(async move {
            ready.await;
            ready_snapshot();
            ready_state.lock().frozen = true;
        });
        Arc::new(move |name| {
            snapshot();
            let mut state = state.lock();
            let frozen = state.frozen;
            let names = state.names.as_mut().unwrap();
            if names.contains(name) {
                return true;
            }
            if frozen || view.get(name).is_none() {
                return false;
            }
            names.insert(name.to_string());
            true
        })
    }

    fn mutate<F>(&self, work: F) -> McpConfigFuture
    where
        F: std::future::Future<Output = Result<(), McpConfigAsyncError>> + Send + 'static,
    {
        let previous = self.inner.state.lock().mutation_tail.clone();
        let tail = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            work.await.map_err(SharedMcpError::new)
        }
        .boxed()
        .shared();
        self.inner.state.lock().mutation_tail = Some(tail.clone());
        tokio::spawn(tail.clone());
        async move { tail.await.map_err(SharedMcpError::boxed) }.boxed()
    }

    fn schedule_primary_rebind(&self) {
        let this = self.clone();
        tokio::spawn(async move {
            let result = async move {
                this.ready().await?;
                let this_mutate = this.clone();
                this.mutate(async move {
                    let primary = this_mutate.inner.dirs.primary();
                    if this_mutate.inner.state.lock().applied_primary.as_ref() == Some(&primary) {
                        return Ok(());
                    }
                    for (name, config) in this_mutate.inner.mcp_config.servers() {
                        if config.transport.as_str() != "stdio"
                            || config.cwd.is_some()
                            || config.enabled == Some(false)
                            || this_mutate.inner.manager.get(&name).is_none()
                        {
                            continue;
                        }
                        if let Err(error) = this_mutate.inner.manager.reconnect(&name).await {
                            this_mutate.inner.log.warn(
                                &format!("mcp primary reconnect failed for {name}: {error}"),
                                None,
                            );
                        }
                    }
                    this_mutate.inner.state.lock().applied_primary = Some(primary);
                    Ok(())
                })
                .await
            }
            .await;
            if let Err(error) = result {
                this.inner.log.warn(
                    &format!("mcp primary reconnect failed: {error}"),
                    None,
                );
            }
        });
    }

    async fn initialize(&self) -> Result<(), McpConfigAsyncError> {
        let config_ready = self.inner.mcp_config.ready();
        let identity_ready = self.inner.identity.resolved();
        let (config, identity) = tokio::join!(config_ready, identity_ready);
        config?;
        identity?;
        self.inner.state.lock().applied_primary = Some(self.inner.dirs.primary());
        let servers = self.inner.mcp_config.servers();
        if servers.is_empty() {
            return Ok(());
        }
        self.inner.manager.connect_all(servers).await?;
        self.track_mcp_initial_load();
        Ok(())
    }

    fn schedule_apply(&self, change: McpServersChange) -> McpConfigFuture {
        let this = self.clone();
        async move {
            let result = async move {
                this.ready().await?;
                let this_apply = this.clone();
                this.mutate(async move { this_apply.apply(change).await }).await
            }
            .await;
            if let Err(error) = result {
                this.inner.log.warn(
                    &format!("mcp server change apply failed: {error}"),
                    None,
                );
            }
            Ok(())
        }
        .boxed()
    }

    async fn apply(&self, change: McpServersChange) -> Result<(), McpConfigAsyncError> {
        for name in change.remove {
            self.inner.manager.mark_removed(&name).await?;
        }
        for (name, config) in change.upsert {
            self.inner.manager.connect(&name, config).await?;
        }
        Ok(())
    }

    fn track_mcp_initial_load(&self) {
        let entries = self
            .inner
            .manager
            .list()
            .into_iter()
            .filter(|entry| entry.status != McpServerStatus::Disabled)
            .collect::<Vec<_>>();
        let total_count = entries.len();
        if total_count == 0 {
            return;
        }
        let connected_count = entries
            .iter()
            .filter(|entry| entry.status == McpServerStatus::Connected)
            .count();
        if connected_count > 0 {
            let mut properties = serde_json::Map::new();
            properties.insert("server_count".to_string(), connected_count.into());
            properties.insert("total_count".to_string(), total_count.into());
            self.inner.telemetry.track2("mcp_connected", Some(properties));
        }
        let failed_count = entries
            .iter()
            .filter(|entry| entry.status == McpServerStatus::Failed)
            .count();
        if failed_count > 0 {
            let mut properties = serde_json::Map::new();
            properties.insert("failed_count".to_string(), failed_count.into());
            properties.insert("total_count".to_string(), total_count.into());
            self.inner.telemetry.track2("mcp_failed", Some(properties));
        }
    }
}

impl IWorkspaceMcpService for WorkspaceMcpService {
    fn ready(&self) -> McpConfigFuture {
        WorkspaceMcpService::ready(self)
    }

    fn connection_manager(&self) -> Arc<McpConnectionManager> {
        WorkspaceMcpService::connection_manager(self)
    }

    fn session_handle(&self) -> Arc<dyn ISessionMcpHandle> {
        WorkspaceMcpService::session_handle(self)
    }

    fn session_overlay(
        &self,
        servers: HashMap<String, McpServerConfig>,
        opts: Option<SessionMcpOverlayOptions>,
    ) -> Arc<dyn ISessionMcpOverlay> {
        WorkspaceMcpService::session_overlay(self, servers, opts)
    }
}

fn unit_result(future: UnitFuture) -> McpConfigFuture {
    async move {
        future.await;
        Ok(())
    }
    .boxed()
}

fn mcp_defaults(tunables: McpTunables) -> McpDefaults {
    McpDefaults {
        startup_timeout_ms: tunables.startup_timeout_ms,
        tool_timeout_ms: tunables.tool_timeout_ms,
        default_deferred: tunables.default_deferred,
        auto_defer_tool_threshold: tunables.auto_defer_tool_threshold,
        compact_tool_schemas: tunables.compact_tool_schemas,
        reconnect_initial_delay_ms: tunables.reconnect_initial_delay_ms,
        reconnect_max_delay_ms: tunables.reconnect_max_delay_ms,
    }
}
