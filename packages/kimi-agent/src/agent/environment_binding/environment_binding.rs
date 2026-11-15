use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::event::Event;
use crate::environment::environment::EnvironmentBinding;
use crate::wire::record::WireAsyncError;

pub trait IAgentEnvironmentBindingService: Send + Sync {
    fn current(&self) -> EnvironmentBinding;
    fn on_did_change(&self) -> Event<EnvironmentBinding>;
    fn bind(
        &self,
        environment_id: &str,
        cwd: Option<String>,
    ) -> Result<EnvironmentBinding, WireAsyncError>;
}

pub trait IAgentEnvironmentBindingSeed: Send + Sync {
    fn binding(&self) -> &EnvironmentBinding;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentEnvironmentBindingSeed {
    pub binding: EnvironmentBinding,
}

impl IAgentEnvironmentBindingSeed for AgentEnvironmentBindingSeed {
    fn binding(&self) -> &EnvironmentBinding {
        &self.binding
    }
}


pub static I_AGENT_ENVIRONMENT_BINDING_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentEnvironmentBindingService>>> =
    LazyLock::new(|| create_decorator("agentEnvironmentBindingService"));
