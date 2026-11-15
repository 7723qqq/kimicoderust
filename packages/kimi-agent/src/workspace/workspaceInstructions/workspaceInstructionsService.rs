use crate::_base::state::state_registry::IStateRegistryExt;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::safely_call_listener;
use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::subtree_watch_filter;
use crate::agent::profile::context::{agents_md_watch_roots, load_agents_md_for_roots};
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::human::utils::watch::{WatchChange, watch_candidates};
use crate::os::interface::host_environment::HostEnvironmentInfo;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::session::session_instructions::instructions_provider::ISessionInstructionsProvider;
use crate::state::state::{StateDefinition, define_state};
use crate::wire::record::WireAsyncError;
use crate::workspace::state::workspace_state::IWorkspaceStateService;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;

use super::workspace_instructions::{
    IWorkspaceInstructionsService, WorkspaceInstructionsSnapshot,
};

const WATCH_DEBOUNCE_MS: u64 = 200;

pub static WORKSPACE_INSTRUCTIONS_CURRENT_KEY: LazyLock<
    StateDefinition<WorkspaceInstructionsSnapshot>,
> = LazyLock::new(|| {
    define_state("workspaceInstructions.current", || {
        WorkspaceInstructionsSnapshot::default()
    })
});

#[derive(Debug, Clone)]
struct SharedInstructionsError(Arc<String>);

impl SharedInstructionsError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedInstructionsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedInstructionsError {}

type ReloadFuture = Shared<BoxFuture<'static, Result<(), SharedInstructionsError>>>;
type InstructionsListener = Arc<dyn Fn(Vec<WatchChange>) + Send + Sync>;

struct InstructionsState {
    reload_tail: Option<ReloadFuture>,
    reloading: Option<ReloadFuture>,
    reload_generation: u64,
    reload_requested: bool,
    loaded: bool,
    pending_changes: IndexMap<String, WatchChange>,
    watchers: Vec<DisposableRef>,
    watched_primary: Option<String>,
    listeners: IndexMap<u64, InstructionsListener>,
    next_listener_id: u64,
    watch_generation: u64,
    dirs_generation: u64,
    is_disposed: bool,
}

struct WorkspaceInstructionsServiceInner {
    dirs: Arc<dyn IWorkspaceDirs>,
    fs: Arc<dyn IHostFileSystem>,
    env: HostEnvironmentInfo,
    bootstrap: Arc<dyn IBootstrapService>,
    log: Arc<dyn ILogService>,
    states: Arc<dyn IWorkspaceStateService>,
    personal_fs: Option<Arc<dyn IHostFileSystem>>,
    watch_enabled: bool,
    state: Arc<Mutex<InstructionsState>>,
    ready: ReloadFuture,
    _dirs_subscription: Mutex<Option<DisposableRef>>,
}

#[derive(Clone)]
pub struct WorkspaceInstructionsService {
    inner: Arc<WorkspaceInstructionsServiceInner>,
}

impl WorkspaceInstructionsService {
    pub fn new(
        dirs: Arc<dyn IWorkspaceDirs>,
        fs: Arc<dyn IHostFileSystem>,
        env: HostEnvironmentInfo,
        bootstrap: Arc<dyn IBootstrapService>,
        log: Arc<dyn ILogService>,
        states: Arc<dyn IWorkspaceStateService>,
        personal_fs: Option<Arc<dyn IHostFileSystem>>,
        watch_enabled: bool,
    ) -> Self {
        states.contribute_state(&WORKSPACE_INSTRUCTIONS_CURRENT_KEY);
        let state = Arc::new(Mutex::new(InstructionsState {
            reload_tail: None,
            reloading: None,
            reload_generation: 0,
            reload_requested: false,
            loaded: false,
            pending_changes: IndexMap::new(),
            watchers: Vec::new(),
            watched_primary: None,
            listeners: IndexMap::new(),
            next_listener_id: 0,
            watch_generation: 0,
            dirs_generation: 0,
            is_disposed: false,
        }));
        let this = Self {
            inner: Arc::new_cyclic(|weak| {
                let weak = weak.clone();
                let ready = async move {
                    let Some(inner) = weak.upgrade() else {
                        return Ok(());
                    };
                    Self { inner }
                        .reload()
                        .await
                        .map_err(SharedInstructionsError::new)
                }
                .boxed()
                .shared();
                WorkspaceInstructionsServiceInner {
                    dirs,
                    fs,
                    env,
                    bootstrap,
                    log,
                    states,
                    personal_fs,
                    watch_enabled,
                    state,
                    ready,
                    _dirs_subscription: Mutex::new(None),
                }
            }),
        };
        let weak = Arc::downgrade(&this.inner);
        let subscription = this.inner.dirs.on_did_change(move || {
            if let Some(inner) = weak.upgrade() {
                let this = Self { inner };
                let generation = {
                    let mut state = this.inner.state.lock();
                    state.dirs_generation += 1;
                    state.dirs_generation
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(0)).await;
                    if this.inner.state.lock().dirs_generation != generation {
                        return;
                    }
                    if let Err(error) = this.reload().await {
                        this.inner.log.warn(
                            &format!("AGENTS.md reload failed: {error}"),
                            None,
                        );
                    }
                });
            }
        });
        *this.inner._dirs_subscription.lock() = Some(subscription);
        tokio::spawn(this.inner.ready.clone());
        this
    }

    fn current(&self) -> WorkspaceInstructionsSnapshot {
        self.inner.states.get(&WORKSPACE_INSTRUCTIONS_CURRENT_KEY)
    }

    fn set_current(&self, value: WorkspaceInstructionsSnapshot) {
        self.inner
            .states
            .set(&WORKSPACE_INSTRUCTIONS_CURRENT_KEY, value);
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let ready = self.inner.ready.clone();
        async move { ready.await.map_err(SharedInstructionsError::boxed) }.boxed()
    }

    pub fn snapshot(&self) -> WorkspaceInstructionsSnapshot {
        self.current()
    }

    pub fn on_did_change(&self, listener: InstructionsListener) -> DisposableRef {
        let id = {
            let mut state = self.inner.state.lock();
            if state.is_disposed {
                drop(state);
                return to_disposable(|| {});
            }
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.listeners.insert(id, listener);
            id
        };
        let state = self.inner.state.clone();
        to_disposable(move || {
            state.lock().listeners.shift_remove(&id);
        })
    }

    pub fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let reloading = self.inner.state.lock().reloading.clone();
        if let Some(reloading) = reloading {
            self.inner.state.lock().reload_requested = true;
            return async move { reloading.await.map_err(SharedInstructionsError::boxed) }.boxed();
        }
        let previous = self.inner.state.lock().reload_tail.clone();
        let generation = {
            let mut state = self.inner.state.lock();
            state.reload_generation += 1;
            state.reload_generation
        };
        let this = self.clone();
        let tail = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            loop {
                this.inner.state.lock().reload_requested = false;
                this.reload_now().await.map_err(SharedInstructionsError::new)?;
                if !this.inner.state.lock().reload_requested {
                    break;
                }
            }
            Ok(())
        }
        .boxed()
        .shared();
        {
            let mut state = self.inner.state.lock();
            state.reloading = Some(tail.clone());
            state.reload_tail = Some(tail.clone());
        }
        let this = self.clone();
        let observed = tail.clone();
        tokio::spawn(async move {
            let _ = observed.await;
            let mut state = this.inner.state.lock();
            if state.reload_generation == generation {
                state.reloading = None;
            }
        });
        tokio::spawn(tail.clone());
        async move { tail.await.map_err(SharedInstructionsError::boxed) }.boxed()
    }

    async fn reload_now(&self) -> Result<(), WireAsyncError> {
        let result = load_agents_md_for_roots(
            self.inner.fs.as_ref(),
            &self.inner.env.home_dir(),
            &self.inner.bootstrap.home_dir(),
            &[self.inner.dirs.primary()],
            self.inner.personal_fs.as_deref(),
        )
        .await?;
        let next = WorkspaceInstructionsSnapshot {
            agents_md: result.content,
            agents_md_warning: result.warning,
            agents_md_paths: result.paths,
        };
        let current = self.current();
        let changed = next.agents_md != current.agents_md
            || next.agents_md_warning != current.agents_md_warning;
        self.set_current(next);
        self.refresh_watchers().await?;
        let (changes, loaded) = {
            let mut state = self.inner.state.lock();
            let changes = state.pending_changes.drain(..).map(|(_, change)| change).collect();
            let loaded = state.loaded;
            state.loaded = true;
            (changes, loaded)
        };
        if changed && loaded {
            let listeners = self
                .inner
                .state
                .lock()
                .listeners
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for listener in listeners {
                safely_call_listener(|| listener(changes.clone()));
            }
        }
        Ok(())
    }

    pub fn session_provider(&self) -> Arc<dyn ISessionInstructionsProvider> {
        Arc::new(WorkspaceSessionInstructionsProvider { service: self.clone() })
    }

    async fn refresh_watchers(&self) -> Result<(), WireAsyncError> {
        if !self.inner.watch_enabled {
            return Ok(());
        }
        let primary = self.inner.dirs.primary();
        if self.inner.state.lock().watched_primary.as_ref() == Some(&primary) {
            return Ok(());
        }
        let plan = agents_md_watch_roots(
            self.inner.fs.as_ref(),
            &self.inner.env.home_dir(),
            &primary,
            &self.inner.bootstrap.home_dir(),
        )
        .await?;
        let previous = {
            let mut state = self.inner.state.lock();
            std::mem::take(&mut state.watchers)
        };
        for watcher in previous {
            watcher.dispose();
        }
        self.inner.state.lock().watched_primary = Some(primary);
        for entry in plan {
            let handle = match watch_candidates(
                &entry.root,
                &entry.candidates,
                Some(subtree_watch_filter(&entry.root, &entry.candidates)),
            ) {
                Ok(handle) => handle,
                Err(error) => {
                    self.inner.log.warn(
                        &format!("cannot watch instruction root {}: {error}", entry.root),
                        None,
                    );
                    continue;
                }
            };
            let this = self.clone();
            let subscription = handle.on_did_change(move |change| {
                this.inner.state.lock().pending_changes.insert(
                    change.path.clone(),
                    change,
                );
                let generation = {
                    let mut state = this.inner.state.lock();
                    state.watch_generation += 1;
                    state.watch_generation
                };
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS)).await;
                    if this.inner.state.lock().watch_generation != generation {
                        return;
                    }
                    if let Err(error) = this.reload().await {
                        this.inner.log.warn(
                            &format!("AGENTS.md reload failed: {error}"),
                            None,
                        );
                    }
                });
            });
            let mut state = self.inner.state.lock();
            state.watchers.push(handle);
            state.watchers.push(subscription);
        }
        Ok(())
    }
}

impl WorkspaceInstructionsService {
    pub fn dispose(&self) {
        if let Some(subscription) = self.inner._dirs_subscription.lock().take() {
            subscription.dispose();
        }
        let watchers = {
            let mut state = self.inner.state.lock();
            state.is_disposed = true;
            state.watch_generation += 1;
            state.dirs_generation += 1;
            state.listeners.clear();
            std::mem::take(&mut state.watchers)
        };
        for watcher in watchers {
            watcher.dispose();
        }
    }
}

impl IWorkspaceInstructionsService for WorkspaceInstructionsService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        WorkspaceInstructionsService::ready(self)
    }

    fn snapshot(&self) -> WorkspaceInstructionsSnapshot {
        WorkspaceInstructionsService::snapshot(self)
    }

    fn on_did_change(&self, listener: InstructionsListener) -> DisposableRef {
        WorkspaceInstructionsService::on_did_change(self, listener)
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        WorkspaceInstructionsService::reload(self)
    }

    fn session_provider(&self) -> Arc<dyn ISessionInstructionsProvider> {
        WorkspaceInstructionsService::session_provider(self)
    }
}

struct WorkspaceSessionInstructionsProvider {
    service: WorkspaceInstructionsService,
}

#[async_trait]
impl ISessionInstructionsProvider for WorkspaceSessionInstructionsProvider {
    async fn ready(&self) -> Result<(), WireAsyncError> {
        self.service.ready().await
    }

    fn on_did_change(&self, listener: InstructionsListener) -> DisposableRef {
        self.service.on_did_change(listener)
    }

    fn agents_md(&self) -> Option<String> {
        self.service.current().agents_md
    }

    fn agents_md_warning(&self) -> Option<String> {
        self.service.current().agents_md_warning
    }

    fn agents_md_paths(&self) -> Option<Vec<String>> {
        self.service.current().agents_md_paths
    }
}
