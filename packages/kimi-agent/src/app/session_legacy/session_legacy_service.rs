use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::scope::{IAgentScopeHandle, ISessionScopeHandle, ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::features::goal::types::{GoalSnapshot, GoalToolResult};
use crate::session::agentLifecycle::agentLifecycle::IAgentLifecycleService;
use crate::session::agentLifecycle::errors::AGENT_NOT_FOUND;
use crate::session::agentLifecycle::mainAgent::ensure_main_agent;
use crate::session::errors::SESSION_NOT_FOUND;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::session_legacy::ISessionLegacyService;

#[async_trait]
pub trait ISessionLegacyRuntime: Send + Sync {
    async fn resume(&self, session_id: &str) -> Result<Option<Arc<ISessionScopeHandle>>, WireAsyncError>;
    fn goal(&self, agent: Arc<IAgentScopeHandle>) -> Result<GoalToolResult, WireAsyncError>;
}

pub struct SessionLegacyService {
    runtime: Arc<dyn ISessionLegacyRuntime>,
}

impl SessionLegacyService {
    pub fn new(runtime: Arc<dyn ISessionLegacyRuntime>) -> Self {
        Self { runtime }
    }

    async fn resolve_main_agent(&self, session_id: &str) -> Result<Arc<IAgentScopeHandle>, WireAsyncError> {
        let Some(session) = self.runtime.resume(session_id).await? else {
            return Err(Box::new(Error2::new(SESSION_NOT_FOUND, format!("session {session_id} does not exist"))));
        };
        let context = ensure_main_agent(&session).await?;
        let handle = session.accessor().get::<IAgentLifecycleService>().handle_of(&context.agent_id);
        handle.ok_or_else(|| Box::new(Error2::new(AGENT_NOT_FOUND, "Main agent was not found")) as WireAsyncError)
    }
}

#[async_trait]
impl ISessionLegacyService for SessionLegacyService {
    async fn goal(&self, session_id: &str) -> Result<Option<GoalSnapshot>, WireAsyncError> {
        let agent = self.resolve_main_agent(session_id).await?;
        Ok(self.runtime.goal(agent)?.goal)
    }
}

pub fn register_session_legacy_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "sessionLegacy");
}
