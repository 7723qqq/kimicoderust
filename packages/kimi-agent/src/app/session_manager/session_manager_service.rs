use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use indexmap::IndexMap;
use kimi_core::primitives::errors::{Error2, Error2Options};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::ScopeActivation;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable, to_disposable};
use crate::_base::di::scope::{ISessionScopeHandle, register_scoped_service};
use crate::app::environment::environment::IEnvironmentService;
use crate::app::environment_declaration::environment_declaration::IEnvironmentDeclarationService;
use crate::app::scopes::LifecycleScope;
use crate::app::session_index::session_index::{ISessionIndex, SessionSummary};
use crate::environment::environment::LOCAL_ENVIRONMENT_ID;
use crate::environment::environment_defaults::normalize_remote_cwd;
use crate::features::cron::cron_service::AgentCronService;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_fs_errors::{is_host_fs_not_directory, is_host_fs_not_found};
use crate::persistence::interface::session_lock::{ISessionLockService, SessionLockLease};
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::session_activity::session_activity::{ISessionActivityView, SessionPendingInteraction};
use crate::session::session_metadata::session_metadata::SessionMeta;
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle::{
    CreateChildSessionOptions, CreateSessionOptions, ForkSessionOptions, LifecycleWaitEvent,
    ResumeSessionOptions, SessionArchivedEvent, SessionClosedEvent, SessionCreateSource,
    SessionCreatedEvent, SessionForkedEvent, SessionWillCloseEvent, SessionWillCreateEvent,
};
use crate::workspace::session_lifecycle::session_lifecycle_service::SessionLifecycleService;
use crate::workspace::workspace_instance::workspace_instance_manager::{
    IWorkspaceInstanceManager, WorkspaceInstanceLease, WorkspaceInstanceRef,
};

use super::session_manager::{
    CreateManagedSessionOptions, CreationEnvironment, I_SESSION_MANAGER, ISessionManager,
    SessionLifecycleEvents, SessionLifecycleOutput, SessionLifecycleWork, SessionWillDeleteEvent,
    UnguardedSessionLifecycle,
};

const MAX_RESUME_FAILURES: usize = 64;

struct SessionControllerEntry {
    workspace_id: String,
    workspace_lease: Mutex<Option<WorkspaceInstanceLease>>,
    generation: String,
    controller: Arc<SessionLifecycleService>,
    subscriptions: DisposableStore,
    session_count: AtomicIsize,
    operation_count: AtomicIsize,
}

struct LifecycleListeners<T> {
    next_id: u64,
    listeners: HashMap<u64, Arc<dyn Fn(T) + Send + Sync>>,
    disposed: bool,
}

impl<T> Default for LifecycleListeners<T> {
    fn default() -> Self {
        Self {
            next_id: 0,
            listeners: HashMap::new(),
            disposed: false,
        }
    }
}

struct SessionManagerEvents {
    will_create: Mutex<LifecycleListeners<SessionWillCreateEvent>>,
    did_create: Mutex<LifecycleListeners<LifecycleWaitEvent<SessionCreatedEvent>>>,
    will_close: Mutex<LifecycleListeners<LifecycleWaitEvent<SessionWillCloseEvent>>>,
    did_close: Mutex<LifecycleListeners<SessionClosedEvent>>,
    will_delete: Mutex<LifecycleListeners<LifecycleWaitEvent<SessionWillDeleteEvent>>>,
    did_archive: Mutex<LifecycleListeners<SessionArchivedEvent>>,
    did_fork: Mutex<LifecycleListeners<SessionForkedEvent>>,
}

impl Default for SessionManagerEvents {
    fn default() -> Self {
        Self {
            will_create: Mutex::new(LifecycleListeners::default()),
            did_create: Mutex::new(LifecycleListeners::default()),
            will_close: Mutex::new(LifecycleListeners::default()),
            did_close: Mutex::new(LifecycleListeners::default()),
            will_delete: Mutex::new(LifecycleListeners::default()),
            did_archive: Mutex::new(LifecycleListeners::default()),
            did_fork: Mutex::new(LifecycleListeners::default()),
        }
    }
}

#[derive(Clone, Debug)]
struct SharedResumeFailure {
    message: Arc<String>,
    code: Option<Arc<str>>,
    name: Option<Arc<str>>,
}

impl SharedResumeFailure {
    fn new(error: WireAsyncError) -> Self {
        let error2 = error.downcast_ref::<Error2>();
        Self {
            message: Arc::new(error.to_string()),
            code: error2.map(|error| Arc::<str>::from(error.code().as_str())),
            name: error2.map(|error| Arc::<str>::from(error.name())),
        }
    }

    fn boxed(self) -> WireAsyncError {
        if let Some(code) = self.code {
            let mut error = Error2::with_options(
                code.as_ref(),
                self.message.as_str(),
                Error2Options {
                    name: self.name.as_ref().map(|name| name.to_string()),
                    ..Default::default()
                },
            );
            if let Some(name) = self.name {
                error = error.with_name(name.as_ref());
            }
            return Box::new(error);
        }
        Box::new(self)
    }
}

impl fmt::Display for SharedResumeFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message.as_str())
    }
}

impl Error for SharedResumeFailure {}

type ResumeFuture = Shared<
    BoxFuture<
        'static,
        Result<Option<Arc<ISessionScopeHandle>>, SharedResumeFailure>,
    >,
>;

struct SessionManagerState {
    sessions: HashMap<String, Arc<ISessionScopeHandle>>,
    owners: HashMap<String, Arc<SessionControllerEntry>>,
    pending_resumes: HashMap<String, ResumeFuture>,
    resume_failures: IndexMap<String, SharedResumeFailure>,
    reclaimed: HashSet<String>,
    lifecycle_chains: HashMap<String, Weak<tokio::sync::Mutex<()>>>,
    controllers: HashMap<String, Arc<SessionControllerEntry>>,
    controller_entries: IndexMap<usize, Arc<SessionControllerEntry>>,
    disposed: bool,
}

impl Default for SessionManagerState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            owners: HashMap::new(),
            pending_resumes: HashMap::new(),
            resume_failures: IndexMap::new(),
            reclaimed: HashSet::new(),
            lifecycle_chains: HashMap::new(),
            controllers: HashMap::new(),
            controller_entries: IndexMap::new(),
            disposed: false,
        }
    }
}

struct SessionManagerInner {
    workspaces: Arc<dyn IWorkspaceInstanceManager>,
    index: Arc<dyn ISessionIndex>,
    environment_declarations: Arc<dyn IEnvironmentDeclarationService>,
    environments: Arc<dyn IEnvironmentService>,
    host_fs: Arc<dyn IHostFileSystem>,
    session_locks: Arc<dyn ISessionLockService>,
    state: Mutex<SessionManagerState>,
    events: SessionManagerEvents,
}

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<SessionManagerInner>,
}

struct OpenedCreation {
    workspace_id: String,
    environment_id: Option<String>,
    environment_cwd: Option<String>,
    lease: Option<WorkspaceInstanceLease>,
}

impl Drop for OpenedCreation {
    fn drop(&mut self) {
        if let Some(lease) = self.lease.take() {
            lease.dispose();
        }
    }
}

struct SessionLockGuard(SessionLockLease);

impl Drop for SessionLockGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct ControllerOperationGuard {
    manager: SessionManager,
    entry: Arc<SessionControllerEntry>,
}

impl Drop for ControllerOperationGuard {
    fn drop(&mut self) {
        self.entry.operation_count.fetch_sub(1, Ordering::SeqCst);
        self.manager.retire_entry_if_idle(&self.entry);
    }
}

struct BoxedDisposable(DisposableRef);

impl IDisposable for BoxedDisposable {
    fn dispose(&self) {
        self.0.dispose();
    }
}

struct ManagedUnguardedLifecycle {
    manager: SessionManager,
    session_id: String,
}

#[async_trait]
impl UnguardedSessionLifecycle for ManagedUnguardedLifecycle {
    async fn archive(&self) -> Result<(), WireAsyncError> {
        self.manager.archive_inner(&self.session_id).await
    }

    async fn restore(
        &self,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        self.manager.restore_inner(&self.session_id, options).await
    }
}

impl SessionManager {
    pub fn new(
        workspaces: Arc<dyn IWorkspaceInstanceManager>,
        index: Arc<dyn ISessionIndex>,
        environment_declarations: Arc<dyn IEnvironmentDeclarationService>,
        environments: Arc<dyn IEnvironmentService>,
        host_fs: Arc<dyn IHostFileSystem>,
        session_locks: Arc<dyn ISessionLockService>,
    ) -> Self {
        Self {
            inner: Arc::new(SessionManagerInner {
                workspaces,
                index,
                environment_declarations,
                environments,
                host_fs,
                session_locks,
                state: Mutex::new(SessionManagerState::default()),
                events: SessionManagerEvents::default(),
            }),
        }
    }

    pub fn on_will_create_session(
        &self,
        listener: Arc<dyn Fn(SessionWillCreateEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.will_create, listener)
    }

    pub fn on_did_create_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionCreatedEvent>) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.did_create, listener)
    }

    pub fn on_will_close_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillCloseEvent>) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.will_close, listener)
    }

    pub fn on_did_close_session(
        &self,
        listener: Arc<dyn Fn(SessionClosedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.did_close, listener)
    }

    pub fn on_will_delete_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillDeleteEvent>) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.will_delete, listener)
    }

    pub fn on_did_archive_session(
        &self,
        listener: Arc<dyn Fn(SessionArchivedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.did_archive, listener)
    }

    pub fn on_did_fork_session(
        &self,
        listener: Arc<dyn Fn(SessionForkedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        add_listener(&self.inner.events.did_fork, listener)
    }

    pub async fn resolve_creation_environment(
        &self,
        options: CreateManagedSessionOptions,
    ) -> Result<CreationEnvironment, WireAsyncError> {
        let opened = self.open_creation(&options).await?;
        Ok(CreationEnvironment {
            workspace_id: opened.workspace_id.clone(),
            environment_id: opened.environment_id.clone(),
            environment_cwd: opened.environment_cwd.clone(),
        })
    }

    pub async fn create(
        &self,
        options: CreateManagedSessionOptions,
    ) -> Result<Arc<ISessionScopeHandle>, WireAsyncError> {
        let opened = self.open_creation(&options).await?;
        let mut effective = options.clone();
        if opened.environment_id.is_some() || opened.environment_cwd.is_some() {
            effective.environment_id = opened.environment_id.clone();
            effective.environment_cwd = opened.environment_cwd.clone();
        }
        let workspace_id = opened.workspace_id.clone();
        let work_dir = options.work_dir.clone();
        let this = self.clone();
        let create = move || -> BoxFuture<'static, Result<Arc<ISessionScopeHandle>, WireAsyncError>> {
            async move {
                let entry = this
                    .controller_for_workspace(WorkspaceInstanceRef::WorkspaceId {
                        workspace_id,
                        root: Some(work_dir),
                    })
                    .await?;
                let create_options: CreateSessionOptions = effective.into();
                this.run_controller_operation(entry, move |controller| {
                    async move { controller.create(create_options).await }.boxed()
                })
                .await
            }
            .boxed()
        };
        if let Some(session_id) = options.session_id.clone() {
            self.serialize_lifecycle(&session_id, create).await
        } else {
            create().await
        }
    }

    async fn open_creation(
        &self,
        options: &CreateManagedSessionOptions,
    ) -> Result<OpenedCreation, WireAsyncError> {
        self.inner.environments.ready().await?;
        let declarations = if options.environment_id.as_deref() == Some(LOCAL_ENVIRONMENT_ID) {
            None
        } else {
            Some(self.inner.environment_declarations.declarations().await?)
        };
        let environment_id = options
            .environment_id
            .clone()
            .or_else(|| declarations.as_ref().and_then(|declarations| declarations.default_id.clone()));
        let remote = environment_id
            .as_deref()
            .is_some_and(|environment_id| environment_id != LOCAL_ENVIRONMENT_ID);
        if !remote {
            self.assert_usable_work_dir(&options.work_dir).await?;
        }
        let reference = match &options.workspace_id {
            Some(workspace_id) => WorkspaceInstanceRef::WorkspaceId {
                workspace_id: workspace_id.clone(),
                root: Some(options.work_dir.clone()),
            },
            None => WorkspaceInstanceRef::Root {
                root: options.work_dir.clone(),
            },
        };
        let lease = self.inner.workspaces.acquire(reference).await?;
        let mut lease = Some(lease);
        let workspace_id = lease.as_ref().unwrap().instance.id();
        let resolved = async {
            if !remote {
                return Ok((environment_id, options.environment_cwd.clone()));
            }
            let environment_id = environment_id.as_ref().unwrap();
            if let Some(error) = self
                .inner
                .environment_declarations
                .registration_error(environment_id)
            {
                return Err(error);
            }
            let declared = declarations.as_ref().and_then(|declarations| {
                declarations
                    .entries
                    .iter()
                    .find(|entry| &entry.id == environment_id)
            });
            if declared.is_none() && self.inner.environments.current(environment_id).is_none() {
                return Err(Box::new(Error2::new(
                    "config.invalid",
                    format!("environment \"{environment_id}\" is not declared in [environments]"),
                )) as WireAsyncError);
            }
            let cwd = options.environment_cwd.clone().or_else(|| {
                declared.and_then(|declared| declared.entry.default_cwd().map(str::to_string))
            });
            let Some(cwd) = cwd else {
                return Err(Box::new(Error2::new(
                    "config.invalid",
                    if declared.is_none() {
                        format!("environment \"{environment_id}\" requires a cwd")
                    } else {
                        format!("environment \"{environment_id}\" does not set defaultCwd in [environments]")
                    },
                )) as WireAsyncError);
            };
            let environment_cwd = if declared.is_none() {
                cwd
            } else {
                normalize_remote_cwd(&cwd)?
            };
            Ok((Some(environment_id.clone()), Some(environment_cwd)))
        }
        .await;
        match resolved {
            Ok((environment_id, environment_cwd)) => Ok(OpenedCreation {
                workspace_id,
                environment_id,
                environment_cwd,
                lease: lease.take(),
            }),
            Err(error) => {
                if let Some(lease) = lease.take() {
                    lease.dispose();
                }
                Err(error)
            }
        }
    }

    async fn assert_usable_work_dir(&self, work_dir: &str) -> Result<(), WireAsyncError> {
        let mut stat = match self.inner.host_fs.stat(work_dir).await {
            Ok(stat) => stat,
            Err(error) => {
                if is_host_fs_not_found(&error) || is_host_fs_not_directory(&error) {
                    return Err(Box::new(Error2::new(
                        "fs.path_not_found",
                        format!("workspace root {work_dir} does not exist"),
                    )));
                }
                return Err(error);
            }
        };
        if !stat.is_directory {
            if let Ok(realpath) = self.inner.host_fs.realpath(work_dir).await {
                if let Ok(realpath_stat) = self.inner.host_fs.stat(&realpath).await {
                    stat = realpath_stat;
                }
            }
        }
        if !stat.is_directory {
            return Err(Box::new(Error2::new(
                "fs.path_not_found",
                format!("workspace root {work_dir} is not a directory"),
            )));
        }
        Ok(())
    }

    pub fn resume(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> BoxFuture<'static, Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>> {
        if let Some(inflight) = self.inner.state.lock().pending_resumes.get(session_id).cloned() {
            return async move { inflight.await.map_err(SharedResumeFailure::boxed) }.boxed();
        }
        self.inner.state.lock().resume_failures.shift_remove(session_id);
        let this = self.clone();
        let session_id = session_id.to_string();
        let future_session_id = session_id.clone();
        let future = async move {
            let operation_session_id = future_session_id.clone();
            let operation_manager = this.clone();
            let result = this
                .serialize_lifecycle(&future_session_id, move || {
                    let operation_session_id = operation_session_id.clone();
                    let operation_manager = operation_manager.clone();
                    async move {
                        let Some(entry) = operation_manager
                            .controller_for_session(&operation_session_id)
                            .await?
                        else {
                            return Ok(None);
                        };
                        operation_manager
                            .run_controller_operation(entry, move |controller| {
                                async move { controller.resume(&operation_session_id, options).await }.boxed()
                            })
                            .await
                    }
                    .boxed()
                })
                .await
                .map_err(SharedResumeFailure::new);
            this.inner.state.lock().pending_resumes.shift_remove(&future_session_id);
            if let Err(error) = &result {
                if !this.inner.state.lock().disposed {
                    let mut state = this.inner.state.lock();
                    state
                        .resume_failures
                        .insert(future_session_id.clone(), error.clone());
                    while state.resume_failures.len() > MAX_RESUME_FAILURES {
                        state.resume_failures.shift_remove_index(0);
                    }
                }
            }
            result
        }
        .boxed()
        .shared();
        self.inner
            .state
            .lock()
            .pending_resumes
            .insert(session_id, future.clone());
        tokio::spawn(future.clone());
        async move { future.await.map_err(SharedResumeFailure::boxed) }.boxed()
    }

    pub fn get(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>> {
        self.inner.state.lock().sessions.get(session_id).cloned()
    }

    pub async fn status(&self, session_id: &str) -> Result<Option<SessionSummary>, WireAsyncError> {
        self.inner.index.get(session_id).await
    }

    pub async fn when_resume_settled(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let observed = self.inner.state.lock().resume_failures.get(session_id).cloned();
        let pending = self.inner.state.lock().pending_resumes.get(session_id).cloned();
        if let Some(pending) = pending {
            let _ = pending.await;
        }
        let failure = self
            .inner
            .state
            .lock()
            .resume_failures
            .get(session_id)
            .cloned()
            .or(observed);
        if let Some(failure) = failure {
            return Err(failure.boxed());
        }
        let controller = self
            .inner
            .state
            .lock()
            .owners
            .get(session_id)
            .map(|entry| entry.controller.clone());
        if let Some(controller) = controller {
            controller.when_resume_settled(session_id).await?;
        }
        Ok(())
    }

    async fn serialize_lifecycle<T, F>(&self, session_id: &str, work: F) -> Result<T, WireAsyncError>
    where
        T: Send + 'static,
        F: FnOnce() -> BoxFuture<'static, Result<T, WireAsyncError>> + Send + 'static,
    {
        self.serialize_lifecycle_for_keys(&[session_id.to_string()], work)
            .await
    }

    async fn serialize_lifecycle_for_keys<T, F>(
        &self,
        keys: &[String],
        work: F,
    ) -> Result<T, WireAsyncError>
    where
        T: Send + 'static,
        F: FnOnce() -> BoxFuture<'static, Result<T, WireAsyncError>> + Send + 'static,
    {
        if keys.is_empty() {
            return work().await;
        }
        let locks = {
            let mut state = self.inner.state.lock();
            keys.iter()
                .map(|key| {
                    let lock = state
                        .lifecycle_chains
                        .get(key)
                        .and_then(Weak::upgrade)
                        .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
                    state.lifecycle_chains.insert(key.clone(), Arc::downgrade(&lock));
                    lock
                })
                .collect::<Vec<_>>()
        };
        let mut guards = Vec::with_capacity(locks.len());
        for lock in &locks {
            guards.push(lock.lock().await);
        }
        let result = work().await;
        drop(guards);
        let mut state = self.inner.state.lock();
        for (key, lock) in keys.iter().zip(locks.iter()) {
            if Arc::strong_count(lock) != 1 {
                continue;
            }
            let remove = state
                .lifecycle_chains
                .get(key)
                .is_some_and(|current| Weak::ptr_eq(current, &Arc::downgrade(lock)));
            if remove {
                state.lifecycle_chains.remove(key);
            }
        }
        drop(state);
        result
    }

    fn lifecycle_keys(&self, ids: &[Option<String>]) -> Vec<String> {
        let mut keys = ids.iter().flatten().cloned().collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        keys
    }

    pub async fn with_lifecycle_serialization<T, F, Fut>(
        &self,
        session_id: &str,
        work: F,
    ) -> Result<T, WireAsyncError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn UnguardedSessionLifecycle>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WireAsyncError>> + Send + 'static,
    {
        let unguarded: Arc<dyn UnguardedSessionLifecycle> = Arc::new(ManagedUnguardedLifecycle {
            manager: self.clone(),
            session_id: session_id.to_string(),
        });
        self.serialize_lifecycle(session_id, move || {
            let future = work(unguarded);
            async move { future.await }.boxed()
        })
        .await
    }

    pub fn list(&self) -> Vec<Arc<ISessionScopeHandle>> {
        self.inner.state.lock().sessions.values().cloned().collect()
    }

    pub async fn close(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let this = self.clone();
        let session_id = session_id.to_string();
        self.serialize_lifecycle(&session_id.clone(), move || {
            async move {
                let entry = this.inner.state.lock().owners.get(&session_id).cloned();
                let Some(entry) = entry else {
                    return Ok(());
                };
                let entry = this.retain_controller(&entry);
                this.run_controller_operation(entry, move |controller| {
                    async move { controller.close(&session_id).await }.boxed()
                })
                .await
            }
            .boxed()
        })
        .await
    }

    pub async fn close_if_idle(
        &self,
        session_id: &str,
        idle_timeout_ms: i64,
    ) -> Result<bool, WireAsyncError> {
        let this = self.clone();
        let session_id = session_id.to_string();
        self.serialize_lifecycle(&session_id.clone(), move || {
            async move {
                let (handle, entry) = {
                    let state = this.inner.state.lock();
                    (
                        state.sessions.get(&session_id).cloned(),
                        state.owners.get(&session_id).cloned(),
                    )
                };
                let (Some(handle), Some(entry)) = (handle, entry) else {
                    return Ok(false);
                };
                let entry = this.retain_controller(&entry);
                this.run_controller_operation(entry, move |controller| {
                    async move {
                        let last_active_at = handle.accessor().get::<ISessionActivityView>().last_active_at();
                        if chrono::Utc::now().timestamp_millis() - last_active_at < idle_timeout_ms {
                            return Ok(false);
                        }
                        let lease = match this.inner.session_locks.lease(&session_id) {
                            Ok(lease) => lease,
                            Err(error) => {
                                if error.downcast_ref::<Error2>().is_some_and(|error| {
                                    error.code().as_str() == "session.occupied"
                                }) {
                                    return Ok(false);
                                }
                                return Err(error);
                            }
                        };
                        let _lease = SessionLockGuard(lease);
                        if this.retained(&handle).await? {
                            return Ok(false);
                        }
                        controller.close(&session_id).await?;
                        this.inner.state.lock().reclaimed.insert(session_id);
                        Ok(true)
                    }
                    .boxed()
                })
                .await
            }
            .boxed()
        })
        .await
    }

    async fn retained(&self, handle: &Arc<ISessionScopeHandle>) -> Result<bool, WireAsyncError> {
        let agents = handle.accessor().get::<IAgentLifecycleService>();
        for agent in agents.list() {
            let Some(agent_handle) = agents.handle_of(&agent.agent_id) else {
                continue;
            };
            if !agent_handle.accessor().get::<AgentCronService>().list().is_empty() {
                return Ok(true);
            }
        }
        let activity = handle.accessor().get::<ISessionActivityView>().state();
        Ok(activity.busy || activity.pending_interaction != SessionPendingInteraction::None)
    }

    async fn archive_inner(&self, session_id: &str) -> Result<(), WireAsyncError> {
        if let Some(entry) = self.controller_for_session(session_id).await? {
            self.run_controller_operation(entry, move |controller| {
                let session_id = session_id.to_string();
                async move { controller.archive(&session_id).await }.boxed()
            })
            .await?;
        }
        self.inner.state.lock().reclaimed.remove(session_id);
        Ok(())
    }

    pub async fn archive(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let this = self.clone();
        let session_id = session_id.to_string();
        self.serialize_lifecycle(&session_id.clone(), move || {
            async move { this.archive_inner(&session_id).await }.boxed()
        })
        .await
    }

    async fn restore_inner(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        let Some(entry) = self.controller_for_session(session_id).await? else {
            return Ok(None);
        };
        let session_id = session_id.to_string();
        self.run_controller_operation(entry, move |controller| {
            async move { controller.restore(&session_id, options).await }.boxed()
        })
        .await
    }

    pub async fn restore(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        let this = self.clone();
        let session_id = session_id.to_string();
        self.serialize_lifecycle(&session_id.clone(), move || {
            async move { this.restore_inner(&session_id, options).await }.boxed()
        })
        .await
    }

    pub async fn delete(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let this = self.clone();
        let session_id = session_id.to_string();
        self.serialize_lifecycle(&session_id.clone(), move || {
            async move {
                let Some(entry) = this.controller_for_session(&session_id).await? else {
                    return Err(Box::new(Error2::new(
                        "session.not_found",
                        format!("session {session_id} does not exist"),
                    )) as WireAsyncError);
                };
                this.run_controller_operation(entry, move |controller| {
                    async move {
                        controller.close(&session_id).await?;
                        this.fire_will_delete(&session_id).await?;
                        controller.delete(&session_id).await?;
                        this.inner.state.lock().reclaimed.remove(&session_id);
                        Ok(())
                    }
                    .boxed()
                })
                .await
            }
            .boxed()
        })
        .await
    }

    async fn fire_will_delete(&self, session_id: &str) -> Result<(), WireAsyncError> {
        let cleanups = Arc::new(Mutex::new(Vec::<BoxFuture<'static, Result<(), WireAsyncError>>>::new()));
        let frozen = Arc::new(AtomicBool::new(false));
        let cleanup_sink = cleanups.clone();
        let frozen_sink = frozen.clone();
        let event = LifecycleWaitEvent {
            event: SessionWillDeleteEvent {
                session_id: session_id.to_string(),
            },
            wait_until: Arc::new(move |cleanup| {
                if frozen_sink.load(Ordering::SeqCst) {
                    panic!("waitUntil must be called synchronously");
                }
                cleanup_sink.lock().push(cleanup);
            }),
        };
        fire_listeners(&self.inner.events.will_delete, event);
        frozen.store(true, Ordering::SeqCst);
        let results = join_all(cleanups.lock().drain(..).collect::<Vec<_>>()).await;
        for result in results {
            result?;
        }
        Ok(())
    }

    pub async fn fork(&self, options: ForkSessionOptions) -> Result<SessionMeta, WireAsyncError> {
        let keys = self.lifecycle_keys(&[
            Some(options.source_session_id.clone()),
            options.new_session_id.clone(),
        ]);
        let this = self.clone();
        self.serialize_lifecycle_for_keys(&keys, move || {
            async move {
                let source_session_id = options.source_session_id.clone();
                let Some(entry) = this.controller_for_session(&source_session_id).await? else {
                    return Err(Box::new(Error2::new(
                        "session.not_found",
                        format!("session {source_session_id} does not exist"),
                    )) as WireAsyncError);
                };
                this.run_controller_operation(entry, move |controller| {
                    async move { controller.fork(options).await }.boxed()
                })
                .await
            }
            .boxed()
        })
        .await
    }

    pub async fn create_child(
        &self,
        options: CreateChildSessionOptions,
    ) -> Result<SessionMeta, WireAsyncError> {
        let keys = self.lifecycle_keys(&[
            Some(options.source_session_id.clone()),
            options.new_session_id.clone(),
        ]);
        let this = self.clone();
        self.serialize_lifecycle_for_keys(&keys, move || {
            async move {
                let source_session_id = options.source_session_id.clone();
                let Some(entry) = this.controller_for_session(&source_session_id).await? else {
                    return Err(Box::new(Error2::new(
                        "session.not_found",
                        format!("session {source_session_id} does not exist"),
                    )) as WireAsyncError);
                };
                this.run_controller_operation(entry, move |controller| {
                    async move { controller.create_child(options).await }.boxed()
                })
                .await
            }
            .boxed()
        })
        .await
    }

    pub fn dispose(&self) {
        let entries = {
            let mut state = self.inner.state.lock();
            state.disposed = true;
            let entries = state.controller_entries.values().cloned().collect::<Vec<_>>();
            state.controller_entries.clear();
            state.controllers.clear();
            state.sessions.clear();
            state.owners.clear();
            state.reclaimed.clear();
            state.resume_failures.clear();
            entries
        };
        for entry in entries.into_iter().rev() {
            self.dispose_entry(&entry);
        }
        dispose_listeners(&self.inner.events.will_create);
        dispose_listeners(&self.inner.events.did_create);
        dispose_listeners(&self.inner.events.will_close);
        dispose_listeners(&self.inner.events.did_close);
        dispose_listeners(&self.inner.events.will_delete);
        dispose_listeners(&self.inner.events.did_archive);
        dispose_listeners(&self.inner.events.did_fork);
    }

    async fn controller_for_workspace(
        &self,
        reference: WorkspaceInstanceRef,
    ) -> Result<Arc<SessionControllerEntry>, WireAsyncError> {
        let workspace_lease = self.inner.workspaces.acquire(reference).await?;
        let mut workspace_lease = Some(workspace_lease);
        if self.inner.state.lock().disposed {
            if let Some(lease) = workspace_lease.take() {
                lease.dispose();
            }
            return Err("session manager is disposed".into());
        }
        let workspace = workspace_lease.as_ref().unwrap().instance.clone();
        let workspace_id = workspace.id();
        let generation = workspace.program.session_controller_generation();
        let existing = self.inner.state.lock().controllers.get(&workspace_id).cloned();
        if let Some(existing_entry) = existing.as_ref().filter(|entry| entry.generation == generation) {
            existing_entry.operation_count.fetch_add(1, Ordering::SeqCst);
            if let Some(lease) = workspace_lease.take() {
                lease.dispose();
            }
            return Ok(existing_entry.clone());
        }
        let controller = workspace.program.create_session_controller();
        let entry = Arc::new(SessionControllerEntry {
            workspace_id: workspace_id.clone(),
            workspace_lease: Mutex::new(workspace_lease.take()),
            generation,
            controller: controller.clone(),
            subscriptions: DisposableStore::new(),
            session_count: AtomicIsize::new(0),
            operation_count: AtomicIsize::new(1),
        });
        entry.subscriptions.add(Arc::from(controller.on_will_create_session({
            let this = self.clone();
            Arc::new(move |event| {
                fire_listeners(&this.inner.events.will_create, event);
            })
        })));
        entry.subscriptions.add(Arc::from(controller.on_did_create_session({
            let this = self.clone();
            let entry = entry.clone();
            Arc::new(move |event: LifecycleWaitEvent<SessionCreatedEvent>| {
                entry.session_count.fetch_add(1, Ordering::SeqCst);
                let session_id = event.event.session_id.clone();
                let handle = event.event.handle.clone();
                let mut state = this.inner.state.lock();
                let reclaimed = state.reclaimed.remove(&session_id);
                state.sessions.insert(session_id.clone(), handle);
                state.owners.insert(session_id, entry.clone());
                drop(state);
                let event = if reclaimed {
                    LifecycleWaitEvent {
                        event: SessionCreatedEvent {
                            source: SessionCreateSource::Reclaimed,
                            ..event.event
                        },
                        wait_until: event.wait_until,
                    }
                } else {
                    event
                };
                fire_listeners(&this.inner.events.did_create, event);
            })
        })));
        entry.subscriptions.add(Arc::from(controller.on_will_close_session({
            let this = self.clone();
            Arc::new(move |event| {
                fire_listeners(&this.inner.events.will_close, event);
            })
        })));
        entry.subscriptions.add(Arc::from(controller.on_did_close_session({
            let this = self.clone();
            let entry = entry.clone();
            Arc::new(move |event: SessionClosedEvent| {
                entry.session_count.fetch_sub(1, Ordering::SeqCst);
                {
                    let mut state = this.inner.state.lock();
                    state.sessions.remove(&event.session_id);
                    state.owners.remove(&event.session_id);
                }
                fire_listeners(&this.inner.events.did_close, event);
                this.retire_entry_if_idle(&entry);
            })
        })));
        entry.subscriptions.add(Arc::from(controller.on_did_archive_session({
            let this = self.clone();
            let entry = entry.clone();
            Arc::new(move |event: SessionArchivedEvent| {
                entry.session_count.fetch_sub(1, Ordering::SeqCst);
                {
                    let mut state = this.inner.state.lock();
                    state.sessions.remove(&event.session_id);
                    state.owners.remove(&event.session_id);
                }
                fire_listeners(&this.inner.events.did_archive, event);
                this.retire_entry_if_idle(&entry);
            })
        })));
        entry.subscriptions.add(Arc::from(controller.on_did_fork_session({
            let this = self.clone();
            Arc::new(move |event| {
                fire_listeners(&this.inner.events.did_fork, event);
            })
        })));
        {
            let mut state = self.inner.state.lock();
            let id = entry_id(&entry);
            state.controller_entries.insert(id, entry.clone());
            state.controllers.insert(workspace_id, entry.clone());
        }
        if let Some(existing) = existing {
            self.retire_entry_if_idle(&existing);
        }
        Ok(entry)
    }

    fn retain_controller(&self, entry: &Arc<SessionControllerEntry>) -> Arc<SessionControllerEntry> {
        entry.operation_count.fetch_add(1, Ordering::SeqCst);
        entry.clone()
    }

    async fn run_controller_operation<T, F>(
        &self,
        entry: Arc<SessionControllerEntry>,
        work: F,
    ) -> Result<T, WireAsyncError>
    where
        F: FnOnce(Arc<SessionLifecycleService>) -> BoxFuture<'static, Result<T, WireAsyncError>>,
    {
        let _guard = ControllerOperationGuard {
            manager: self.clone(),
            entry: entry.clone(),
        };
        work(entry.controller.clone()).await
    }

    fn retire_entry_if_idle(&self, entry: &Arc<SessionControllerEntry>) {
        if entry.session_count.load(Ordering::SeqCst) != 0
            || entry.operation_count.load(Ordering::SeqCst) != 0
        {
            return;
        }
        let removed = {
            let mut state = self.inner.state.lock();
            let id = entry_id(entry);
            if !state.controller_entries.contains_key(&id) {
                false
            } else {
                state.controller_entries.shift_remove(&id);
                let current = state.controllers.get(&entry.workspace_id).cloned();
                if current.is_some_and(|current| Arc::ptr_eq(&current, entry)) {
                    state.controllers.remove(&entry.workspace_id);
                }
                true
            }
        };
        if removed {
            self.dispose_entry(entry);
        }
    }

    fn dispose_entry(&self, entry: &Arc<SessionControllerEntry>) {
        entry.subscriptions.dispose();
        if let Some(lease) = entry.workspace_lease.lock().take() {
            lease.dispose();
        }
    }

    async fn controller_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<Arc<SessionControllerEntry>>, WireAsyncError> {
        let live = self.inner.state.lock().owners.get(session_id).cloned();
        if let Some(live) = live {
            return Ok(Some(self.retain_controller(&live)));
        }
        let Some(summary) = self.inner.index.get(session_id).await? else {
            return Ok(None);
        };
        self.inner.environments.ready().await?;
        Ok(Some(
            self.controller_for_workspace(WorkspaceInstanceRef::WorkspaceId {
                workspace_id: summary.workspace_id,
                root: summary.cwd,
            })
            .await?,
        ))
    }
}

impl IDisposable for SessionManager {
    fn dispose(&self) {
        SessionManager::dispose(self);
    }
}

impl SessionLifecycleEvents for SessionManager {
    fn on_did_close_session(
        &self,
        listener: Arc<dyn Fn(SessionClosedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_did_close_session(self, listener)
    }

    fn on_did_archive_session(
        &self,
        listener: Arc<dyn Fn(SessionArchivedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_did_archive_session(self, listener)
    }
}

#[async_trait]
impl ISessionManager for SessionManager {
    fn on_will_create_session(
        &self,
        listener: Arc<dyn Fn(SessionWillCreateEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_will_create_session(self, listener)
    }

    fn on_did_create_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionCreatedEvent>) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_did_create_session(self, listener)
    }

    fn on_will_close_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillCloseEvent>) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_will_close_session(self, listener)
    }

    fn on_will_delete_session(
        &self,
        listener: Arc<dyn Fn(LifecycleWaitEvent<SessionWillDeleteEvent>) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_will_delete_session(self, listener)
    }

    fn on_did_fork_session(
        &self,
        listener: Arc<dyn Fn(SessionForkedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        SessionManager::on_did_fork_session(self, listener)
    }

    async fn resolve_creation_environment(
        &self,
        options: CreateManagedSessionOptions,
    ) -> Result<CreationEnvironment, WireAsyncError> {
        SessionManager::resolve_creation_environment(self, options).await
    }

    async fn create(
        &self,
        options: CreateManagedSessionOptions,
    ) -> Result<Arc<ISessionScopeHandle>, WireAsyncError> {
        SessionManager::create(self, options).await
    }

    async fn resume(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        SessionManager::resume(self, session_id, options).await
    }

    fn get(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>> {
        SessionManager::get(self, session_id)
    }

    async fn status(&self, session_id: &str) -> Result<Option<SessionSummary>, WireAsyncError> {
        SessionManager::status(self, session_id).await
    }

    async fn when_resume_settled(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionManager::when_resume_settled(self, session_id).await
    }

    fn with_lifecycle_serialization(
        &self,
        session_id: &str,
        work: SessionLifecycleWork,
    ) -> BoxFuture<'static, Result<SessionLifecycleOutput, WireAsyncError>> {
        let this = self.clone();
        let session_id = session_id.to_string();
        async move {
            SessionManager::with_lifecycle_serialization(&this, &session_id, move |unguarded| {
                work(unguarded)
            })
            .await
        }
        .boxed()
    }

    fn list(&self) -> Vec<Arc<ISessionScopeHandle>> {
        SessionManager::list(self)
    }

    async fn close(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionManager::close(self, session_id).await
    }

    async fn close_if_idle(
        &self,
        session_id: &str,
        idle_timeout_ms: i64,
    ) -> Result<bool, WireAsyncError> {
        SessionManager::close_if_idle(self, session_id, idle_timeout_ms).await
    }

    async fn archive(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionManager::archive(self, session_id).await
    }

    async fn restore(
        &self,
        session_id: &str,
        options: Option<ResumeSessionOptions>,
    ) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError> {
        SessionManager::restore(self, session_id, options).await
    }

    async fn delete(&self, session_id: &str) -> Result<(), WireAsyncError> {
        SessionManager::delete(self, session_id).await
    }

    async fn fork(&self, options: ForkSessionOptions) -> Result<SessionMeta, WireAsyncError> {
        SessionManager::fork(self, options).await
    }

    async fn create_child(
        &self,
        options: CreateChildSessionOptions,
    ) -> Result<SessionMeta, WireAsyncError> {
        SessionManager::create_child(self, options).await
    }
}

fn entry_id(entry: &Arc<SessionControllerEntry>) -> usize {
    Arc::as_ptr(entry) as usize
}

fn add_listener<T: 'static>(
    listeners: &Arc<Mutex<LifecycleListeners<T>>>,
    listener: Arc<dyn Fn(T) + Send + Sync>,
) -> Box<dyn IDisposable> {
    let id = {
        let mut listeners = listeners.lock();
        if listeners.disposed {
            return Box::new(BoxedDisposable(to_disposable(|| {})));
        }
        let id = listeners.next_id;
        listeners.next_id += 1;
        listeners.listeners.insert(id, listener);
        id
    };
    let listeners = listeners.clone();
    Box::new(BoxedDisposable(to_disposable(move || {
        listeners.lock().listeners.remove(&id);
    })))
}

fn fire_listeners<T: Clone>(listeners: &Arc<Mutex<LifecycleListeners<T>>>, event: T) {
    let snapshot = {
        let listeners = listeners.lock();
        if listeners.disposed {
            return;
        }
        listeners.listeners.values().cloned().collect::<Vec<_>>()
    };
    for listener in snapshot {
        listener(event.clone());
    }
}

fn dispose_listeners<T>(listeners: &Arc<Mutex<LifecycleListeners<T>>>) {
    let mut listeners = listeners.lock();
    listeners.disposed = true;
    listeners.listeners.clear();
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("session manager dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static SESSION_MANAGER_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let manager: Arc<dyn ISessionManager> = Arc::new(SessionManager::new(
            dependency::<dyn IWorkspaceInstanceManager>(&args, 0),
            dependency::<dyn ISessionIndex>(&args, 1),
            dependency::<dyn IEnvironmentDeclarationService>(&args, 2),
            dependency::<dyn IEnvironmentService>(&args, 3),
            dependency::<dyn IHostFileSystem>(&args, 4),
            dependency::<dyn ISessionLockService>(&args, 5),
        ));
        Arc::new(manager) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_SESSION_MANAGER.clone(),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "sessionManager",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_session_manager_registered() -> Result<(), String> {
    Lazy::force(&SESSION_MANAGER_REGISTRATION).clone()
}
