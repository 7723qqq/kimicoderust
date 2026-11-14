use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::errors::errors::BugIndicatingError;
use crate::app::scopes::LifecycleScope;

use super::agent_profile_catalog::{AgentProfile, DEFAULT_AGENT_PROFILE_NAME};
use super::agent_profile_contribution::{
    AGENT_PROFILE_SOURCE_PRIORITY, AgentProfileContribution,
};
use super::agent_profile_registry::{AgentProfileRegistration, IAgentProfileRegistry};
use super::builtin_agent_profile_loader::{
    BUILTIN_AGENT_PROFILE_SOURCE_ID, IBuiltinAgentProfileLoader,
};
use super::contribution::get_agent_profile_contributions;

pub struct BuiltinAgentProfileLoaderService {
    by_name: IndexMap<String, AgentProfile>,
    ordered: Vec<AgentProfile>,
    _registration: Mutex<Option<Box<dyn IDisposable>>>,
}

impl BuiltinAgentProfileLoaderService {
    pub fn new(registry: Arc<dyn IAgentProfileRegistry>) -> Self {
        let ordered = get_agent_profile_contributions();
        let by_name = ordered
            .iter()
            .map(|profile| (profile.name.clone(), profile.clone()))
            .collect();
        let registration = registry.register(AgentProfileRegistration {
            source_id: BUILTIN_AGENT_PROFILE_SOURCE_ID.to_string(),
            priority: AGENT_PROFILE_SOURCE_PRIORITY.builtin,
            workspace_key: None,
            context_key: None,
            contribution: AgentProfileContribution {
                profiles: ordered.clone(),
                skipped: None,
                scanned_roots: None,
            },
        });
        Self {
            by_name,
            ordered,
            _registration: Mutex::new(Some(registration)),
        }
    }

    pub fn get(&self, name: &str) -> Option<AgentProfile> {
        self.by_name.get(name).cloned()
    }

    pub fn get_default(&self) -> AgentProfile {
        self.get(DEFAULT_AGENT_PROFILE_NAME).unwrap_or_else(|| {
            std::panic::panic_any(BugIndicatingError::new(format!(
                "Default agent profile \"{DEFAULT_AGENT_PROFILE_NAME}\" is not registered"
            )))
        })
    }

    pub fn list(&self) -> Vec<AgentProfile> {
        self.ordered.clone()
    }
}

impl IBuiltinAgentProfileLoader for BuiltinAgentProfileLoaderService {
    fn get(&self, name: &str) -> Option<AgentProfile> {
        BuiltinAgentProfileLoaderService::get(self, name)
    }

    fn get_default(&self) -> AgentProfile {
        BuiltinAgentProfileLoaderService::get_default(self)
    }

    fn list(&self) -> Vec<AgentProfile> {
        BuiltinAgentProfileLoaderService::list(self)
    }
}

pub fn register_builtin_agent_profile_loader_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "agentProfileCatalog",
    );
}
