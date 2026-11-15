use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::state::state_registry::{
    IStateRegistry, StateInspection, StateKey, StateRegistry,
};
use crate::app::state::app_state::IAppStateService;

use super::workspace_state::IWorkspaceStateService;

pub struct WorkspaceStateService {
    registry: Arc<StateRegistry>,
}

impl WorkspaceStateService {
    pub fn new(app_state: Option<Arc<dyn IAppStateService>>) -> Self {
        let parent = app_state.map(|state| state as Arc<dyn IStateRegistry>);
        Self {
            registry: StateRegistry::with_inspect("workspace", parent),
        }
    }

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

impl IStateRegistry for WorkspaceStateService {
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

impl IWorkspaceStateService for WorkspaceStateService {}
