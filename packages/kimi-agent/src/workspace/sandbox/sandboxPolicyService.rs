use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ServiceIdentifier, ServiceDependency, create_decorator, refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::agent::tool_executor::before_tool_execute_event::deny_tool_execution;
use crate::agent::tool_executor::tool_executor::{
    I_AGENT_TOOL_EXECUTOR_SERVICE, IAgentToolExecutorService,
};
use crate::app::config::config::{I_CONFIG_SERVICE, IConfigService};
use crate::app::scopes::LifecycleScope;
use crate::session::sessionContext::sessionContext::{I_SESSION_CONTEXT, ISessionContext};

use super::sandbox::{SandboxMode, resolve_sandbox_policy, sandbox_tool_veto};

/// v2 `ISandboxPolicyService`: a marker interface. The service exists for the
/// side effect of its constructor, which subscribes the veto.
pub trait ISandboxPolicyService: Send + Sync {}

pub struct SandboxPolicyService {
    _subscription: Option<DisposableRef>,
}

impl SandboxPolicyService {
    pub fn new(
        config: Arc<dyn IConfigService>,
        session_ctx: Arc<dyn ISessionContext>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
    ) -> Self {
        let policy = resolve_sandbox_policy(config.as_ref(), &session_ctx.cwd());
        if policy.mode == SandboxMode::Off {
            return Self {
                _subscription: None,
            };
        }
        let subscription = tool_executor.on_before_execute_tool().subscribe({
            let policy = policy.clone();
            move |event| {
                let message = sandbox_tool_veto(
                    &policy,
                    event.context.base.tool_call.name.as_str(),
                    event
                        .context
                        .base
                        .tool
                        .as_ref()
                        .and_then(|tool| tool.side_effect()),
                    event.context.execution.accesses.as_ref(),
                );
                if let Some(message) = message {
                    event.veto(deny_tool_execution(&message));
                }
            }
        });
        Self {
            _subscription: Some(Arc::new(subscription) as DisposableRef),
        }
    }
}

impl Drop for SandboxPolicyService {
    fn drop(&mut self) {
        if let Some(subscription) = self._subscription.take() {
            subscription.dispose();
        }
    }
}

impl ISandboxPolicyService for SandboxPolicyService {}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("sandboxPolicy dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static I_SANDBOX_POLICY_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("sandboxPolicyService"));

static SANDBOX_POLICY_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn ISandboxPolicyService> = Arc::new(SandboxPolicyService::new(
            dependency::<dyn IConfigService>(&args, 0),
            dependency::<dyn ISessionContext>(&args, 1),
            dependency::<dyn IAgentToolExecutorService>(&args, 2),
        ));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: refine_service_decorator(&I_CONFIG_SERVICE),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_SESSION_CONTEXT),
                index: 1,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_AGENT_TOOL_EXECUTOR_SERVICE),
                index: 2,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        refine_service_decorator(&I_SANDBOX_POLICY_SERVICE),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "sandbox",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_sandbox_policy_service_registered() -> Result<(), String> {
    Lazy::force(&SANDBOX_POLICY_SERVICE_REGISTRATION).clone()
}
