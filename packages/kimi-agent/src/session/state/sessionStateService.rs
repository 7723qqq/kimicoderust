use std::sync::Arc;

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::state::state_registry::StateRegistry;
use crate::app::scopes::LifecycleScope;

pub type SessionStateService = StateRegistry;

pub fn session_state_service() -> Arc<SessionStateService> {
    StateRegistry::with_inspect("session", None)
}

pub fn register_session_state_service() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "state",
    );
}
