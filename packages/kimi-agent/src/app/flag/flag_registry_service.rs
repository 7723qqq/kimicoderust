use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;
use crate::errors::BugIndicatingError;

use super::flag_registry::{
    FlagDefinitionInput, IFlagRegistry, get_contributed_flags,
};

pub struct FlagRegistryService {
    by_id: Arc<Mutex<IndexMap<String, FlagDefinitionInput>>>,
}

impl FlagRegistryService {
    pub fn new() -> Result<Self, BugIndicatingError> {
        let this = Self {
            by_id: Arc::new(Mutex::new(IndexMap::new())),
        };
        for definition in get_contributed_flags() {
            this.add(definition)?;
        }
        Ok(this)
    }

    pub fn register(
        &self,
        definition: FlagDefinitionInput,
    ) -> Result<DisposableRef, BugIndicatingError> {
        let id = definition.id.clone();
        self.add(definition)?;
        let by_id = Arc::downgrade(&self.by_id);
        Ok(to_disposable(move || {
            if let Some(by_id) = by_id.upgrade() {
                by_id.lock().shift_remove(&id);
            }
        }))
    }

    pub fn get(&self, id: &str) -> Option<FlagDefinitionInput> {
        self.by_id.lock().get(id).cloned()
    }

    pub fn list(&self) -> Vec<FlagDefinitionInput> {
        self.by_id.lock().values().cloned().collect()
    }

    fn add(&self, definition: FlagDefinitionInput) -> Result<(), BugIndicatingError> {
        let mut by_id = self.by_id.lock();
        if by_id.contains_key(&definition.id) {
            return Err(BugIndicatingError::new(format!(
                "Flag '{}' is already registered",
                definition.id
            )));
        }
        by_id.insert(definition.id.clone(), definition);
        Ok(())
    }
}

impl IFlagRegistry for FlagRegistryService {
    fn register(
        &self,
        definition: FlagDefinitionInput,
    ) -> Result<DisposableRef, BugIndicatingError> {
        FlagRegistryService::register(self, definition)
    }

    fn get(&self, id: &str) -> Option<FlagDefinitionInput> {
        FlagRegistryService::get(self, id)
    }

    fn list(&self) -> Vec<FlagDefinitionInput> {
        FlagRegistryService::list(self)
    }
}

pub fn register_flag_registry_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "flag",
    );
}
