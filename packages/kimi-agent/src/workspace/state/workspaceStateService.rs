use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::_base::state::state_registry::StateRegistry;
use crate::app::state::app_state::IAppStateService;

use super::workspace_state::IWorkspaceStateService;

pub struct WorkspaceStateService {
    registry: StateRegistry,
}

impl WorkspaceStateService {
    pub fn new(app_state: Option<Arc<dyn IAppStateService>>) -> Self {
        let mut registry = StateRegistry::new();
        registry.inspect_scope = "workspace";
        registry.inspect_parent = app_state;
        Self { registry }
    }
}

impl WorkspaceStateService {
    pub fn dispose(&self) {
        self.registry.dispose();
    }
}

impl Deref for WorkspaceStateService {
    type Target = StateRegistry;

    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl DerefMut for WorkspaceStateService {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.registry
    }
}

impl IWorkspaceStateService for WorkspaceStateService {}
