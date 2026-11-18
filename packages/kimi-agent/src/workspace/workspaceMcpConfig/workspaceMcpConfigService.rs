use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::safely_call_listener;
use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::{dirname, subtree_watch_filter};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::mcp_config::config_loader::{load_mcp_servers, resolve_mcp_json_paths};
use crate::app::mcp_config::config_section::MCP_SECTION;
use crate::app::mcp_config::config_store::IMcpConfigStore;
use crate::app::plugin::plugin::IPluginService;
use crate::human::utils::watch::{watch, watch_candidates};
use crate::mcp_core::config_schema::McpServerConfig;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;
use crate::workspace::workspace_trust::workspace_trust::IWorkspaceTrust;

use super::workspace_mcp_config::{
    IWorkspaceMcpConfigService, McpConfigAsyncError, McpConfigFuture, McpServersChange,
    McpServersChangeEvent, McpTunables,
};

const WATCH_DEBOUNCE_MS: u64 = 200;

#[derive(Debug, Clone)]
struct SharedMcpConfigError(Arc<String>);

impl SharedMcpConfigError {
    fn new(error: McpConfigAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> McpConfigAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedMcpConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedMcpConfigError {}

type MutationFuture = Shared<BoxFuture<'static, Result<(), SharedMcpConfigError>>>;
type ChangeListener = Arc<dyn Fn(McpServersChangeEvent) + Send + Sync>;
type TunablesListener = Arc<dyn Fn(McpTunables) + Send + Sync>;

struct McpConfigState {
    mutation_tail: Option<MutationFuture>,
    primary_reload_tail: Option<MutationFuture>,
    file_servers: HashMap<String, McpServerConfig>,
    plugin_servers: HashMap<String, McpServerConfig>,
    current: HashMap<String, McpServerConfig>,
    initialization_complete: bool,
    change_listeners: IndexMap<u64, ChangeListener>,
    tunables_listeners: IndexMap<u64, TunablesListener>,
    next_listener_id: u64,
    project_watcher: Option<DisposableRef>,
    watched_primary: Option<String>,
    watch_generation: u64,
    is_disposed: bool,
}

struct WorkspaceMcpConfigServiceInner {
    dirs: Arc<dyn IWorkspaceDirs>,
    bootstrap: Arc<dyn IBootstrapService>,
    plugins: Arc<dyn IPluginService>,
    log: Arc<dyn ILogService>,
    config: Arc<dyn IConfigService>,
    fs: Arc<dyn IHostFileSystem>,
    trust: Arc<dyn IWorkspaceTrust>,
    state: Arc<Mutex<McpConfigState>>,
    ready: Shared<BoxFuture<'static, ()>>,
    _subscriptions: Mutex<Vec<DisposableRef>>,
}

#[derive(Clone)]
pub struct WorkspaceMcpConfigService {
    inner: Arc<WorkspaceMcpConfigServiceInner>,
}

impl WorkspaceMcpConfigService {
    pub fn new(
        dirs: Arc<dyn IWorkspaceDirs>,
        bootstrap: Arc<dyn IBootstrapService>,
        plugins: Arc<dyn IPluginService>,
        log: Arc<dyn ILogService>,
        config: Arc<dyn IConfigService>,
        fs: Arc<dyn IHostFileSystem>,
        trust: Arc<dyn IWorkspaceTrust>,
        mcp_config_store: Arc<dyn IMcpConfigStore>,
    ) -> Self {
        let state = Arc::new(Mutex::new(McpConfigState {
            mutation_tail: None,
            primary_reload_tail: None,
            file_servers: HashMap::new(),
            plugin_servers: HashMap::new(),
            current: HashMap::new(),
            initialization_complete: false,
            change_listeners: IndexMap::new(),
            tunables_listeners: IndexMap::new(),
            next_listener_id: 0,
            project_watcher: None,
            watched_primary: None,
            watch_generation: 0,
            is_disposed: false,
        }));
        let shell = Self {
            inner: Arc::new_cyclic(|weak| {
                let weak = weak.clone();
                let ready = async move {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let this = Self { inner };
                    if let Err(error) = this.initialize(false).await {
                        this.inner.log.error(
                            "mcp config initial load failed",
                            Some(serde_json::json!({ "error": error.to_string() })),
                        );
                    }
                }
                .boxed()
                .shared();
                WorkspaceMcpConfigServiceInner {
                    dirs,
                    bootstrap,
                    plugins,
                    log,
                    config,
                    fs,
                    trust,
                    state,
                    ready,
                    _subscriptions: Mutex::new(Vec::new()),
                }
            }),
        };
        shell.register_plugins();
        shell.register_trust();
        shell.register_store(mcp_config_store);
        shell.register_config();
        shell.register_dirs();
        shell.spawn_ready();
        let watch_service = shell.clone();
        let watch = async move { watch_service.watch_config_files().await };
        shell.set_primary_tail(watch);
        shell
    }

    fn spawn_ready(&self) {
        tokio::spawn(self.inner.ready.clone());
    }

    fn register_plugins(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.plugins.on_did_reload(Arc::new(move |event: crate::app::plugin::types::PluginReloadEvent| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            event.wait_until(Box::pin(async move {
                if let Err(error) = this.reload_plugin_servers().await {
                    this.inner.log.warn(
                        &format!("mcp plugin reload failed: {error}"),
                        None,
                    );
                }
                Ok(())
            }));
        }));
        self.inner._subscriptions.lock().push(subscription);
    }

    fn register_trust(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.trust.on_did_change(Arc::new(move |_: crate::workspace::workspace_trust::workspace_trust::WorkspaceTrustChange| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            tokio::spawn(async move {
                if let Err(error) = this.reload_after_trust_change().await {
                    this.inner.log.warn(
                        &format!("mcp trust reload failed: {error}"),
                        None,
                    );
                }
            });
        }));
        self.inner._subscriptions.lock().push(subscription);
    }

    fn register_store(&self, store: Arc<dyn IMcpConfigStore>) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = store.on_did_write(Arc::new(move |event: crate::app::mcp_config::config_store::McpConfigWriteEvent| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = Self { inner };
            event.wait_until(Box::pin(async move {
                if let Err(error) = this.reload_file_servers().await {
                    this.inner.log.warn(
                        &format!("mcp config reload after management write failed: {error}"),
                        None,
                    );
                }
                Ok(())
            }));
        }));
        self.inner._subscriptions.lock().push(subscription);
    }

    fn register_config(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.config.on_did_section_change(Box::new(move |event: crate::app::config::config::ConfigSectionChangedEvent| {
            if event.domain != MCP_SECTION {
                return;
            }
            let value = to_mcp_tunables(event.value.as_ref());
            let previous = to_mcp_tunables(event.previous_value.as_ref());
            if value.reconnect_initial_delay_ms == previous.reconnect_initial_delay_ms
                && value.reconnect_max_delay_ms == previous.reconnect_max_delay_ms
            {
                return;
            }
            if let Some(inner) = weak.upgrade() {
                let listeners = inner
                    .state
                    .lock()
                    .tunables_listeners
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                for listener in listeners {
                    safely_call_listener(|| listener(value.clone()));
                }
            }
        }));
        self.inner._subscriptions.lock().push(subscription);
    }

    fn register_dirs(&self) {
        let weak = Arc::downgrade(&self.inner);
        let subscription = self.inner.dirs.on_did_change(Arc::new(move || {
            if let Some(inner) = weak.upgrade() {
                let this = Self { inner };
                let reload_this = this.clone();
                let future = async move {
                    if let Err(error) = reload_this.refresh_primary().await {
                        reload_this.inner.log.warn(
                            &format!("mcp primary reload failed: {error}"),
                            None,
                        );
                        Ok(())
                    } else {
                        Ok(())
                    }
                };
                this.set_primary_tail(future);
            }
        }));
        self.inner._subscriptions.lock().push(subscription);
    }

    fn set_primary_tail<F>(&self, future: F)
    where
        F: std::future::Future<Output = Result<(), McpConfigAsyncError>> + Send + 'static,
    {
        let previous = self.inner.state.lock().primary_reload_tail.clone();
        let tail = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            future.await.map_err(SharedMcpConfigError::new)
        }
        .boxed()
        .shared();
        self.inner.state.lock().primary_reload_tail = Some(tail.clone());
        tokio::spawn(tail);
    }

    pub fn ready(&self) -> McpConfigFuture {
        let ready = self.inner.ready.clone();
        async move {
            ready.await;
            Ok(())
        }
        .boxed()
    }

    pub fn servers(&self) -> HashMap<String, McpServerConfig> {
        self.inner.state.lock().current.clone()
    }

    pub fn tunables(&self) -> McpTunables {
        to_mcp_tunables(self.inner.config.get(MCP_SECTION).as_ref())
    }

    pub fn on_did_change(&self, listener: ChangeListener) -> DisposableRef {
        let id = {
            let mut state = self.inner.state.lock();
            if state.is_disposed {
                drop(state);
                return to_disposable(|| {});
            }
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.change_listeners.insert(id, listener);
            id
        };
        let state = self.inner.state.clone();
        to_disposable(move || {
            state.lock().change_listeners.shift_remove(&id);
        })
    }

    pub fn on_did_change_tunables(&self, listener: TunablesListener) -> DisposableRef {
        let id = {
            let mut state = self.inner.state.lock();
            if state.is_disposed {
                drop(state);
                return to_disposable(|| {});
            }
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.tunables_listeners.insert(id, listener);
            id
        };
        let state = self.inner.state.clone();
        to_disposable(move || {
            state.lock().tunables_listeners.shift_remove(&id);
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
            work.await.map_err(SharedMcpConfigError::new)
        }
        .boxed()
        .shared();
        self.inner.state.lock().mutation_tail = Some(tail.clone());
        tokio::spawn(tail.clone());
        async move { tail.await.map_err(SharedMcpConfigError::boxed) }.boxed()
    }

    async fn initialize(&self, publish: bool) -> Result<(), McpConfigAsyncError> {
        self.inner.config.ready().await?;
        let trusted = self.inner.trust.get().await?;
        let cwd = self.inner.dirs.primary();
        let home_dir = self.inner.bootstrap.home_dir();
        let (file_servers, plugin_servers) = tokio::try_join!(
            load_mcp_servers(
                self.inner.fs.as_ref(),
                &cwd,
                &home_dir,
                trusted,
            ),
            self.inner.plugins.enabled_mcp_servers()
        )?;
        self.inner.state.lock().file_servers = file_servers;
        self.inner.state.lock().plugin_servers = plugin_servers;
        self.inner.state.lock().initialization_complete = true;
        if publish {
            self.publish_if_changed().await?;
        } else {
            let merged = self.merged();
            self.inner.state.lock().current = merged;
        }
        Ok(())
    }

    async fn reload_after_trust_change(&self) -> Result<(), McpConfigAsyncError> {
        self.ready().await?;
        let this = self.clone();
        self.mutate(async move {
            if !this.inner.state.lock().initialization_complete {
                this.initialize(true).await?;
            } else {
                let fresh = load_mcp_servers(
                    this.inner.fs.as_ref(),
                    &this.inner.dirs.primary(),
                    &this.inner.bootstrap.home_dir(),
                    this.inner.trust.is_trusted(),
                )
                .await?;
                this.inner.state.lock().file_servers = fresh;
            }
            this.publish_if_changed().await
        })
        .await
    }

    fn merged(&self) -> HashMap<String, McpServerConfig> {
        let state = self.inner.state.lock();
        let mut merged = state.plugin_servers.clone();
        merged.extend(state.file_servers.clone());
        merged
    }

    async fn refresh_primary(&self) -> Result<(), McpConfigAsyncError> {
        self.inner.dirs.ready().await?;
        if self.inner.state.lock().watched_primary.as_ref()
            == Some(&self.inner.dirs.primary())
        {
            return Ok(());
        }
        let (reload, watcher) = tokio::join!(
            self.reload_file_servers(),
            self.refresh_project_config_watcher()
        );
        reload?;
        watcher
    }

    async fn watch_config_files(&self) -> Result<(), McpConfigAsyncError> {
        let paths = resolve_mcp_json_paths(
            self.inner.fs.as_ref(),
            &self.inner.dirs.primary(),
            &self.inner.bootstrap.home_dir(),
        )
        .await?;
        self.watch_paths(&[paths.user]);
        self.refresh_project_config_watcher().await
    }

    async fn refresh_project_config_watcher(&self) -> Result<(), McpConfigAsyncError> {
        let primary = self.inner.dirs.primary();
        if self.inner.state.lock().watched_primary.as_ref() == Some(&primary) {
            return Ok(());
        }
        let paths = resolve_mcp_json_paths(
            self.inner.fs.as_ref(),
            &primary,
            &self.inner.bootstrap.home_dir(),
        )
        .await?;
        let project_root = dirname(&paths.project_root);
        let handle = watch_candidates(
            &project_root,
            &[paths.project_root.clone(), paths.project.clone()],
            Some(Arc::from(subtree_watch_filter(
                &project_root,
                &[paths.project_root, paths.project],
                None,
            ))),
        );
        let weak = Arc::downgrade(&self.inner);
        let subscription = handle.on_did_change(move |_change| {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.schedule_file_reload();
            }
        });
        let watcher = to_disposable(move || {
            subscription.dispose();
            handle.dispose();
        });
        let previous = self.inner.state.lock().project_watcher.take();
        if let Some(previous) = previous {
            previous.dispose();
        }
        self.inner.state.lock().project_watcher = Some(watcher);
        self.inner.state.lock().watched_primary = Some(primary);
        Ok(())
    }

    fn watch_paths(&self, paths: &[String]) {
        for path in paths {
            let handle = watch(path);
            let weak = Arc::downgrade(&self.inner);
            let subscription = handle.on_did_change(move |_change| {
                if let Some(inner) = weak.upgrade() {
                    Self { inner }.schedule_file_reload();
                }
            });
            self.inner._subscriptions.lock().push(Arc::new(*handle) as DisposableRef);
            self.inner._subscriptions.lock().push(Arc::new(*subscription) as DisposableRef);
        }
    }

    fn schedule_file_reload(&self) {
        let generation = {
            let mut state = self.inner.state.lock();
            state.watch_generation += 1;
            state.watch_generation
        };
        let this = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS)).await;
            if this.inner.state.lock().watch_generation != generation {
                return;
            }
            if let Err(error) = this.reload_file_servers().await {
                this.inner.log.warn(
                    &format!("mcp config reload failed: {error}"),
                    None,
                );
            }
        });
    }

    async fn reload_file_servers(&self) -> Result<(), McpConfigAsyncError> {
        self.ready().await?;
        let this = self.clone();
        self.mutate(async move {
            let fresh = load_mcp_servers(
                this.inner.fs.as_ref(),
                &this.inner.dirs.primary(),
                &this.inner.bootstrap.home_dir(),
                this.inner.trust.is_trusted(),
            )
            .await?;
            this.inner.state.lock().file_servers = fresh;
            this.publish_if_changed().await
        })
        .await
    }

    async fn reload_plugin_servers(&self) -> Result<(), McpConfigAsyncError> {
        self.ready().await?;
        let this = self.clone();
        self.mutate(async move {
            let fresh = this.inner.plugins.enabled_mcp_servers().await?;
            this.inner.state.lock().plugin_servers = fresh;
            this.publish_if_changed().await
        })
        .await
    }

    async fn publish_if_changed(&self) -> Result<(), McpConfigAsyncError> {
        let next = self.merged();
        let mut upsert = HashMap::new();
        let mut remove = Vec::new();
        {
            let state = self.inner.state.lock();
            for (name, config) in &next {
                let changed = state.current.get(name).is_none_or(|previous| {
                    fingerprint_config(previous) != fingerprint_config(config)
                });
                if changed {
                    upsert.insert(name.clone(), config.clone());
                }
            }
            for name in state.current.keys() {
                if !next.contains_key(name) {
                    remove.push(name.clone());
                }
            }
        }
        self.inner.state.lock().current = next;
        if upsert.is_empty() && remove.is_empty() {
            return Ok(());
        }
        let pending = Arc::new(Mutex::new(Vec::<McpConfigFuture>::new()));
        let pending_sink = pending.clone();
        let event = McpServersChangeEvent {
            change: McpServersChange { upsert, remove },
            wait_until: Arc::new(move |future| pending_sink.lock().push(future)),
        };
        let listeners = self
            .inner
            .state
            .lock()
            .change_listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            safely_call_listener(|| listener(event.clone()));
        }
        let futures = pending.lock().drain(..).collect::<Vec<_>>();
        for result in futures_util::future::join_all(futures).await {
            result?;
        }
        Ok(())
    }
}

impl WorkspaceMcpConfigService {
    pub fn dispose(&self) {
        let subscriptions = {
            let mut subscriptions = self.inner._subscriptions.lock();
            std::mem::take(&mut *subscriptions)
        };
        for subscription in subscriptions {
            subscription.dispose();
        }
        let watcher = {
            let mut state = self.inner.state.lock();
            state.is_disposed = true;
            state.watch_generation += 1;
            state.change_listeners.clear();
            state.tunables_listeners.clear();
            state.project_watcher.take()
        };
        if let Some(watcher) = watcher {
            watcher.dispose();
        }
    }
}

impl IWorkspaceMcpConfigService for WorkspaceMcpConfigService {
    fn ready(&self) -> McpConfigFuture {
        WorkspaceMcpConfigService::ready(self)
    }

    fn servers(&self) -> HashMap<String, McpServerConfig> {
        WorkspaceMcpConfigService::servers(self)
    }

    fn tunables(&self) -> McpTunables {
        WorkspaceMcpConfigService::tunables(self)
    }

    fn on_did_change(&self, listener: ChangeListener) -> DisposableRef {
        WorkspaceMcpConfigService::on_did_change(self, listener)
    }

    fn on_did_change_tunables(&self, listener: TunablesListener) -> DisposableRef {
        WorkspaceMcpConfigService::on_did_change_tunables(self, listener)
    }
}

fn fingerprint_config(config: &McpServerConfig) -> String {
    serde_json::to_string(&sort_keys_deep(serde_json::to_value(config).unwrap())).unwrap()
}

fn sort_keys_deep(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(sort_keys_deep).collect()),
        Value::Object(values) => {
            let mut sorted = values
                .into_iter()
                .map(|(key, value)| (key, sort_keys_deep(value)))
                .collect::<Vec<_>>();
            sorted.sort_by(|(left, _), (right, _)| locale_compare(left, right));
            Value::Object(sorted.into_iter().collect::<Map<_, _>>())
        }
        value => value,
    }
}

fn to_mcp_tunables(section: Option<&Value>) -> McpTunables {
    let number = |key: &str| section.and_then(|section| section.get(key)).and_then(Value::as_u64);
    let boolean = |key: &str| section.and_then(|section| section.get(key)).and_then(Value::as_bool);
    McpTunables {
        startup_timeout_ms: number("startupTimeoutMs"),
        tool_timeout_ms: number("toolTimeoutMs"),
        default_deferred: boolean("defaultDeferred"),
        auto_defer_tool_threshold: number("autoDeferToolThreshold"),
        compact_tool_schemas: boolean("compactToolSchemas"),
        reconnect_initial_delay_ms: number("reconnectInitialDelayMs"),
        reconnect_max_delay_ms: number("reconnectMaxDelayMs"),
    }
}

fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    let script = format!(
        "String((function (left, right) {{ return left.localeCompare(right); }})({}, {}))",
        serde_json::to_string(left).unwrap(),
        serde_json::to_string(right).unwrap()
    );
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(script))
            })
        })
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .map(|value| value.cmp(&0))
        .unwrap_or_else(|| left.encode_utf16().cmp(right.encode_utf16()))
}
