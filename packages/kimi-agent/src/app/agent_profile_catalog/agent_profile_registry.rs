use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};

use super::agent_profile_contribution::AgentProfileContribution;

#[derive(Clone)]
pub struct AgentProfileRegistration {
    pub source_id: String,
    pub priority: i64,
    pub workspace_key: Option<String>,
    pub context_key: Option<String>,
    pub contribution: AgentProfileContribution,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileRegistryChange {
    pub source_id: String,
    pub workspace_key: Option<String>,
    pub context_key: Option<String>,
}

pub trait IAgentProfileRegistry: Send + Sync {
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(AgentProfileRegistryChange) + Send + Sync>,
    ) -> DisposableRef;
    fn entries(&self) -> Vec<AgentProfileRegistration>;
    fn register(&self, registration: AgentProfileRegistration) -> DisposableRef;
}

pub static I_AGENT_PROFILE_REGISTRY: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("agentProfileRegistry"));
