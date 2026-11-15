use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::state::state_registry::{
    IStateRegistry, StateInspection, StateKey, StateRegistry,
};
use crate::app::scopes::LifecycleScope;

use super::app_state::IAppStateService;

pub struct AppStateService {
    registry: Arc<StateRegistry>,
}

impl AppStateService {
    pub fn new() -> Self {
        Self {
            registry: StateRegistry::with_inspect("app", None),
        }
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

impl IStateRegistry for AppStateService {
    fn contribute_state_dynamic(
        &self,
        key: StateKey<DynamicValue>,
    ) -> Result<DisposableRef, BugIndicatingError> {
        self.registry.contribute_state_dynamic(key)
    }

    fn has_dynamic(&self, key: &StateKey<DynamicValue>) -> bool {
        self.registry.has_dynamic(key)
    }

    fn get_dynamic(&self, key: &StateKey<DynamicValue>) -> DynamicValue {
        self.registry.get_dynamic(key)
    }

    fn set_dynamic(&self, key: &StateKey<DynamicValue>, value: DynamicValue) {
        self.registry.set_dynamic(key, value)
    }

    fn snapshot(&self) -> HashMap<String, serde_json::Value> {
        self.registry.snapshot()
    }

    fn inspect(&self) -> StateInspection {
        self.registry.inspect()
    }
}

impl IAppStateService for AppStateService {}

pub fn register_app_state_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "state");
}
