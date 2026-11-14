use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::IDisposable;
use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfile;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentProfileSuppressedReason {
    Priority,
    BuiltinOverrideRequired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileSuppressedCandidate {
    pub source_id: String,
    pub priority: i32,
    pub reason: AgentProfileSuppressedReason,
}

#[derive(Clone)]
pub struct AgentProfileInspection {
    pub name: String,
    pub profile: Arc<AgentProfile>,
    pub source_id: String,
    pub priority: i32,
    pub suppressed: Vec<AgentProfileSuppressedCandidate>,
}

#[async_trait]
pub trait ISessionAgentProfileCatalog: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
    fn get(&self, name: &str) -> Option<Arc<AgentProfile>>;
    fn get_default(&self) -> Arc<AgentProfile>;
    fn list(&self) -> Vec<Arc<AgentProfile>>;
    fn inspect(&self, name: &str) -> Option<AgentProfileInspection>;
    async fn load(&self) -> Result<(), WireAsyncError>;
    async fn reload(&self) -> Result<(), WireAsyncError>;
}
