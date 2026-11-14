use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::instantiation::{IInstantiationService, LiveRef};
use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::safely_call_listener;
use crate::_base::log::log::ILogService;
use crate::_base::utils::paths::canonical_workspace_root;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::app::agent_profile_catalog::agent_profile_registry::IAgentProfileRegistry;
use crate::app::agent_profile_catalog::builtin_agent_profile_loader::IBuiltinAgentProfileLoader;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::environment::environment::IEnvironmentService;
use crate::app::event::event::IEventService;
use crate::app::flag::flag::IFlagService;
use crate::app::git::git::IGitService;
use crate::app::mcp_config::config_store::IMcpConfigStore;
use crate::app::mcp_config::oauth_service::IMcpOAuthService;
use crate::app::plugin::plugin::IPluginService;
use crate::app::session_index::session_index::{ISessionIndex, ISessionIndexMirror};
use crate::app::session_manager::session_manager::ISessionManager;
use crate::app::state::app_state::IAppStateService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::workspace::workspace::{IWorkspaceService, Workspace};
use crate::app::workspace::workspace_persistence::IWorkspacePersistence;
use crate::features::skill::catalog::builtin_skill_source::IBuiltinSkillSource;
use crate::features::skill::catalog::user_file_skill_source::IUserFileSkillSource;
use crate::llm_adapter::model::model::IModelService;
use crate::llm_adapter::provider::provider::IProviderService;
use crate::mcp_core::oauth::service::McpOAuthService;
use crate::os::interface::host_environment::IHostEnvironment;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::persistence::interface::append_log_store::IAppendLogStore;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::program::program_dependencies::ProgramDependencies;
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle_service::{
    SessionLifecycleService, SessionLifecycleServices,
};
use crate::workspace::workspace_context::workspace_context::{
    IWorkspaceContext, WorkspaceMeta, WorkspaceSource,
};
use crate::workspace::workspace_trust::workspace_trust_store::IWorkspaceTrustStore;
use kimi_core::primitives::errors::Error2;

use super::workspace_instance::WorkspaceInstance;
use super::workspace_instance_manager::{
    IWorkspaceInstanceManager, WorkspaceInstanceChange, WorkspaceInstanceLease,
    WorkspaceInstanceRef, WorkspaceInstancesSnapshot,
};

#[derive(Debug, Clone)]
struct SharedManagerError(Arc<String>);

impl SharedManagerError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedManagerError {}

type WorkspaceFuture = Shared<BoxFuture<'static, Result<Workspace, SharedManagerError>>>;
type InstanceFuture = Shared<BoxFuture<'static, Result<Arc<WorkspaceInstance>, SharedManagerError>>>;
type CloseFuture = Shared<BoxFuture<'static, ()>>;

pub struct WorkspaceInstanceManagerDependencies {
    pub instantiation: Arc<dyn IInstantiationService>,
    pub bootstrap: Arc<dyn IBootstrapService>,
    pub workspaces: Arc<dyn IWorkspaceService>,
    pub environment: Arc<dyn IHostEnvironment>,
    pub host_fs: Arc<dyn IHostFileSystem>,
    pub app_state: Arc<dyn IAppStateService>,
    pub config: Arc<dyn IConfigService>,
    pub event: Arc<dyn IEventService>,
    pub git: LiveRef<IGitService>,
    pub identity: Arc<dyn IAgentIdentity>,
    pub index: Arc<dyn ISessionIndex>,
    pub index_mirror: Arc<dyn ISessionIndexMirror>,
    pub log: Arc<dyn ILogService>,
    pub models: Arc<dyn IModelService>,
    pub oauth: Arc<McpOAuthService>,
    pub config_store: Arc<dyn IMcpConfigStore>,
    pub plugins: Arc<dyn IPluginService>,
    pub model_providers: Arc<dyn IProviderService>,
    pub session_manager: LiveRef<ISessionManager>,
    pub agent_profiles: Arc<dyn IAgentProfileRegistry>,
    pub builtin_agent_profiles: Arc<dyn IBuiltinAgentProfileLoader>,
    pub builtin_skills: Arc<dyn IBuiltinSkillSource>,
    pub user_skills: Arc<dyn IUserFileSkillSource>,
    pub telemetry: Arc<dyn ITelemetryService>,
    pub flags: Arc<dyn IFlagService>,
    pub append_log_store: Arc<dyn IAppendLogStore>,
    pub session_locks: Arc<dyn ISessionLockService>,
    pub docs: Arc<dyn IAtomicDocumentStore>,
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub environments: Arc<dyn IEnvironmentService>,
    pub workspace_store: Arc<dyn IWorkspacePersistence>,
    pub trust_store: Arc<dyn IWorkspaceTrustStore>,
}

struct ManagerState {
    instances: IndexMap<String, Arc<WorkspaceInstance>>,
    requests: HashMap<String, WorkspaceFuture>,
    inflight: HashMap<String, InstanceFuture>,
    closing: HashMap<String, CloseFuture>,
    references: HashMap<String, usize>,
    listeners: IndexMap<u64, Arc<dyn Fn(WorkspaceInstanceChange) + Send + Sync>>,
    next_listener_id: u64,
    disposing: bool,
    disposed: bool,
}

struct WorkspaceInstanceManagerInner {
    dependencies: Arc<WorkspaceInstanceManagerDependencies>,
    state: Arc<Mutex<ManagerState>>,
    _workspace_store_subscription: Mutex<Option<Box<dyn IDisposable>>>,
}

#[derive(Clone)]
pub struct WorkspaceInstanceManager {
    inner: Arc<WorkspaceInstanceManagerInner>,
}

impl WorkspaceInstanceManager {
    pub fn new(dependencies: WorkspaceInstanceManagerDependencies) -> Self {
        let this = Self {
            inner: Arc::new(WorkspaceInstanceManagerInner {
                dependencies: Arc::new(dependencies),
                state: Arc::new(Mutex::new(ManagerState {
                    instances: IndexMap::new(),
                    requests: HashMap::new(),
                    inflight: HashMap::new(),
                    closing: HashMap::new(),
                    references: HashMap::new(),
                    listeners: IndexMap::new(),
                    next_listener_id: 0,
                    disposing: false,
                    disposed: false,
                })),
                _workspace_store_subscription: Mutex::new(None),
            }),
        };
        let weak = Arc::downgrade(&this.inner);
        let subscription = this.inner.dependencies.workspace_store.on_did_change(move || {
            let instances = weak
                .upgrade()
                .map(|inner| inner.state.lock().instances.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for instance in instances {
                let Some(inner) = weak.upgrade() else {
                    continue;
                };
                let workspaces = inner.dependencies.workspaces.clone();
                let log = inner.dependencies.log.clone();
                let inner = inner.clone();
                tokio::spawn(async move {
                    match workspaces.get(&instance.id()).await {
                        Ok(Some(workspace)) => {
                            if inner
                                .state
                                .lock()
                                .instances
                                .get(&instance.id())
                                .is_some_and(|current| Arc::ptr_eq(current, &instance))
                            {
                                instance.update(workspace);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            log.warn(
                                "workspace metadata refresh failed",
                                Some(serde_json::json!({
                                    "workspaceId": instance.id(),
                                    "error": error.to_string(),
                                })),
                            );
                        }
                    }
                });
            }
        });
        *this.inner._workspace_store_subscription.lock() = Some(subscription);
        this
    }

    pub fn get(&self, workspace_id: &str) -> Option<Arc<WorkspaceInstance>> {
        self.inner.state.lock().instances.get(workspace_id).cloned()
    }

    pub fn find_by_root(&self, root: &str) -> Option<Arc<WorkspaceInstance>> {
        let normalized = trim_one_trailing_separator(root);
        self.inner
            .state
            .lock()
            .instances
            .values()
            .find(|instance| trim_one_trailing_separator(&instance.root()) == normalized)
            .cloned()
    }

    pub fn find_containing(&self, cwd: &str) -> Option<Arc<WorkspaceInstance>> {
        let probe = canonical_workspace_root(cwd);
        let mut best = None;
        for instance in self.inner.state.lock().instances.values() {
            let mut dirs = vec![instance.root()];
            dirs.extend(instance.program.dirs().additional_dirs());
            for dir in dirs {
                let root = canonical_workspace_root(&dir);
                let prefix = if root.ends_with('/') { root.clone() } else { format!("{root}/") };
                if probe != root && !probe.starts_with(&prefix) {
                    continue;
                }
                if best.as_ref().is_none_or(|(_, length): &(Arc<WorkspaceInstance>, usize)| {
                    root.len() > *length
                }) {
                    best = Some((instance.clone(), root.len()));
                }
            }
        }
        best.map(|(instance, _)| instance)
    }

    pub fn list(&self) -> Vec<Arc<WorkspaceInstance>> {
        self.inner.state.lock().instances.values().cloned().collect()
    }

    pub fn snapshot(&self) -> WorkspaceInstancesSnapshot {
        WorkspaceInstancesSnapshot {
            workspaces: self.list().iter().map(|instance| instance.snapshot()).collect(),
        }
    }

    pub fn acquire(&self, reference: WorkspaceInstanceRef) -> BoxFuture<'static, Result<WorkspaceInstanceLease, WireAsyncError>> {
        if self.inner.state.lock().disposing {
            return async { Err("workspace instance manager is disposing".into()) }.boxed();
        }
        let key = match &reference {
            WorkspaceInstanceRef::WorkspaceId { workspace_id, .. } => format!("id:{workspace_id}"),
            WorkspaceInstanceRef::Root { root } => {
                format!("root:{}", trim_one_trailing_separator(root))
            }
        };
        let request = self.inner.state.lock().requests.get(&key).cloned();
        let request = match request {
            Some(request) => request,
            None => {
                let this = self.clone();
                let key_for_future = key.clone();
                let pending = async move {
                    let result = this.resolve_workspace(reference).await;
                    this.inner.state.lock().requests.remove(&key_for_future);
                    result.map_err(SharedManagerError::new)
                }
                .boxed()
                .shared();
                self.inner.state.lock().requests.insert(key, pending.clone());
                tokio::spawn(pending.clone());
                pending
            }
        };
        let this = self.clone();
        let completion = tokio::spawn(async move {
            let workspace = request.await.map_err(SharedManagerError::boxed)?;
            this.acquire_workspace(workspace).await
        });
        async move {
            completion
                .await
                .map_err(|error| -> WireAsyncError { Box::new(error) })?
        }
        .boxed()
    }

    async fn resolve_workspace(&self, reference: WorkspaceInstanceRef) -> Result<Workspace, WireAsyncError> {
        let mut workspace = match &reference {
            WorkspaceInstanceRef::WorkspaceId { workspace_id, root } => {
                let mut workspace = self.inner.dependencies.workspaces.get(workspace_id).await?;
                if workspace.is_none() {
                    if let Some(root) = root {
                        workspace = self.inner.dependencies.workspaces.create_or_touch(root).await?;
                    }
                }
                workspace
            }
            WorkspaceInstanceRef::Root { root } => {
                self.inner.dependencies.workspaces.create_or_touch(root).await?
            }
        };
        if workspace.is_none() {
            let label = match reference {
                WorkspaceInstanceRef::WorkspaceId { workspace_id, .. } => workspace_id,
                WorkspaceInstanceRef::Root { root } => root,
            };
            return Err(Box::new(Error2::new(
                "workspace.not_found",
                format!("workspace {label} does not exist"),
            )));
        }
        Ok(workspace.take().unwrap())
    }

    async fn acquire_workspace(&self, workspace: Workspace) -> Result<WorkspaceInstanceLease, WireAsyncError> {
        while !self.inner.state.lock().disposing {
            if let Some(existing) = self.get(&workspace.id) {
                existing.update(workspace.clone());
                return Ok(self.retain(existing));
            }
            let closing = self.inner.state.lock().closing.get(&workspace.id).cloned();
            if let Some(closing) = closing {
                closing.await;
                continue;
            }
            let pending = self.inner.state.lock().inflight.get(&workspace.id).cloned();
            let pending = match pending {
                Some(pending) => pending,
                None => {
                    let this = self.clone();
                    let workspace = workspace.clone();
                    let id = workspace.id.clone();
                    let materialization = async move {
                        let result = this.materialize(workspace).await;
                        this.inner.state.lock().inflight.remove(&id);
                        result.map_err(SharedManagerError::new)
                    }
                    .boxed()
                    .shared();
                    self.inner.state.lock().inflight.insert(workspace.id.clone(), materialization.clone());
                    tokio::spawn(materialization.clone());
                    materialization
                }
            };
            let _ = pending.await.map_err(SharedManagerError::boxed)?;
        }
        Err("workspace instance manager is disposing".into())
    }

    fn retain(&self, instance: Arc<WorkspaceInstance>) -> WorkspaceInstanceLease {
        let id = instance.id();
        *self.inner.state.lock().references.entry(id).or_insert(0) += 1;
        let weak = Arc::downgrade(&self.inner);
        WorkspaceInstanceLease {
            instance,
            dispose: Box::new(move || {
                if let Some(inner) = weak.upgrade() {
                    Self { inner }.release(instance);
                }
            }),
        }
    }

    fn release(&self, instance: Arc<WorkspaceInstance>) {
        let id = instance.id();
        let same = self
            .inner
            .state
            .lock()
            .instances
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &instance));
        if !same {
            return;
        }
        let mut state = self.inner.state.lock();
        let Some(references) = state.references.get(&id).copied() else {
            return;
        };
        if references > 1 {
            state.references.insert(id, references - 1);
            return;
        }
        drop(state);
        let close = self.close(instance);
        tokio::spawn(close);
    }

    fn close(&self, instance: Arc<WorkspaceInstance>) -> CloseFuture {
        let id = instance.id();
        let same = self
            .inner
            .state
            .lock()
            .instances
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, &instance));
        if !same {
            if let Some(closing) = self.inner.state.lock().closing.get(&id).cloned() {
                return closing;
            }
            return async {}.boxed().shared();
        }
        self.inner.state.lock().instances.shift_remove(&id);
        self.inner.state.lock().references.remove(&id);
        let this = self.clone();
        let pending = async move {
            this.dispose_instance(instance.clone()).await;
            this.inner.state.lock().closing.remove(&id);
            this.fire(WorkspaceInstanceChange {
                workspace_id: instance.id(),
                instance: None,
            });
        }
        .boxed()
        .shared();
        self.inner.state.lock().closing.insert(id, pending.clone());
        tokio::spawn(pending.clone());
        pending
    }

    pub async fn dispose(&self) {
        self.inner.state.lock().disposing = true;
        if let Some(subscription) = self.inner._workspace_store_subscription.lock().take() {
            subscription.dispose();
        }
        let futures = {
            let state = self.inner.state.lock();
            let mut futures = Vec::new();
            futures.extend(state.requests.values().cloned().map(|future| async move { let _ = future.await; }.boxed()));
            futures.extend(state.inflight.values().cloned().map(|future| async move { let _ = future.await; }.boxed()));
            futures.extend(state.closing.values().cloned().map(|future| async move { future.await }.boxed()));
            futures
        };
        futures_util::future::join_all(futures).await;
        let instances = self.list().into_iter().rev().collect::<Vec<_>>();
        for instance in instances {
            self.close(instance).await;
        }
        let mut state = self.inner.state.lock();
        state.disposed = true;
        state.listeners.clear();
    }

    async fn materialize(&self, workspace: Workspace) -> Result<Arc<WorkspaceInstance>, WireAsyncError> {
        self.inner.dependencies.environment.ready().await?;
        self.inner.dependencies.environments.ready().await?;
        let context = Arc::new(WorkspaceContextData {
            workspace_id: workspace.id.clone(),
            cwd: workspace.root.clone(),
            meta: WorkspaceMeta {
                id: workspace.id.clone(),
                root: workspace.root.clone(),
                name: workspace.name.clone(),
                kind: workspace.kind.clone(),
                created_at: workspace.created_at,
                last_opened_at: workspace.last_opened_at,
            },
            persistence_scope: format!(
                "{}/{}",
                self.inner.dependencies.bootstrap.scope("sessions"),
                workspace.id
            ),
        });
        let dependencies = self.program_dependencies();
        let instance = Arc::new(WorkspaceInstance::new(
            workspace.clone(),
            self.inner.dependencies.environments.clone(),
            context,
            dependencies,
        ));
        let result = async {
            instance.activate();
            self.inner.state.lock().instances.insert(workspace.id.clone(), instance.clone());
            self.fire(WorkspaceInstanceChange {
                workspace_id: workspace.id.clone(),
                instance: Some(instance.clone()),
            });
            Ok(instance.clone())
        }
        .await;
        if let Err(error) = result {
            self.inner.state.lock().instances.shift_remove(&workspace.id);
            self.inner.state.lock().references.remove(&workspace.id);
            self.dispose_instance(instance).await;
            return Err(error);
        }
        result
    }

    fn program_dependencies(&self) -> ProgramDependencies {
        let dependencies = self.inner.dependencies.clone();
        ProgramDependencies {
            app_state: dependencies.app_state.clone(),
            bootstrap: dependencies.bootstrap.clone(),
            config: dependencies.config.clone(),
            git: dependencies.git.clone(),
            identity: dependencies.identity.clone(),
            log: dependencies.log.clone(),
            oauth: dependencies.oauth.clone(),
            config_store: dependencies.config_store.clone(),
            plugins: dependencies.plugins.clone(),
            session_manager: dependencies.session_manager.clone(),
            agent_profiles: dependencies.agent_profiles.clone(),
            builtin_agent_profiles: dependencies.builtin_agent_profiles.clone(),
            builtin_skills: dependencies.builtin_skills.clone(),
            user_skills: dependencies.user_skills.clone(),
            telemetry: dependencies.telemetry.clone(),
            docs: dependencies.docs.clone(),
            workspaces: dependencies.workspaces.clone(),
            workspace_store: dependencies.workspace_store.clone(),
            trust_store: dependencies.trust_store.clone(),
            create_session_controller: Arc::new(move |input| {
                Arc::new(SessionLifecycleService::new(
                    SessionLifecycleServices {
                        instantiation: dependencies.instantiation.clone(),
                        workspace_context: input.context,
                        bootstrap: dependencies.bootstrap.clone(),
                        config: dependencies.config.clone(),
                        index: dependencies.index.clone(),
                        index_mirror: dependencies.index_mirror.clone(),
                        append_log_store: dependencies.append_log_store.clone(),
                        session_locks: dependencies.session_locks.clone(),
                        docs: dependencies.docs.clone(),
                        storage: dependencies.storage.clone(),
                        log: dependencies.log.clone(),
                        host_fs: dependencies.host_fs.clone(),
                        event: dependencies.event.clone(),
                        telemetry: dependencies.telemetry.clone(),
                        flags: dependencies.flags.clone(),
                        extra_agent_profile_loader: input.extra_agent_profiles,
                        explicit_agent_profile_loader: input.explicit_agent_profiles,
                        user_agent_profile_loader: input.user_agent_profiles,
                        plugin_agent_profile_loader: input.plugin_agent_profiles,
                        workspace_mcp: input.mcp,
                        models: dependencies.models.clone(),
                        providers: dependencies.model_providers.clone(),
                        environments: dependencies.environments.clone(),
                        acquire_workdir: input.acquire_workdir,
                    },
                    input.on_dispose,
                ))
            }),
        }
    }

    async fn dispose_instance(&self, instance: Arc<WorkspaceInstance>) {
        if let Err(error) = instance.dispose().await {
            self.inner.dependencies.log.error(
                "workspace disposal failed",
                Some(serde_json::json!({
                    "workspaceId": instance.id(),
                    "error": error.to_string(),
                })),
            );
        }
    }

    pub fn on_did_change(&self, listener: Arc<dyn Fn(WorkspaceInstanceChange) + Send + Sync>) -> Box<dyn IDisposable> {
        let id = {
            let mut state = self.inner.state.lock();
            if state.disposed {
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

    fn fire(&self, event: WorkspaceInstanceChange) {
        let listeners = self
            .inner
            .state
            .lock()
            .listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            safely_call_listener(|| listener(event.clone()));
        }
    }
}

struct WorkspaceContextData {
    workspace_id: String,
    cwd: String,
    meta: WorkspaceMeta,
    persistence_scope: String,
}

impl IWorkspaceContext for WorkspaceContextData {
    fn workspace_id(&self) -> String {
        self.workspace_id.clone()
    }

    fn cwd(&self) -> String {
        self.cwd.clone()
    }

    fn source(&self) -> WorkspaceSource {
        WorkspaceSource::Local
    }

    fn meta(&self) -> WorkspaceMeta {
        self.meta.clone()
    }

    fn persistence_scope(&self) -> String {
        self.persistence_scope.clone()
    }
}

#[async_trait]
impl IWorkspaceInstanceManager for WorkspaceInstanceManager {
    fn on_did_change(&self, listener: Arc<dyn Fn(WorkspaceInstanceChange) + Send + Sync>) -> Box<dyn IDisposable> {
        WorkspaceInstanceManager::on_did_change(self, listener)
    }

    async fn acquire(&self, reference: WorkspaceInstanceRef) -> Result<WorkspaceInstanceLease, WireAsyncError> {
        WorkspaceInstanceManager::acquire(self, reference).await
    }

    fn get(&self, workspace_id: &str) -> Option<Arc<WorkspaceInstance>> {
        WorkspaceInstanceManager::get(self, workspace_id)
    }

    fn find_by_root(&self, root: &str) -> Option<Arc<WorkspaceInstance>> {
        WorkspaceInstanceManager::find_by_root(self, root)
    }

    fn find_containing(&self, cwd: &str) -> Option<Arc<WorkspaceInstance>> {
        WorkspaceInstanceManager::find_containing(self, cwd)
    }

    fn list(&self) -> Vec<Arc<WorkspaceInstance>> {
        WorkspaceInstanceManager::list(self)
    }

    fn snapshot(&self) -> WorkspaceInstancesSnapshot {
        WorkspaceInstanceManager::snapshot(self)
    }
}

fn trim_one_trailing_separator(path: &str) -> &str {
    path.strip_suffix(['/', '\\']).unwrap_or(path)
}
