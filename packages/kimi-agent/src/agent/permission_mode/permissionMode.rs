use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use crate::agent::state::listeners::Listeners;

pub use crate::agent::permission_policy::types::PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionModeChangedContext {
    pub mode: PermissionMode,
    pub previous_mode: PermissionMode,
}

pub trait IAgentPermissionModeService: Send + Sync {
    fn mode(&self) -> PermissionMode;
    fn set_mode(&self, mode: PermissionMode);
    fn set_mode_and_broadcast(&self, mode: PermissionMode);
    fn on_did_change_mode(&self) -> Arc<Listeners<PermissionModeChangedContext>>;
}


pub static I_AGENT_PERMISSION_MODE_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentPermissionModeService>>> =
    LazyLock::new(|| create_decorator("agentPermissionModeService"));
