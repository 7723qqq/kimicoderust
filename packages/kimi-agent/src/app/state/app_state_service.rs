use std::ops::{Deref, DerefMut};

use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::state::state_registry::StateRegistry;
use crate::app::scopes::LifecycleScope;

use super::app_state::IAppStateService;

pub struct AppStateService {
    registry: StateRegistry,
}

impl AppStateService {
    pub fn new() -> Self {
        let mut registry = StateRegistry::new();
        registry.inspect_scope = "app";
        Self { registry }
    }

    pub fn dispose(&self) {
        self.registry.dispose();
    }
}

impl Default for AppStateService {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for AppStateService {
    type Target = StateRegistry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl DerefMut for AppStateService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

impl IAppStateService for AppStateService {}

pub fn register_app_state_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "state");
}
