use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::IAgentScopeHandle;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::permission_policy::types::PermissionMode;
use crate::agent::profile::profile::BindAgentInput;
use crate::session::session_metadata::session_metadata::AgentMeta;
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct AgentScopeCreatedEvent {
    pub context: Arc<AgentContext>,
    pub handle: Arc<IAgentScopeHandle>,
    pub meta: Option<AgentMeta>,
}

pub const MAIN_AGENT_ID: &str = "main";

#[derive(Clone, Default)]
pub struct CreateAgentOptions {
    pub agent_id: Option<String>,
    pub binding: Option<BindAgentInput>,
    pub environment_id: Option<String>,
    pub environment_cwd: Option<String>,
    pub forked_from: Option<String>,
    pub labels: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForkAgentBinding {
    pub profile: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub strict_thinking: Option<bool>,
}

#[derive(Clone, Default)]
pub struct ForkAgentOptions {
    pub agent_id: Option<String>,
    pub binding: Option<ForkAgentBinding>,
    pub labels: Option<HashMap<String, String>>,
    pub inherited_history: Option<InheritedHistoryOptions>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InheritedHistoryOptions {
    pub max_user_turns: Option<usize>,
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentListFilter {
    pub prefix: Option<String>,
}

#[async_trait]
pub trait IAgentLifecycleService: Send + Sync {
    fn on_did_create(
        &self,
        listener: Arc<dyn Fn(Arc<AgentContext>) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn on_did_create_scope(
        &self,
        listener: Arc<dyn Fn(AgentScopeCreatedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn on_will_close(
        &self,
        listener: Arc<dyn Fn(Arc<AgentContext>) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn on_did_close(
        &self,
        listener: Arc<dyn Fn(Arc<AgentContext>) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    async fn create(
        self: &Arc<Self>,
        opts: Option<CreateAgentOptions>,
    ) -> Result<Arc<AgentContext>, WireAsyncError>;
    async fn fork(
        self: &Arc<Self>,
        source: Arc<AgentContext>,
        opts: Option<ForkAgentOptions>,
    ) -> Result<Arc<AgentContext>, WireAsyncError>;
    fn get(&self, agent_id: &str) -> Option<Arc<AgentContext>>;
    fn list(&self, filter: Option<AgentListFilter>) -> Vec<Arc<AgentContext>>;
    fn broadcast_permission_mode(&self, mode: PermissionMode);
    async fn remove(&self, agent: Arc<AgentContext>) -> Result<(), WireAsyncError>;
    fn handle_of(&self, agent_id: &str) -> Option<Arc<IAgentScopeHandle>>;
    fn adopt(
        self: &Arc<Self>,
        handle: Arc<IAgentScopeHandle>,
    ) -> Result<Arc<AgentContext>, WireAsyncError>;
}
