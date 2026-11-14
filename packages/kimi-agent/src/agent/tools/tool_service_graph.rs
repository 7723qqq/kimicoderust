use std::sync::Arc;

use crate::_base::di::instantiation_service::InstantiationService;
use crate::_base::di::service_collection::ServiceCollection;
use crate::_base::state::state_registry::StateRegistry;
use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::agent::scope_context::scope_context::{
    IAgentScopeContext, make_agent_scope_context,
};
use crate::agent::task::task::IAgentTaskService;
use crate::agent::tool_policy::tool_policy::IAgentToolPolicyService;
use crate::app::config::config::IConfigService;
use crate::app::environment::environment::IEnvironmentService;
use crate::app::environment::environment_service::EnvironmentService;
use crate::app::event::event_bus::ISessionEventBus;
use crate::app::event::event_bus_service::EventBusService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::app::telemetry::telemetry_service::TelemetryService;
use crate::os::backends::host::host_environment_service::HostEnvironmentService;
use crate::os::backends::host::host_fs_service::HostFileSystem;
use crate::os::backends::host::host_process_service::HostProcessService;
use crate::os::backends::host::host_timers_service::HostTimersService;
use crate::os::interface::host_environment::IHostEnvironment;
use crate::os::interface::host_file_system::IHostFileSystem;
use crate::os::interface::host_process::IHostProcessService;
use crate::os::interface::host_timers::IHostTimers;
use crate::session::session_context::session_context::{
    ISessionContext, SessionContextInput, make_session_context,
};
use crate::session::state::session_state::ISessionStateService;
use crate::session::workspace_context::workspace_context::ISessionWorkspaceContext;

#[derive(Clone, Default)]
pub struct ToolServiceGraphInjection {
    pub environment: Option<Arc<dyn IAgentEnvironmentService>>,
    pub workspace_context: Option<Arc<dyn ISessionWorkspaceContext>>,
    pub tasks: Option<Arc<dyn IAgentTaskService>>,
    pub tool_policy: Option<Arc<dyn IAgentToolPolicyService>>,
    pub config: Option<Arc<dyn IConfigService>>,
}

#[derive(Clone)]
pub struct ToolServiceGraphHost {
    pub environment: Arc<dyn IHostEnvironment>,
    pub fs: Arc<dyn IHostFileSystem>,
    pub process: Arc<dyn IHostProcessService>,
    pub timers: Arc<dyn IHostTimers>,
}

impl ToolServiceGraphHost {
    pub async fn new() -> Self {
        let timers: Arc<dyn IHostTimers> = Arc::new(HostTimersService::new());
        let environment: Arc<dyn IHostEnvironment> = Arc::new(HostEnvironmentService::new());
        let fs: Arc<dyn IHostFileSystem> = Arc::new(HostFileSystem::new());
        let process: Arc<dyn IHostProcessService> =
            Arc::new(HostProcessService::new(Some(timers.clone())));
        Self {
            environment,
            fs,
            process,
            timers,
        }
    }
}

#[derive(Clone)]
pub struct ToolServiceGraph {
    pub session: Arc<dyn ISessionContext>,
    pub agent_scope: Arc<dyn IAgentScopeContext>,
    pub host: ToolServiceGraphHost,
    pub telemetry: Arc<dyn ITelemetryService>,
    pub event_bus: Arc<dyn ISessionEventBus>,
    pub session_state: Arc<dyn ISessionStateService>,
    pub environment: Arc<dyn IEnvironmentService>,
    pub instantiation: Arc<InstantiationService>,
    injection: ToolServiceGraphInjection,
}

impl ToolServiceGraph {
    pub async fn new(
        session: SessionContextInput,
        agent_id: String,
        agent_scope: String,
        forked_from: Option<String>,
        generation: u32,
        injection: ToolServiceGraphInjection,
    ) -> Self {
        let host = ToolServiceGraphHost::new().await;
        let session: Arc<dyn ISessionContext> = Arc::new(make_session_context(session));
        let agent_scope: Arc<dyn IAgentScopeContext> =
            make_agent_scope_context(agent_id, agent_scope, forked_from, generation);
        let telemetry: Arc<dyn ITelemetryService> = Arc::new(TelemetryService::new());
        let event_bus: Arc<dyn ISessionEventBus> = Arc::new(EventBusService::new());
        let session_state: Arc<dyn ISessionStateService> = StateRegistry::new();
        let instantiation = InstantiationService::new(
            Arc::new(ServiceCollection::new(Vec::new())),
            true,
            None,
            false,
        );
        let environment: Arc<dyn IEnvironmentService> = Arc::new(EnvironmentService::new(
            instantiation.clone(),
            host.environment.clone(),
            host.fs.clone(),
            host.process.clone(),
        ));
        Self {
            session,
            agent_scope,
            host,
            telemetry,
            event_bus,
            session_state,
            environment,
            instantiation,
            injection,
        }
    }

    pub fn injection(&self) -> &ToolServiceGraphInjection {
        &self.injection
    }

    pub fn set_injection(&mut self, injection: ToolServiceGraphInjection) {
        self.injection = injection;
    }

    pub fn with_injection(mut self, injection: ToolServiceGraphInjection) -> Self {
        self.injection = injection;
        self
    }
}
