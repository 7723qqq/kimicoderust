use crate::_base::state::state_registry::IStateRegistryExt;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
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
use crate::_base::utils::workdir_slug::workspace_root_key;
use crate::app::project_local_config::project_local_config::{
    AdditionalDirSource, IProjectLocalConfigService,
};
use crate::app::workspace::workspace::IWorkspaceService;
use crate::app::workspace::workspace_persistence::IWorkspacePersistence;
use crate::environment::environment::LOCAL_ENVIRONMENT_ID;
use crate::human::utils::watch::watch_candidates;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::state::state::{StateDefinition, define_state};
use crate::wire::record::WireAsyncError;
use crate::workspace::state::workspace_state::IWorkspaceStateService;
use crate::workspace::workspace_context::workspace_context::IWorkspaceContext;

use super::workspace_dirs::{
    IWorkspaceDirs, WorkspaceAddDirInput, WorkspaceAdditionalDirsResult,
};

const WATCH_DEBOUNCE_MS: u64 = 200;

pub static WORKSPACE_DIRS_FILE_DIRS_KEY: LazyLock<StateDefinition<Vec<String>>> =
    LazyLock::new(|| define_state("workspaceDirs.fileDirs", Vec::new));
pub static WORKSPACE_DIRS_EPHEMERAL_DIRS_KEY: LazyLock<StateDefinition<Vec<String>>> =
    LazyLock::new(|| define_state("workspaceDirs.ephemeralDirs", Vec::new));
pub static WORKSPACE_DIRS_REGISTRY_DIRS_KEY: LazyLock<StateDefinition<Vec<String>>> =
    LazyLock::new(|| define_state("workspaceDirs.registryDirs", Vec::new));
pub static WORKSPACE_DIRS_PRIMARY_BY_WORKSPACE_KEY: LazyLock<
    StateDefinition<HashMap<String, String>>,
> = LazyLock::new(|| define_state("workspaceDirs.primaryByWorkspace", HashMap::new));

#[derive(Debug, Clone)]
struct SharedDirsError(Arc<String>);

impl SharedDirsError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedDirsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedDirsError {}

type ReadyFuture = Shared<BoxFuture<'static, Result<(), SharedDirsError>>>;
type TailFuture = Shared<BoxFuture<'static, ()>>;

struct DirsState {
    project_root: String,
    config_path: String,
    legacy_project_config: bool,
    mutation_tail: Option<TailFuture>,
    registry_reload_queued: bool,
    change_batch_depth: usize,
    change_pending: bool,
    ephemeral_refs: IndexMap<String, usize>,
    local_config_watcher: Option<DisposableRef>,
    listeners: IndexMap<u64, Arc<dyn Fn() + Send + Sync>>,
    next_listener_id: u64,
    watch_generation: u64,
    is_disposed: bool,
}

struct WorkspaceDirsServiceInner {
    workspace: Arc<dyn IWorkspaceContext>,
    local_config: Arc<dyn IProjectLocalConfigService>,
    log: Arc<dyn ILogService>,
    states: Arc<dyn IWorkspaceStateService>,
    workspaces: Arc<dyn IWorkspaceService>,
    environment_id: String,
    follows_primary: bool,
    state: Arc<Mutex<DirsState>>,
    ready: Mutex<Option<ReadyFuture>>,
    _store_subscription: Mutex<Option<DisposableRef>>,
}

#[derive(Clone)]
pub struct WorkspaceDirsService {
    inner: Arc<WorkspaceDirsServiceInner>,
}

impl WorkspaceDirsService {
    pub fn new(
        workspace: Arc<dyn IWorkspaceContext>,
        local_config: Arc<dyn IProjectLocalConfigService>,
        log: Arc<dyn ILogService>,
        states: Arc<dyn IWorkspaceStateService>,
        workspaces: Arc<dyn IWorkspaceService>,
        workspace_store: Arc<dyn IWorkspacePersistence>,
        environment_id: Option<String>,
        follows_primary: Option<bool>,
    ) -> Self {
        states.contribute_state(&WORKSPACE_DIRS_FILE_DIRS_KEY);
        states.contribute_state(&WORKSPACE_DIRS_EPHEMERAL_DIRS_KEY);
        states.contribute_state(&WORKSPACE_DIRS_REGISTRY_DIRS_KEY);
        states.contribute_state(&WORKSPACE_DIRS_PRIMARY_BY_WORKSPACE_KEY);
        let environment_id = environment_id.unwrap_or_else(|| LOCAL_ENVIRONMENT_ID.to_string());
        let follows_primary = follows_primary.unwrap_or(environment_id == LOCAL_ENVIRONMENT_ID);
        let legacy_project_config = workspace.meta().kind.as_deref() != Some("managed");
        let project_root = workspace.cwd();
        let this = Self {
            inner: Arc::new(WorkspaceDirsServiceInner {
                workspace,
                local_config,
                log,
                states,
                workspaces,
                environment_id,
                follows_primary,
                state: Arc::new(Mutex::new(DirsState {
                    project_root,
                    config_path: String::new(),
                    legacy_project_config,
                    mutation_tail: None,
                    registry_reload_queued: false,
                    change_batch_depth: 0,
                    change_pending: false,
                    ephemeral_refs: IndexMap::new(),
                    local_config_watcher: None,
                    listeners: IndexMap::new(),
                    next_listener_id: 0,
                    watch_generation: 0,
                    is_disposed: false,
                })),
                ready: Mutex::new(None),
                _store_subscription: Mutex::new(None),
            }),
        };
        let ready = this.enqueue_with(|service| async move { service.reload_workspace(true).await });
        *this.inner.ready.lock() = Some(ready.clone());
        tokio::spawn(ready.clone());
        let weak = Arc::downgrade(&this.inner);
        let subscription = workspace_store.on_did_change(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let service = Self { inner };
            if service.inner.state.lock().registry_reload_queued {
                return;
            }
            service.inner.state.lock().registry_reload_queued = true;
            let log = service.inner.log.clone();
            let future = service.enqueue_with(|service| async move {
                service.inner.state.lock().registry_reload_queued = false;
                service.reload_workspace(false).await
            });
            tokio::spawn(async move {
                if let Err(error) = future.await {
                    log.warn(
                        &format!("workspace registry reload failed: {error}"),
                        None,
                    );
                }
            });
        });
        *this.inner._store_subscription.lock() = Some(subscription);
        this
    }

    fn file_dirs(&self) -> Vec<String> {
        self.inner.states.get(&WORKSPACE_DIRS_FILE_DIRS_KEY)
    }

    fn set_file_dirs_state(&self, value: Vec<String>) {
        self.inner.states.set(&WORKSPACE_DIRS_FILE_DIRS_KEY, value);
    }

    fn ephemeral_dirs(&self) -> Vec<String> {
        self.inner.states.get(&WORKSPACE_DIRS_EPHEMERAL_DIRS_KEY)
    }

    fn set_ephemeral_dirs_state(&self, value: Vec<String>) {
        self.inner.states.set(&WORKSPACE_DIRS_EPHEMERAL_DIRS_KEY, value);
    }

    fn registry_dirs(&self) -> Vec<String> {
        self.inner.states.get(&WORKSPACE_DIRS_REGISTRY_DIRS_KEY)
    }

    fn set_registry_dirs_state(&self, value: Vec<String>) {
        self.inner.states.set(&WORKSPACE_DIRS_REGISTRY_DIRS_KEY, value);
    }

    pub fn primary_by_workspace(&self) -> HashMap<String, String> {
        self.inner.states.get(&WORKSPACE_DIRS_PRIMARY_BY_WORKSPACE_KEY)
    }

    fn set_primary_by_workspace(&self, value: HashMap<String, String>) {
        self.inner
            .states
            .set(&WORKSPACE_DIRS_PRIMARY_BY_WORKSPACE_KEY, value);
    }

    pub fn primary(&self) -> String {
        if self.inner.environment_id != LOCAL_ENVIRONMENT_ID || !self.inner.follows_primary {
            return self.inner.workspace.cwd();
        }
        self.primary_by_workspace()
            .get(&self.inner.workspace.workspace_id())
            .cloned()
            .unwrap_or_else(|| self.inner.workspace.meta().root)
    }

    pub fn shared_additional_dirs(&self) -> Vec<String> {
        let mut dirs = self.file_dirs();
        dirs.extend(self.registry_dirs());
        dedup(dirs)
    }

    pub fn additional_dirs(&self) -> Vec<String> {
        let mut dirs = self.shared_additional_dirs();
        dirs.extend(self.ephemeral_dirs());
        dedup(dirs)
    }

    pub fn add_dir(&self, input: WorkspaceAddDirInput) -> BoxFuture<'static, Result<WorkspaceAdditionalDirsResult, WireAsyncError>> {
        self.enqueue_with(move |service| async move { service.apply_add_dir(input).await })
    }

    pub fn merge_additional_dirs(&self, base_dir: &str, dirs: &[String]) -> BoxFuture<'static, Result<DisposableRef, WireAsyncError>> {
        let base_dir = base_dir.to_string();
        let dirs = dirs.to_vec();
        if dirs.is_empty() {
            return async { Ok(Arc::from(to_disposable(|| {}))) }.boxed();
        }
        self.enqueue_with(move |service| async move {
            let resolved = service
                .inner
                .local_config
                .resolve_additional_dirs(&base_dir, &dirs, Some(AdditionalDirSource::Session))
                .await?;
            if service.union_ephemeral(&resolved) {
                service.fire();
            }
            let released = Arc::new(AtomicBool::new(false));
            let released_sink = released.clone();
            let weak = Arc::downgrade(&service.inner);
            Ok(Arc::from(to_disposable(move || {
                if released_sink.swap(true, Ordering::Relaxed) {
                    return;
                }
                if let Some(inner) = weak.upgrade() {
                    let service = Self { inner };
                    if service.release_ephemeral(&resolved) {
                        service.fire();
                    }
                }
                let _ = released;
            })))
        })
    }

    fn release_ephemeral(&self, dirs: &[String]) -> bool {
        let before = self.additional_dirs();
        let mut state = self.inner.state.lock();
        for dir in dirs {
            let Some(count) = state.ephemeral_refs.get(dir).copied() else {
                continue;
            };
            if count <= 1 {
                state.ephemeral_refs.remove(dir);
            } else {
                state.ephemeral_refs.insert(dir.clone(), count - 1);
            }
        }
        let dirs = state.ephemeral_refs.keys().cloned().collect::<Vec<_>>();
        drop(state);
        self.set_ephemeral_dirs_state(dirs);
        !same_string_list(&before, &self.additional_dirs())
    }

    pub fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        self.enqueue_with(|service| async move { service.reload_workspace(true).await })
    }

    async fn reload_workspace(&self, reload_local: bool) -> Result<(), WireAsyncError> {
        self.batch_changes(move |service| async move {
            let previous_primary = service.primary();
            service.reload_from_registry().await?;
            if !service.inner.state.lock().legacy_project_config {
                return Ok(());
            }
            let primary_changed = previous_primary != service.primary();
            let watcher_missing = service.inner.state.lock().local_config_watcher.is_none();
            if reload_local || primary_changed || watcher_missing {
                service.reload_from_disk().await?;
            }
            if primary_changed || service.inner.state.lock().local_config_watcher.is_none() {
                service.watch_local_toml();
            }
            Ok(())
        })
        .await
    }

    pub fn session_info(&self) -> Arc<dyn ISessionWorkspaceInfo> {
        Arc::new(WorkspaceSessionInfo { service: self.clone() })
    }

    async fn apply_add_dir(&self, input: WorkspaceAddDirInput) -> Result<WorkspaceAdditionalDirsResult, WireAsyncError> {
        let persist = input.persist.unwrap_or(true);
        if persist {
            let persisted = self
                .inner
                .local_config
                .append_additional_dir(&self.primary(), &input.path)
                .await?;
            self.inner.state.lock().project_root = persisted.project_root.clone();
            self.inner.state.lock().config_path = persisted.config_path.clone();
            if self.set_file_dirs(persisted.additional_dirs.clone()) {
                self.fire();
            }
            return Ok(WorkspaceAdditionalDirsResult {
                project_root: persisted.project_root,
                config_path: persisted.config_path,
                additional_dirs: self.additional_dirs(),
                persisted: true,
            });
        }
        let on_disk = self
            .inner
            .local_config
            .read_additional_dirs(&self.primary())
            .await?;
        self.inner.state.lock().project_root = on_disk.project_root.clone();
        self.inner.state.lock().config_path = on_disk.config_path.clone();
        let resolved = self
            .inner
            .local_config
            .resolve_additional_dirs(&self.primary(), &[input.path], None)
            .await?;
        if self.union_ephemeral(&resolved) {
            self.fire();
        }
        Ok(WorkspaceAdditionalDirsResult {
            project_root: on_disk.project_root,
            config_path: on_disk.config_path,
            additional_dirs: self.additional_dirs(),
            persisted: false,
        })
    }

    async fn reload_from_disk(&self) -> Result<(), WireAsyncError> {
        let on_disk = self
            .inner
            .local_config
            .read_additional_dirs(&self.primary())
            .await?;
        self.inner.state.lock().project_root = on_disk.project_root.clone();
        self.inner.state.lock().config_path = on_disk.config_path.clone();
        if self.set_file_dirs(on_disk.additional_dirs) {
            self.fire_change();
        }
        Ok(())
    }

    async fn reload_from_registry(&self) -> Result<(), WireAsyncError> {
        let registered = self.inner.workspaces.list().await?;
        let current_id = self.inner.workspace.workspace_id();
        let current = if registered.iter().any(|entry| entry.id == current_id) {
            None
        } else {
            self.inner.workspaces.get(&current_id).await?
        };
        let entries = match current {
            Some(current) => vec![current]
                .into_iter()
                .chain(registered)
                .collect::<Vec<_>>(),
            None => registered,
        };
        let cwd_key = workspace_root_key(&self.inner.workspace.cwd());
        let mut dirs = Vec::new();
        let mut primary = HashMap::new();
        for entry in entries {
            let mut set = vec![entry.root.clone()];
            set.extend(entry.additional_dirs.clone());
            let current_entry = self.inner.follows_primary
                && self.inner.environment_id == LOCAL_ENVIRONMENT_ID
                && entry.id == current_id;
            if !current_entry && !set.iter().any(|dir| workspace_root_key(dir) == cwd_key) {
                continue;
            }
            primary.insert(entry.id.clone(), entry.root.clone());
            let primary_key = workspace_root_key(&entry.root);
            for dir in set {
                let key = workspace_root_key(&dir);
                if self.inner.follows_primary && current_entry && key == primary_key {
                    continue;
                }
                if (!self.inner.follows_primary || !current_entry) && key == cwd_key {
                    continue;
                }
                if dirs.iter().any(|kept| workspace_root_key(kept) == key) {
                    continue;
                }
                dirs.push(dir);
            }
        }
        let before_dirs = self.shared_additional_dirs();
        let before_primary = self.primary_by_workspace();
        self.set_registry_dirs_state(dirs);
        self.set_primary_by_workspace(primary.clone());
        if !same_string_list(&before_dirs, &self.shared_additional_dirs())
            || !same_primary_by_workspace(&before_primary, &primary)
        {
            self.fire_change();
        }
        Ok(())
    }

    fn set_file_dirs(&self, dirs: Vec<String>) -> bool {
        let before = self.shared_additional_dirs();
        self.set_file_dirs_state(dirs);
        !same_string_list(&before, &self.shared_additional_dirs())
    }

    fn union_ephemeral(&self, dirs: &[String]) -> bool {
        let before = self.additional_dirs();
        {
            let mut state = self.inner.state.lock();
            for dir in dirs {
                *state.ephemeral_refs.entry(dir.clone()).or_insert(0) += 1;
            }
        }
        let dirs = self
            .inner
            .state
            .lock()
            .ephemeral_refs
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.set_ephemeral_dirs_state(dirs);
        !same_string_list(&before, &self.additional_dirs())
    }

    fn watch_local_toml(&self) {
        let (project_root, config_path) = {
            let state = self.inner.state.lock();
            (state.project_root.clone(), state.config_path.clone())
        };
        let handle = match watch_candidates(
            &project_root,
            &[config_path.clone()],
            Some(subtree_watch_filter(&project_root, &[config_path.clone()])),
        ) {
            Ok(handle) => handle,
            Err(error) => {
                self.inner.log.warn(
                    &format!("cannot watch project-local config {config_path}: {error}"),
                    None,
                );
                return;
            }
        };
        let this = self.clone();
        let subscription = handle.on_did_change(move || {
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
                let future = this.enqueue_with(|service| async move { service.reload_from_disk().await });
                if let Err(error) = future.await {
                    this.inner.log.warn(
                        &format!("local.toml reload failed: {error}"),
                        None,
                    );
                }
            });
        });
        let watcher = to_disposable(move || {
            handle.dispose();
            subscription.dispose();
        });
        let previous = {
            let mut state = self.inner.state.lock();
            if state.is_disposed {
                drop(state);
                watcher.dispose();
                return;
            }
            state.local_config_watcher.replace(watcher)
        };
        if let Some(previous) = previous {
            previous.dispose();
        }
    }

    fn fire_change(&self) {
        let mut state = self.inner.state.lock();
        if state.change_batch_depth > 0 {
            state.change_pending = true;
            return;
        }
        drop(state);
        self.fire();
    }

    async fn batch_changes<F>(&self, work: F) -> Result<(), WireAsyncError>
    where
        F: FnOnce(Self) -> BoxFuture<'static, Result<(), WireAsyncError>>,
    {
        self.inner.state.lock().change_batch_depth += 1;
        let result = work(self.clone()).await;
        let should_fire = {
            let mut state = self.inner.state.lock();
            state.change_batch_depth -= 1;
            if state.change_batch_depth == 0 && state.change_pending {
                state.change_pending = false;
                true
            } else {
                false
            }
        };
        if should_fire {
            self.fire();
        }
        result
    }

    fn enqueue_with<T, F>(&self, work: F) -> BoxFuture<'static, Result<T, WireAsyncError>>
    where
        T: Clone + Send + 'static,
        F: FnOnce(Self) -> BoxFuture<'static, Result<T, WireAsyncError>> + Send + 'static,
    {
        let previous = self.inner.state.lock().mutation_tail.clone();
        let service = self.clone();
        let run = async move {
            if let Some(previous) = previous {
                previous.await;
            }
            work(service).await.map_err(SharedDirsError::new)
        }
        .boxed()
        .shared();
        let tail = {
            let run = run.clone();
            async move {
                let _ = run.await;
            }
            .boxed()
            .shared()
        };
        self.inner.state.lock().mutation_tail = Some(tail.clone());
        tokio::spawn(run.clone());
        tokio::spawn(tail);
        async move { run.await.map_err(SharedDirsError::boxed) }.boxed()
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let ready = self.inner.ready.lock().clone();
        async move {
            if let Some(ready) = ready {
                ready.await.map_err(SharedDirsError::boxed)
            } else {
                Ok(())
            }
        }
        .boxed()
    }

    pub fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef {
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

    fn fire(&self) {
        let listeners = {
            let state = self.inner.state.lock();
            if state.is_disposed {
                return;
            }
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            safely_call_listener(|| listener());
        }
    }
}

impl WorkspaceDirsService {
    pub fn dispose(&self) {
        if let Some(subscription) = self.inner._store_subscription.lock().take() {
            subscription.dispose();
        }
        let watcher = {
            let mut state = self.inner.state.lock();
            state.is_disposed = true;
            state.watch_generation += 1;
            state.listeners.clear();
            state.local_config_watcher.take()
        };
        if let Some(watcher) = watcher {
            watcher.dispose();
        }
    }
}

impl IWorkspaceDirs for WorkspaceDirsService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        WorkspaceDirsService::ready(self)
    }

    fn primary(&self) -> String {
        WorkspaceDirsService::primary(self)
    }

    fn additional_dirs(&self) -> Vec<String> {
        WorkspaceDirsService::additional_dirs(self)
    }

    fn shared_additional_dirs(&self) -> Vec<String> {
        WorkspaceDirsService::shared_additional_dirs(self)
    }

    fn primary_by_workspace(&self) -> HashMap<String, String> {
        WorkspaceDirsService::primary_by_workspace(self)
    }

    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef {
        WorkspaceDirsService::on_did_change(self, listener)
    }

    fn add_dir(&self, input: WorkspaceAddDirInput) -> BoxFuture<'static, Result<WorkspaceAdditionalDirsResult, WireAsyncError>> {
        WorkspaceDirsService::add_dir(self, input)
    }

    fn merge_additional_dirs(&self, base_dir: &str, dirs: &[String]) -> BoxFuture<'static, Result<DisposableRef, WireAsyncError>> {
        WorkspaceDirsService::merge_additional_dirs(self, base_dir, dirs)
    }

    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        WorkspaceDirsService::reload(self)
    }

    fn session_info(&self) -> Arc<dyn ISessionWorkspaceInfo> {
        WorkspaceDirsService::session_info(self)
    }
}

struct WorkspaceSessionInfo {
    service: WorkspaceDirsService,
}

#[async_trait]
impl ISessionWorkspaceInfo for WorkspaceSessionInfo {
    async fn ready(&self) -> Result<(), WireAsyncError> {
        self.service.ready().await
    }

    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef {
        self.service.on_did_change(listener)
    }

    fn additional_dirs(&self) -> Vec<String> {
        self.service.additional_dirs()
    }

    fn primary_by_workspace(&self) -> HashMap<String, String> {
        self.service.primary_by_workspace()
    }
}

fn same_string_list(a: &[String], b: &[String]) -> bool {
    a == b
}

fn same_primary_by_workspace(a: &HashMap<String, String>, b: &HashMap<String, String>) -> bool {
    a == b
}

fn dedup(dirs: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    dirs.into_iter().filter(|dir| seen.insert(dir.clone())).collect()
}

