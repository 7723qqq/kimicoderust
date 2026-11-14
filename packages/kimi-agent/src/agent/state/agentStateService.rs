use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::event::Event;
use crate::_base::state::state_registry::{
    IStateRegistry, StateChange, StateInspection, StateKey, StateRegistry, replayable_is_object,
};
use crate::app::scopes::LifecycleScope;
use crate::session::state::session_state::ISessionStateService;
use crate::state::state::ReplayableStateKey;

use super::agent_state::StateError;
use super::agent_state_service::Listeners;
use super::agent_state_v2::{IAgentStateService, ReplayableStateListener};

pub struct AgentStateService {
    registry: Arc<StateRegistry>,
    replayables: Arc<Mutex<Vec<Arc<ReplayableStateKey>>>>,
    contribute_listeners: Arc<Listeners<Arc<ReplayableStateKey>>>,
    withdraw_listeners: Arc<Listeners<Arc<ReplayableStateKey>>>,
}

impl AgentStateService {
    pub fn new(session_state: Option<Arc<dyn ISessionStateService>>) -> Self {
        let parent = session_state.map(|service| service as Arc<dyn IStateRegistry>);
        Self {
            registry: StateRegistry::with_inspect("agent", parent),
            replayables: Arc::new(Mutex::new(Vec::new())),
            contribute_listeners: Arc::new(Listeners::default()),
            withdraw_listeners: Arc::new(Listeners::default()),
        }
    }
}

impl IAgentStateService for AgentStateService {
    fn contribute_state(&self, key: StateKey<Value>) -> Result<DisposableRef, StateError> {
        let Some(replayable) = replayable_descriptor(&key) else {
            return self.registry.contribute_state(key).map_err(state_error);
        };
        let registration = self.registry.contribute_key(key).map_err(state_error)?;
        self.replayables.lock().push(replayable.clone());
        if let Err(error) = self.contribute_listeners.fire(&replayable) {
            registration.dispose();
            withdraw_replayable(&self.replayables, &replayable);
            let _ = self.withdraw_listeners.fire(&replayable);
            return Err(error);
        }
        let replayables = self.replayables.clone();
        let withdraw_listeners = self.withdraw_listeners.clone();
        Ok(to_disposable(move || {
            registration.dispose();
            withdraw_replayable(&replayables, &replayable);
            let _ = withdraw_listeners.fire(&replayable);
        }))
    }

    fn has(&self, key: &StateKey<Value>) -> bool {
        self.registry.has(key)
    }

    fn get(&self, key: &StateKey<Value>) -> Value {
        self.registry.get(key).unwrap_or_else(|error| panic!("{error}"))
    }

    fn set(&self, key: &StateKey<Value>, value: Value) {
        self.registry
            .set(key, value)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    fn on_did_change(&self, key: &StateKey<Value>) -> Event<Value> {
        self.registry.on_did_change(key)
    }

    fn on_did_change_any(&self) -> Event<StateChange> {
        self.registry.on_did_change_any.clone()
    }

    fn entries(&self) -> Vec<(String, DynamicValue)> {
        self.registry.entries()
    }

    fn replayable_keys(&self) -> Vec<Arc<ReplayableStateKey>> {
        self.replayables.lock().clone()
    }

    fn on_did_contribute_replayable(&self, listener: ReplayableStateListener) -> DisposableRef {
        let registration = self.contribute_listeners.add(listener);
        to_disposable(move || registration.dispose())
    }

    fn on_did_withdraw_replayable(&self, listener: ReplayableStateListener) -> DisposableRef {
        let registration = self.withdraw_listeners.add(listener);
        to_disposable(move || registration.dispose())
    }
}

impl IStateRegistry for AgentStateService {
    fn snapshot(&self) -> HashMap<String, Value> {
        IStateRegistry::snapshot(self.registry.as_ref())
    }

    fn inspect(&self) -> StateInspection {
        IStateRegistry::inspect(self.registry.as_ref())
    }
}

impl IDisposable for AgentStateService {
    fn dispose(&self) {
        StateRegistry::dispose(self.registry.as_ref());
    }
}

fn replayable_descriptor(key: &StateKey<Value>) -> Option<Arc<ReplayableStateKey>> {
    let replayable = key.replayable.as_ref()?;
    if !replayable_is_object(replayable) {
        return None;
    }
    Some(
        replayable
            .downcast_ref::<Arc<ReplayableStateKey>>()
            .expect("replayable state key must carry its ReplayableStateKey descriptor")
            .clone(),
    )
}

fn withdraw_replayable(
    replayables: &Mutex<Vec<Arc<ReplayableStateKey>>>,
    replayable: &Arc<ReplayableStateKey>,
) {
    replayables
        .lock()
        .retain(|item| !Arc::ptr_eq(item, replayable));
}

fn state_error(error: BugIndicatingError) -> StateError {
    StateError::new(error.message)
}

static AGENT_STATE_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn IAgentStateService> = Arc::new(AgentStateService::new(None));
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        crate::_base::di::instantiation::create_decorator("agentStateService"),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "state",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_agent_state_service_registered() -> Result<(), String> {
    Lazy::force(&AGENT_STATE_SERVICE_REGISTRATION).clone()
}
