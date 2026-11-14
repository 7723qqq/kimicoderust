use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::subtree_watch_filter;
use crate::app::agent_profile_catalog::agent_profile_contribution::AGENT_PROFILE_SOURCE_PRIORITY;
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::human::utils::watch::watch_candidates;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::wire::record::WireAsyncError;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;

use super::internal::agent_file_discovery::discover_agent_files;
use super::internal::agent_profile_from_file::profiles_from_discovery;
use super::internal::agent_profile_loader::{
    AgentProfileLoaderBase, AgentProfileLoaderHooks, AgentProfileProvide,
};
use super::internal::agent_roots::{project_agent_root_candidates, project_agent_roots};
use super::user_agent_profile_loader::IUserAgentProfileLoader;
use super::workspace_agent_profile_loader::IWorkspaceAgentProfileLoader;

const WATCH_DEBOUNCE_MS: u64 = 200;

#[derive(Debug, Clone)]
struct SharedWatchError(Arc<String>);

impl SharedWatchError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedWatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedWatchError {}

type WatchFuture = Shared<BoxFuture<'static, Result<(), SharedWatchError>>>;

struct WatchState {
    watch_ready: Option<WatchFuture>,
    primary_reload_tail: Option<WatchFuture>,
    watchers: Vec<Box<dyn IDisposable>>,
    watched_primary: Option<String>,
    watch_generation: u64,
}

struct WorkspaceAgentProfileLoaderServiceInner {
    base: Arc<AgentProfileLoaderBase>,
    fs: Arc<dyn IHostFileSystem>,
    dirs: Arc<dyn IWorkspaceDirs>,
    log: Arc<dyn ILogService>,
    watch_enabled: bool,
    state: Arc<Mutex<WatchState>>,
    _subscription: Mutex<Option<Box<dyn IDisposable>>>,
}

#[derive(Clone)]
pub struct WorkspaceAgentProfileLoaderService {
    inner: Arc<WorkspaceAgentProfileLoaderServiceInner>,
}

impl WorkspaceAgentProfileLoaderService {
    pub fn new(
        workspace: Arc<dyn IWorkspaceContext>,
        fs: Arc<dyn IHostFileSystem>,
        dirs: Arc<dyn IWorkspaceDirs>,
        log: Arc<dyn ILogService>,
        user: Arc<dyn IUserAgentProfileLoader>,
        registry: Option<Arc<dyn IAgentProfileRegistry>>,
        context_key: Option<String>,
        watch_enabled: bool,
        provide: AgentProfileProvide,
    ) -> Self {
        let state = Arc::new(Mutex::new(WatchState {
            watch_ready: None,
            primary_reload_tail: None,
            watchers: Vec::new(),
            watched_primary: None,
            watch_generation: 0,
        }));
        let load_fs = fs.clone();
        let load_dirs = dirs.clone();
        let load_log = log.clone();
        let load_user = user.clone();
        let load_state = state.clone();
        let base = Arc::new(AgentProfileLoaderBase::new(
            log.clone(),
            registry,
            provide,
            AgentProfileLoaderHooks {
                source_id: "workspace".to_string(),
                priority: AGENT_PROFILE_SOURCE_PRIORITY.workspace,
                fatal: false,
                workspace_key: Some(workspace.workspace_id()),
                context_key,
                load: Arc::new(move || {
                    let fs = load_fs.clone();
                    let dirs = load_dirs.clone();
                    let log = load_log.clone();
                    let user = load_user.clone();
                    let state = load_state.clone();
                    async move {
                        watch_ready(&state).await?;
                        let warn_log = log.clone();
                        let roots = project_agent_roots(
                            fs.as_ref(),
                            &dirs.primary(),
                            Some(&move |message, error| {
                                warn_log.warn(
                                    message,
                                    error.map(|error| {
                                        serde_json::json!({ "error": error.to_string() })
                                    }),
                                );
                            }),
                        )
                        .await?;
                        let discovery = discover_agent_files(
                            fs.as_ref(),
                            &roots,
                            Some(&|message| log.warn(message, None)),
                        )
                        .await?;
                        Ok(profiles_from_discovery(
                            discovery,
                            Arc::new(move |context| {
                                user.get_default_profile().render_system_prompt(context)
                            }),
                        ))
                    }
                    .boxed()
                }),
            },
        ));
        let this = Self {
            inner: Arc::new(WorkspaceAgentProfileLoaderServiceInner {
                base,
                fs,
                dirs,
                log,
                watch_enabled,
                state,
                _subscription: Mutex::new(None),
            }),
        };
        this.set_primary_tail(false);
        let weak = Arc::downgrade(&this.inner);
        let subscription = this.inner.dirs.on_did_change(move || {
            if let Some(inner) = weak.upgrade() {
                Self { inner }.set_primary_tail(true);
            }
        });
        *this.inner._subscription.lock() = Some(subscription);
        this.inner.base.start();
        this
    }

    fn set_primary_tail(&self, reload: bool) {
        let previous = self.inner.state.lock().primary_reload_tail.clone();
        let this = self.clone();
        let tail = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let result = this.refresh_primary(reload).await;
            if reload {
                if let Err(error) = &result {
                    this.inner.log.warn(
                        &format!("agent profile primary reload failed: {error}"),
                        None,
                    );
                }
            }
            result.map_err(SharedWatchError::new)
        }
        .boxed()
        .shared();
        let mut state = self.inner.state.lock();
        state.primary_reload_tail = Some(tail.clone());
        if state.watch_ready.is_none() {
            state.watch_ready = Some(tail.clone());
        }
        drop(state);
        tokio::spawn(tail);
    }

    async fn refresh_primary(&self, reload: bool) -> Result<(), WireAsyncError> {
        self.inner.dirs.ready().await?;
        let primary = self.inner.dirs.primary();
        if self.inner.state.lock().watched_primary.as_ref() == Some(&primary) {
            return Ok(());
        }
        self.refresh_project_agent_roots().await?;
        if reload {
            self.inner.base.reload().await?;
        }
        Ok(())
    }

    async fn refresh_project_agent_roots(&self) -> Result<(), WireAsyncError> {
        let primary = self.inner.dirs.primary();
        if !self.inner.watch_enabled
            || self.inner.state.lock().watched_primary.as_ref() == Some(&primary)
        {
            return Ok(());
        }
        let candidates = project_agent_root_candidates(
            self.inner.fs.as_ref(),
            &primary,
            Some(&|message, error| {
                self.inner.log.warn(
                    message,
                    error.map(|error| serde_json::json!({ "error": error.to_string() })),
                );
            }),
        )
        .await?;
        let handle = watch_candidates(
            &candidates.project_root,
            &candidates.candidates,
            Some(subtree_watch_filter(
                &candidates.project_root,
                &candidates.candidates,
            )),
        );
        let this = self.clone();
        let subscription = handle.on_did_change(move || {
            this.schedule_reload();
        });
        let previous = {
            let mut state = self.inner.state.lock();
            std::mem::take(&mut state.watchers)
        };
        for watcher in previous {
            watcher.dispose();
        }
        let mut state = self.inner.state.lock();
        state.watchers.push(handle);
        state.watchers.push(subscription);
        state.watched_primary = Some(primary);
        Ok(())
    }

    fn schedule_reload(&self) {
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
            if let Err(error) = this.inner.base.reload().await {
                this.inner.log.warn(
                    &format!("agent profile loader \"workspace\" reload failed: {error}"),
                    None,
                );
            }
        });
    }
}

impl WorkspaceAgentProfileLoaderService {
    pub fn dispose(&self) {
        if let Some(subscription) = self.inner._subscription.lock().take() {
            subscription.dispose();
        }
        let watchers = {
            let mut state = self.inner.state.lock();
            state.watch_generation += 1;
            std::mem::take(&mut state.watchers)
        };
        for watcher in watchers {
            watcher.dispose();
        }
        self.inner.base.dispose();
    }
}

impl IWorkspaceAgentProfileLoader for WorkspaceAgentProfileLoaderService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.inner.base.ready()
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.inner.base.reload().boxed()
    }
}

fn watch_ready(state: &Arc<Mutex<WatchState>>) -> BoxFuture<'static, Result<(), WireAsyncError>> {
    let ready = state.lock().watch_ready.clone();
    async move {
        if let Some(ready) = ready {
            ready.await.map_err(SharedWatchError::boxed)
        } else {
            Ok(())
        }
    }
    .boxed()
}
