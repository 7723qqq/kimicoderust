use std::sync::Arc;

use crate::_base::state::state_registry::StateRegistry;
use crate::app::scopes::LifecycleScope;

pub type SessionStateService = StateRegistry;

pub fn session_state_service() -> Arc<SessionStateService> {
    StateRegistry::with_inspect("session", None)
}
