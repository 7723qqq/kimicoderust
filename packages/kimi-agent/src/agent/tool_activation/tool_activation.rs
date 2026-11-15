use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use async_trait::async_trait;

#[async_trait]
pub trait IAgentToolActivationService: Send + Sync {
    async fn activate(&self);
}

pub trait AgentToolActivation {
    fn activate(&self);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolActivationProfileData {
    pub active_tool_names: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
}

pub trait ToolActivationSource {
    fn profile(&self) -> ToolActivationProfileData;

    fn workspace_disabled_tools(&self) -> Option<Vec<String>>;
}


pub static I_AGENT_TOOL_ACTIVATION_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentToolActivationService>>> =
    LazyLock::new(|| create_decorator("agentToolActivationService"));
