use std::error::Error;
use std::fmt;
use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared, try_join};
use futures_util::FutureExt;
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, I_INSTANTIATION_SERVICE, ScopeActivation, ServiceDependency,
    ServiceIdentifier, ServicesAccessor, create_decorator,
};
use crate::_base::di::instantiation_service::{InstantiationService, register_descriptor_dependencies};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::register_scoped_service;
use crate::app::scopes::LifecycleScope;
use crate::environment::environment::{Environment, EnvironmentBinding, EnvironmentCapability, EnvironmentLease};
use crate::environment::environment_provider::{
    EnvironmentProviderAttachment, EnvironmentProviderFactory, EnvironmentProviderHost,
};
use crate::environment::environment_registry::{
    EnvironmentRegistrationHandle, EnvironmentRegistry, EnvironmentRegistryChange,
    EnvironmentRegistryOptions, EnvironmentRegistrySnapshot,
};
use crate::environment::local_environment::LocalEnvironment;
use crate::os::interface::host_environment::IHostEnvironment;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

use super::environment::{
    I_ENVIRONMENT_SERVICE, IEnvironmentProviderHandle, IEnvironmentService,
};

#[derive(Clone, Debug)]
struct SharedEnvironmentError(Arc<String>);

impl SharedEnvironmentError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

impl Error for SharedEnvironmentError {}

type LocalReadyFuture = Shared<BoxFuture<'static, Result<(), SharedEnvironmentError>>>;
type ProviderSettledFuture = Shared<BoxFuture<'static, ()>>;
type PendingAttachmentFuture =
    Shared<BoxFuture<'static, Result<Arc<AttachedProvider>, SharedEnvironmentError>>>;

#[derive(Clone)]
struct PendingProvider {
    token: Arc<()>,
    attachment: PendingAttachmentFuture,
}

struct EnvironmentServiceState {
    provider_settled: ProviderSettledFuture,
    providers: IndexMap<String, PendingProvider>,
    stopped: bool,
}

struct EnvironmentServiceInner {
    registry: EnvironmentRegistry,
    instantiation: Arc<InstantiationService>,
    local_ready: LocalReadyFuture,
    state: Arc<Mutex<EnvironmentServiceState>>,
}

#[derive(Clone)]
pub struct EnvironmentService {
    inner: Arc<EnvironmentServiceInner>,
}

struct AttachedProviderState {
    attachment: Option<EnvironmentProviderAttachment>,
    environment_ids: Vec<String>,
    disposed: bool,
}

struct AttachedProvider {
    registry: EnvironmentRegistry,
    state: AsyncMutex<AttachedProviderState>,
}

impl AttachedProvider {
    async fn dispose(&self) -> Result<(), WireAsyncError> {
        let mut state = self.state.lock().await;
        if state.disposed {
            return Ok(());
        }
        state.disposed = true;
        if let Some(attachment) = state.attachment.take() {
            attachment.dispose().await;
        }
        for environment_id in state.environment_ids.clone().into_iter().rev() {
            self.registry.remove(&environment_id).await?;
        }
        state.environment_ids.clear();
        Ok(())
    }
}

impl EnvironmentService {
    pub fn new(
        instantiation: Arc<InstantiationService>,
        environment: Arc<dyn IHostEnvironment>,
        fs: Arc<dyn IHostFileSystem>,
        process: Arc<dyn IHostProcessService>,
    ) -> Self {
        let registry = EnvironmentRegistry::new(EnvironmentRegistryOptions::default());
        let state = Arc::new(Mutex::new(EnvironmentServiceState {
            provider_settled: async {}.boxed().shared(),
            providers: IndexMap::new(),
            stopped: false,
        }));
        let local_ready = {
            let registry = registry.clone();
            let state = state.clone();
            async move {
                environment.ready().await.map_err(SharedEnvironmentError::new)?;
                let state = state.lock();
                if !state.stopped {
                    registry
                        .register(Arc::new(LocalEnvironment::new(
                            environment,
                            Some(fs),
                            Some(process),
                        )))
                        .map_err(SharedEnvironmentError::new)?;
                }
                Ok(())
            }
            .boxed()
            .shared()
        };
        let _ = tokio::spawn(local_ready.clone());
        Self {
            inner: Arc::new(EnvironmentServiceInner {
                registry,
                instantiation,
                local_ready,
                state,
            }),
        }
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let local_ready = self.inner.local_ready.clone();
        let provider_settled = self.inner.state.lock().provider_settled.clone();
        let local = async move { local_ready.await.map_err(SharedEnvironmentError::boxed) };
        let providers = async move {
            provider_settled.await;
            Ok::<(), WireAsyncError>(())
        };
        async move {
            try_join(local, providers).await?;
            Ok(())
        }
        .boxed()
    }

    pub async fn add_provider(
        &self,
        factory: Arc<dyn EnvironmentProviderFactory>,
    ) -> Result<Arc<dyn IEnvironmentProviderHandle>, WireAsyncError> {
        let id = factory.id().to_string();
        let (pending, provider_settled) = {
            let mut state = self.inner.state.lock();
            if state.stopped {
                return Err(io::Error::other("environment service is disposed").into());
            }
            if state.providers.contains_key(&id) {
                return Err(io::Error::other(format!("environment provider {id} already exists")).into());
            }
            let pending = PendingProvider {
                token: Arc::new(()),
                attachment: Self::attach(
                    factory,
                    self.inner.local_ready.clone(),
                    self.inner.instantiation.clone(),
                    self.inner.registry.clone(),
                )
                .boxed()
                .shared(),
            };
            let settled = {
                let attachment = pending.attachment.clone();
                async move {
                    let _ = attachment.await;
                }
                .boxed()
                .shared()
            };
            let previous = state.provider_settled.clone();
            let provider_settled = async move {
                let _ = futures_util::join!(previous, settled);
            }
            .boxed()
            .shared();
            state.provider_settled = provider_settled.clone();
            state.providers.insert(id.clone(), pending.clone());
            (pending, provider_settled)
        };
        let _ = tokio::spawn(pending.attachment.clone());
        let _ = tokio::spawn(provider_settled);
        match pending.attachment.clone().await {
            Ok(attachment) => Ok(Arc::new(EnvironmentProviderHandle {
                service: self.clone(),
                id,
                pending,
                attachment,
            })),
            Err(error) => {
                let mut state = self.inner.state.lock();
                if state
                    .providers
                    .get(&id)
                    .is_some_and(|current| Arc::ptr_eq(&current.token, &pending.token))
                {
                    state.providers.shift_remove(&id);
                }
                Err(error.boxed())
            }
        }
    }

    async fn attach(
        factory: Arc<dyn EnvironmentProviderFactory>,
        local_ready: LocalReadyFuture,
        instantiation: Arc<InstantiationService>,
        registry: EnvironmentRegistry,
    ) -> Result<Arc<AttachedProvider>, SharedEnvironmentError> {
        local_ready.await?;
        let host = Arc::new(EnvironmentProviderHostImpl {
            instantiation,
            registry: registry.clone(),
            environment_ids: Arc::new(Mutex::new(Vec::new())),
        });
        let environment_ids = host.environment_ids.lock().clone();
        match factory.attach(host).await {
            Ok(attachment) => Ok(Arc::new(AttachedProvider {
                registry,
                state: AsyncMutex::new(AttachedProviderState {
                    attachment: Some(attachment),
                    environment_ids,
                    disposed: false,
                }),
            })),
            Err(error) => {
                for environment_id in environment_ids.into_iter().rev() {
                    registry
                        .remove(&environment_id)
                        .await
                        .map_err(SharedEnvironmentError::new)?;
                }
                Err(SharedEnvironmentError::new(error))
            }
        }
    }

    pub async fn dispose(&self) -> Result<(), WireAsyncError> {
        let attachments = {
            let mut state = self.inner.state.lock();
            if state.stopped {
                return Ok(());
            }
            state.stopped = true;
            let attachments = state.providers.values().cloned().collect::<Vec<_>>();
            state.providers.clear();
            attachments
        };
        let mut failure = None;
        for pending in attachments.into_iter().rev() {
            if let Ok(attachment) = pending.attachment.await {
                if let Err(error) = attachment.dispose().await {
                    failure = Some(error);
                    break;
                }
            }
        }
        self.inner.registry.dispose().await;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

struct EnvironmentProviderHandle {
    service: EnvironmentService,
    id: String,
    pending: PendingProvider,
    attachment: Arc<AttachedProvider>,
}

#[async_trait]
impl IEnvironmentProviderHandle for EnvironmentProviderHandle {
    async fn dispose(&self) -> Result<(), WireAsyncError> {
        let current = {
            let mut state = self.service.inner.state.lock();
            if state
                .providers
                .get(&self.id)
                .is_some_and(|current| Arc::ptr_eq(&current.token, &self.pending.token))
            {
                state.providers.shift_remove(&self.id);
                true
            } else {
                false
            }
        };
        if !current {
            return Ok(());
        }
        self.attachment.dispose().await
    }
}

struct EnvironmentProviderHostImpl {
    instantiation: Arc<InstantiationService>,
    registry: EnvironmentRegistry,
    environment_ids: Arc<Mutex<Vec<String>>>,
}

impl EnvironmentProviderHost for EnvironmentProviderHostImpl {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue {
        self.instantiation
            .invoke_function(|accessor| accessor.get_dynamic(id))
    }

    fn register_environment(
        &self,
        environment: Arc<dyn Environment>,
    ) -> Result<EnvironmentRegistrationHandle, WireAsyncError> {
        let handle = self.registry.register(environment)?;
        self.environment_ids.lock().push(handle.environment_id.clone());
        Ok(handle)
    }
}

#[async_trait]
impl IEnvironmentService for EnvironmentService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        EnvironmentService::ready(self)
    }

    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(EnvironmentRegistryChange) + Send + Sync>,
    ) -> DisposableRef {
        self.inner.registry.on_did_change(listener)
    }

    fn snapshot(&self) -> EnvironmentRegistrySnapshot {
        self.inner.registry.snapshot()
    }

    fn current(&self, environment_id: &str) -> Option<Arc<dyn Environment>> {
        self.inner.registry.current(environment_id)
    }

    fn register(
        &self,
        environment: Arc<dyn Environment>,
    ) -> Result<EnvironmentRegistrationHandle, WireAsyncError> {
        self.inner.registry.register(environment)
    }

    async fn drain_session(&self, session_id: &str) {
        self.inner.registry.drain_session(session_id).await;
    }

    async fn add_provider(
        &self,
        factory: Arc<dyn EnvironmentProviderFactory>,
    ) -> Result<Arc<dyn IEnvironmentProviderHandle>, WireAsyncError> {
        EnvironmentService::add_provider(self, factory).await
    }

    async fn dispose(&self) -> Result<(), WireAsyncError> {
        EnvironmentService::dispose(self).await
    }
}

#[async_trait]
impl super::environment::EnvironmentResolver for EnvironmentService {
    fn inspect(&self, binding: &EnvironmentBinding) -> Result<Arc<dyn Environment>, WireAsyncError> {
        self.inner.registry.inspect(binding)
    }

    fn acquire(
        &self,
        binding: &EnvironmentBinding,
        required: &[EnvironmentCapability],
    ) -> Result<EnvironmentLease, WireAsyncError> {
        self.inner.registry.acquire(binding, required)
    }

    async fn acquire_when_ready(
        &self,
        binding: &EnvironmentBinding,
        required: &[EnvironmentCapability],
    ) -> Result<EnvironmentLease, WireAsyncError> {
        self.inner.registry.acquire_when_ready(binding, required).await
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("environment dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static ENVIRONMENT_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IEnvironmentService> = Arc::new(EnvironmentService::new(
            dependency::<InstantiationService>(&args, 0),
            dependency::<dyn IHostEnvironment>(&args, 1),
            dependency::<dyn IHostFileSystem>(&args, 2),
            dependency::<dyn IHostProcessService>(&args, 3),
        ));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: I_INSTANTIATION_SERVICE.clone(),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("hostEnvironment"),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("hostFileSystem"),
                index: 2,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("hostProcessService"),
                index: 3,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_ENVIRONMENT_SERVICE.clone(),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "environment",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_environment_service_registered() -> Result<(), String> {
    Lazy::force(&ENVIRONMENT_SERVICE_REGISTRATION).clone()
}
