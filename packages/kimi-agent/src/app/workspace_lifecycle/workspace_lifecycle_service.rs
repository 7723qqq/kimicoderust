use std::collections::HashMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    ScopeActivation, ServiceIdentifier, ServicesAccessor, refine_service_decorator,
};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::register_scoped_service;
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::event::Emitter;
use crate::app::scopes::LifecycleScope;
use crate::app::session_manager::session_manager::ISessionManager;
use crate::session::session_context::session_context::{ISessionContext, I_SESSION_CONTEXT};
use crate::wire::record::WireAsyncError;
use crate::workspace::session_lifecycle::session_lifecycle::{
    ISessionLifecycleService, I_SESSION_LIFECYCLE_SERVICE,
};
use crate::workspace::session_lifecycle::session_lifecycle_service::SessionLifecycleService;
use crate::workspace::workspace_instance::workspace_instance::WorkspaceInstance;
use crate::workspace::workspace_instance::workspace_instance_manager::{
    IWorkspaceInstanceManager, WorkspaceInstanceLease, WorkspaceInstanceRef,
};

use super::workspace_lifecycle::{
    I_WORKSPACE_LIFECYCLE_SERVICE, IWorkspaceLifecycleService, IWorkspaceScopeHandle,
    WorkspaceHandlerRegistry, WorkspaceRef, WorkspaceSessionRegistry,
};

struct WorkspaceLifecycleInner {
    manager: Arc<dyn IWorkspaceInstanceManager>,
    session_manager: Arc<dyn ISessionManager>,
    controllers: Mutex<HashMap<String, Arc<SessionLifecycleService>>>,
    handles: Mutex<HashMap<String, Arc<WorkspaceScopeHandle>>>,
    leases: Mutex<HashMap<String, WorkspaceInstanceLease>>,
    on_did_materialize_handler: Emitter<Arc<dyn IWorkspaceScopeHandle>>,
    manager_subscription: Mutex<Option<DisposableRef>>,
}

impl WorkspaceLifecycleInner {
    fn handle_of(self: &Arc<Self>, instance: Arc<WorkspaceInstance>) -> Arc<WorkspaceScopeHandle> {
        let id = instance.id();
        let mut handles = self.handles.lock();
        handles
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(WorkspaceScopeHandle {
                    id,
                    accessor: Arc::new(WorkspaceScopeAccessor {
                        inner: Arc::downgrade(self),
                        instance,
                    }),
                    inner: Arc::downgrade(self),
                })
            })
            .clone()
    }

    fn controller_of(&self, instance: &Arc<WorkspaceInstance>) -> Arc<SessionLifecycleService> {
        let id = instance.id();
        let mut controllers = self.controllers.lock();
        controllers
            .entry(id)
            .or_insert_with(|| instance.program.create_session_controller())
            .clone()
    }

    fn dispose_handle(&self, id: &str) {
        self.handles.lock().remove(id);
        let controller = self.controllers.lock().remove(id);
        drop(controller);
        if let Some(lease) = self.leases.lock().remove(id) {
            lease.dispose();
        }
    }

    fn dispose(self: &Arc<Self>) {
        if let Some(subscription) = self.manager_subscription.lock().take() {
            subscription.dispose();
        }
        self.on_did_materialize_handler.dispose();
        let handles = self.handles.lock().values().cloned().collect::<Vec<_>>();
        for handle in handles {
            handle.dispose();
        }
    }
}

struct WorkspaceScopeAccessor {
    inner: Weak<WorkspaceLifecycleInner>,
    instance: Arc<WorkspaceInstance>,
}

impl ServicesAccessor for WorkspaceScopeAccessor {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue {
        if id == &refine_service_decorator(&I_SESSION_LIFECYCLE_SERVICE) {
            let inner = self
                .inner
                .upgrade()
                .expect("workspace lifecycle service is disposed");
            let controller: Arc<dyn ISessionLifecycleService> =
                inner.controller_of(&self.instance);
            return Arc::new(controller) as DynamicValue;
        }
        std::panic::panic_any(BugIndicatingError::new(format!(
            "workspace handler of {} only resolves ISessionLifecycleService",
            self.instance.id()
        )));
    }
}

struct WorkspaceScopeHandle {
    id: String,
    accessor: Arc<WorkspaceScopeAccessor>,
    inner: Weak<WorkspaceLifecycleInner>,
}

impl IWorkspaceScopeHandle for WorkspaceScopeHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn accessor(&self) -> Arc<dyn ServicesAccessor> {
        self.accessor.clone()
    }

    fn dispose(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.dispose_handle(&self.id);
        }
    }
}

struct WorkspaceHandlerRegistryImpl {
    inner: Weak<WorkspaceLifecycleInner>,
}

impl WorkspaceHandlerRegistry for WorkspaceHandlerRegistryImpl {
    fn list(&self) -> Vec<Arc<dyn IWorkspaceScopeHandle>> {
        let inner = self
            .inner
            .upgrade()
            .expect("workspace lifecycle service is disposed");
        inner
            .manager
            .list()
            .into_iter()
            .map(|instance| inner.handle_of(instance) as Arc<dyn IWorkspaceScopeHandle>)
            .collect()
    }
}

struct WorkspaceSessionRegistryImpl {
    inner: Weak<WorkspaceLifecycleInner>,
}

impl WorkspaceSessionRegistry for WorkspaceSessionRegistryImpl {
    fn list(&self, workspace_id: &str) -> Vec<String> {
        let inner = self
            .inner
            .upgrade()
            .expect("workspace lifecycle service is disposed");
        inner
            .session_manager
            .list()
            .into_iter()
            .filter(|handle| {
                let value = handle
                    .accessor()
                    .get_dynamic(&refine_service_decorator(&I_SESSION_CONTEXT));
                let context = Arc::downcast::<Arc<dyn ISessionContext>>(value)
                    .expect("service type does not match its identifier");
                context.workspace_id() == workspace_id
            })
            .map(|handle| handle.id().to_string())
            .collect()
    }
}

#[derive(Clone)]
pub struct WorkspaceLifecycleService {
    inner: Arc<WorkspaceLifecycleInner>,
    handlers: Arc<dyn WorkspaceHandlerRegistry>,
    sessions: Arc<dyn WorkspaceSessionRegistry>,
}

impl WorkspaceLifecycleService {
    pub fn new(
        manager: Arc<dyn IWorkspaceInstanceManager>,
        session_manager: Arc<dyn ISessionManager>,
    ) -> Self {
        let inner = Arc::new(WorkspaceLifecycleInner {
            manager,
            session_manager,
            controllers: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            leases: Mutex::new(HashMap::new()),
            on_did_materialize_handler: Emitter::new(),
            manager_subscription: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let subscription = inner.manager.on_did_change(Arc::new(move |change| {
            let Some(instance) = change.instance else {
                return;
            };
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let handle = inner.handle_of(instance);
            inner.on_did_materialize_handler.fire(handle);
        }));
        *inner.manager_subscription.lock() = Some(subscription);
        Self {
            handlers: Arc::new(WorkspaceHandlerRegistryImpl {
                inner: Arc::downgrade(&inner),
            }),
            sessions: Arc::new(WorkspaceSessionRegistryImpl {
                inner: Arc::downgrade(&inner),
            }),
            inner,
        }
    }

    pub fn on_did_materialize_handler(
        &self,
        listener: Arc<dyn Fn(Arc<dyn IWorkspaceScopeHandle>) + Send + Sync>,
    ) -> DisposableRef {
        Arc::new(
            self.inner
                .on_did_materialize_handler
                .event()
                .subscribe_arc(listener),
        )
    }

    pub async fn handler_for(
        &self,
        reference: WorkspaceRef,
    ) -> Result<Arc<dyn IWorkspaceScopeHandle>, WireAsyncError> {
        let reference = match reference {
            WorkspaceRef::WorkspaceId { workspace_id, root } => {
                WorkspaceInstanceRef::WorkspaceId { workspace_id, root }
            }
            WorkspaceRef::Root { root } => WorkspaceInstanceRef::Root { root },
        };
        let lease = self.inner.manager.acquire(reference).await?;
        let handle = self.inner.handle_of(lease.instance.clone());
        let replaced = self.inner.leases.lock().insert(handle.id.clone(), lease);
        if let Some(replaced) = replaced {
            replaced.dispose();
        }
        Ok(handle)
    }

    pub fn handlers(&self) -> Arc<dyn WorkspaceHandlerRegistry> {
        self.handlers.clone()
    }

    pub fn sessions(&self) -> Arc<dyn WorkspaceSessionRegistry> {
        self.sessions.clone()
    }

    pub fn dispose(&self) {
        self.inner.dispose();
    }
}

#[async_trait]
impl IWorkspaceLifecycleService for WorkspaceLifecycleService {
    fn on_did_materialize_handler(
        &self,
        listener: Arc<dyn Fn(Arc<dyn IWorkspaceScopeHandle>) + Send + Sync>,
    ) -> DisposableRef {
        WorkspaceLifecycleService::on_did_materialize_handler(self, listener)
    }

    async fn handler_for(
        &self,
        reference: WorkspaceRef,
    ) -> Result<Arc<dyn IWorkspaceScopeHandle>, WireAsyncError> {
        WorkspaceLifecycleService::handler_for(self, reference).await
    }

    fn handlers(&self) -> Arc<dyn WorkspaceHandlerRegistry> {
        WorkspaceLifecycleService::handlers(self)
    }

    fn sessions(&self) -> Arc<dyn WorkspaceSessionRegistry> {
        WorkspaceLifecycleService::sessions(self)
    }
}

impl IDisposable for WorkspaceLifecycleService {
    fn dispose(&self) {
        WorkspaceLifecycleService::dispose(self);
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| {
            panic!("workspace lifecycle dependency {index} has an incompatible type")
        })
        .as_ref()
        .clone()
}

static WORKSPACE_LIFECYCLE_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IWorkspaceLifecycleService> = Arc::new(WorkspaceLifecycleService::new(
            dependency::<dyn IWorkspaceInstanceManager>(&args, 0),
            dependency::<dyn ISessionManager>(&args, 1),
        ));
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        refine_service_decorator(&I_WORKSPACE_LIFECYCLE_SERVICE),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "workspaceLifecycle",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_workspace_lifecycle_service_registered() -> Result<(), String> {
    Lazy::force(&WORKSPACE_LIFECYCLE_SERVICE_REGISTRATION).clone()
}
