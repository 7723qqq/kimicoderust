use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::{Emitter, Event, Subscription};
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::environment::environment::IEnvironmentService;
use crate::app::event::event_bus::ISessionEventBus;
use crate::environment::environment::{
    Environment, EnvironmentBinding, EnvironmentCapability, EnvironmentLease, EnvironmentPath,
    EnvironmentWorkspaceRoots, LOCAL_ENVIRONMENT_ID,
};
use crate::environment::environment_registry::{
    EnvironmentGenerationSnapshot, EnvironmentRegistryChange, environment_is_ready,
};
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::session_context::session_context::ISessionContext;
use crate::session::workspace_context::workspace_roots::resolve_effective_workspace_roots;
use crate::session::workspace_info::workspace_info::ISessionWorkspaceInfo;
use crate::wire::record::WireAsyncError;

use super::environment_binding::IAgentEnvironmentBindingService;
use super::environment_events::EnvironmentStatusChanged;

#[derive(Clone)]
pub struct AgentEnvironmentBindingSnapshot {
    pub binding: EnvironmentBinding,
    pub available: bool,
    pub environment: Option<EnvironmentGenerationSnapshot>,
}

#[async_trait]
pub trait IAgentEnvironmentService: Send + Sync {
    fn on_did_change(&self) -> Event<()>;
    fn inspect(&self) -> Result<Arc<dyn Environment>, WireAsyncError>;
    fn is_available(&self, required: &[&str]) -> bool;
    fn acquire(&self, required: &[&str]) -> Result<EnvironmentLease, WireAsyncError>;
    async fn acquire_when_ready(
        &self,
        required: &[&str],
    ) -> Result<EnvironmentLease, WireAsyncError>;
    fn workspace_roots(&self) -> EnvironmentWorkspaceRoots;
}

pub static I_AGENT_ENVIRONMENT_SERVICE: LazyLock<ServiceIdentifier<DynamicValue>> =
    LazyLock::new(|| create_decorator("agentEnvironmentService"));

pub async fn acquire_or_when_ready(
    service: &dyn IAgentEnvironmentService,
    required: &[&str],
) -> Result<EnvironmentLease, WireAsyncError> {
    if service.is_available(required) {
        return service.acquire(required);
    }
    service.acquire_when_ready(required).await
}

pub fn pinned_generation(environment: &dyn Environment) -> Option<String> {
    environment_is_ready(environment).then(|| environment.identity().generation)
}

pub struct EnvironmentTempTarget {
    pub fs: Arc<dyn IHostFileSystem>,
    pub path: EnvironmentPath,
    pub dir: String,
}

pub fn environment_temp_target(
    service: &dyn IAgentEnvironmentService,
    subdir: &str,
    local_temp_dir: Option<&str>,
) -> Option<EnvironmentTempTarget> {
    let Ok(lease) = service.acquire(&[]) else {
        return None;
    };
    let result = (|| {
        let environment = &lease.environment;
        let fs = environment.fs();
        let path = environment.path();
        let host = environment.host();
        let identity = environment.identity();
        let temp_dir = host.and_then(|host| host.temp_dir).or_else(|| {
            (identity.environment_id == LOCAL_ENVIRONMENT_ID)
                .then(|| local_temp_dir.map(str::to_string))
                .flatten()
        });
        let (Some(path), Some(temp_dir)) = (path, temp_dir) else {
            return None;
        };
        Some(EnvironmentTempTarget {
            fs,
            dir: path.join(&temp_dir, &["kimi-code", subdir]),
            path,
        })
    })();
    lease.dispose();
    result
}

pub fn snapshot_agent_environment_binding(
    binding_service: &dyn IAgentEnvironmentBindingService,
    environment_service: &dyn IAgentEnvironmentService,
) -> AgentEnvironmentBindingSnapshot {
    let binding = binding_service.current();
    let Ok(environment) = environment_service.inspect() else {
        return AgentEnvironmentBindingSnapshot {
            binding,
            available: false,
            environment: None,
        };
    };
    let identity = environment.identity();
    AgentEnvironmentBindingSnapshot {
        binding,
        available: environment_service.is_available(&[]),
        environment: Some(EnvironmentGenerationSnapshot {
            environment_id: identity.environment_id,
            generation: identity.generation,
            status: environment.status(),
            capabilities: environment.capabilities().into_iter().collect(),
            connect_error: environment.connect_error(),
        }),
    }
}

struct AgentEnvironmentServiceInner {
    scope_context: Arc<dyn IAgentScopeContext>,
    binding: Arc<dyn IAgentEnvironmentBindingService>,
    resolver: Arc<dyn IEnvironmentService>,
    event_bus: Arc<dyn ISessionEventBus>,
    session: Arc<dyn ISessionContext>,
    workspace_info: Arc<dyn ISessionWorkspaceInfo>,
    change_emitter: Emitter<()>,
    binding_subscription: Mutex<Option<Subscription>>,
    registry_subscription: Mutex<Option<Box<dyn IDisposable>>>,
}

impl AgentEnvironmentServiceInner {
    fn publish_environment_status(&self, change: &EnvironmentRegistryChange) {
        let agent = self.scope_context.agent_context();
        if self.scope_context.agent_id() != MAIN_AGENT_ID || !self.event_bus.is_agent_active(&agent) {
            return;
        }
        self.event_bus.publish(
            EnvironmentStatusChanged {
                agent_id: agent.agent_id.clone(),
                environment_id: change.environment_id.clone(),
                status: change.status,
            }
            .into(),
            Some(agent),
        );
    }

    fn dispose(&self) {
        if let Some(subscription) = self.registry_subscription.lock().take() {
            subscription.dispose();
        }
        if let Some(subscription) = self.binding_subscription.lock().take() {
            subscription.dispose();
        }
        self.change_emitter.dispose();
    }
}

#[derive(Clone)]
pub struct AgentEnvironmentService {
    inner: Arc<AgentEnvironmentServiceInner>,
}

impl AgentEnvironmentService {
    pub fn new(
        scope_context: Arc<dyn IAgentScopeContext>,
        binding: Arc<dyn IAgentEnvironmentBindingService>,
        resolver: Arc<dyn IEnvironmentService>,
        event_bus: Arc<dyn ISessionEventBus>,
        session: Arc<dyn ISessionContext>,
        workspace_info: Arc<dyn ISessionWorkspaceInfo>,
    ) -> Self {
        let inner = Arc::new(AgentEnvironmentServiceInner {
            scope_context,
            binding,
            resolver,
            event_bus,
            session,
            workspace_info,
            change_emitter: Emitter::new(),
            binding_subscription: Mutex::new(None),
            registry_subscription: Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let binding_subscription = inner.binding.on_did_change().subscribe(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.change_emitter.fire(());
            }
        });
        *inner.binding_subscription.lock() = Some(binding_subscription);

        let weak = Arc::downgrade(&inner);
        let registry_subscription = inner.resolver.on_did_change(Arc::new(move |change| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if change.environment_id != inner.binding.current().environment_id {
                return;
            }
            if let Some(current) = &change.current {
                let registered = inner.resolver.current(&change.environment_id);
                if registered
                    .as_ref()
                    .is_none_or(|registered| !Arc::ptr_eq(current, registered))
                {
                    return;
                }
            }
            inner.change_emitter.fire(());
            inner.publish_environment_status(&change);
        }));
        *inner.registry_subscription.lock() = Some(registry_subscription);
        Self { inner }
    }

    pub fn dispose(&self) {
        self.inner.dispose();
    }
}

#[async_trait]
impl IAgentEnvironmentService for AgentEnvironmentService {
    fn on_did_change(&self) -> Event<()> {
        self.inner.change_emitter.event()
    }

    fn inspect(&self) -> Result<Arc<dyn Environment>, WireAsyncError> {
        self.inner.resolver.inspect(&self.inner.binding.current())
    }

    fn is_available(&self, required: &[&str]) -> bool {
        let Ok(environment) = self.inspect() else {
            return false;
        };
        if !environment_is_ready(environment.as_ref()) {
            return false;
        }
        let capabilities = environment.capabilities();
        required.iter().all(|capability| match *capability {
            "fs" => capabilities.contains(&EnvironmentCapability::Fs),
            "process" => capabilities.contains(&EnvironmentCapability::Process),
            _ => false,
        })
    }

    fn acquire(&self, required: &[&str]) -> Result<EnvironmentLease, WireAsyncError> {
        self.inner.resolver.acquire(
            &self.inner.binding.current(),
            &environment_capabilities(required),
        )
    }

    async fn acquire_when_ready(
        &self,
        required: &[&str],
    ) -> Result<EnvironmentLease, WireAsyncError> {
        self.inner
            .resolver
            .acquire_when_ready(
                &self.inner.binding.current(),
                &environment_capabilities(required),
            )
            .await
    }

    fn workspace_roots(&self) -> EnvironmentWorkspaceRoots {
        let binding = self.inner.binding.current();
        let host = if binding.cwd.is_none() && binding.environment_id != LOCAL_ENVIRONMENT_ID {
            self.inner
                .resolver
                .inspect(&binding)
                .ok()
                .and_then(|environment| environment.host())
        } else {
            None
        };
        let roots = resolve_effective_workspace_roots(
            self.inner.session.as_ref(),
            Some(&binding),
            self.inner.workspace_info.as_ref(),
            host.as_ref(),
        );
        EnvironmentWorkspaceRoots {
            work_dir: roots.work_dir,
            primary_dir: roots.primary_dir,
            additional_dirs: Some(roots.additional_dirs),
            follows_primary: Some(roots.follows_primary),
        }
    }
}

impl IDisposable for AgentEnvironmentService {
    fn dispose(&self) {
        self.inner.dispose();
    }
}

fn environment_capabilities(required: &[&str]) -> Vec<EnvironmentCapability> {
    required
        .iter()
        .map(|capability| match *capability {
            "fs" => EnvironmentCapability::Fs,
            "process" => EnvironmentCapability::Process,
            _ => unreachable!("unknown environment capability"),
        })
        .collect()
}
