use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::Value;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::DisposableRef;
use crate::_base::event::Event;
use crate::_base::state::state_registry::{IStateRegistry, StateChange, StateKey};
use crate::state::state::ReplayableStateKey;

use super::errors::StateError;

pub type ReplayableStateListener =
    Arc<dyn Fn(&Arc<ReplayableStateKey>) -> Result<(), StateError> + Send + Sync>;

pub trait IAgentStateService: IStateRegistry {
    fn contribute_state(&self, key: StateKey<Value>) -> Result<DisposableRef, StateError>;
    fn has(&self, key: &StateKey<Value>) -> bool;
    fn get(&self, key: &StateKey<Value>) -> Value;
    fn set(&self, key: &StateKey<Value>, value: Value);
    fn on_did_change(&self, key: &StateKey<Value>) -> Event<Value>;
    fn on_did_change_any(&self) -> Event<StateChange>;
    fn entries(&self) -> Vec<(String, DynamicValue)>;
    fn replayable_keys(&self) -> Vec<Arc<ReplayableStateKey>>;
    fn on_did_contribute_replayable(&self, listener: ReplayableStateListener) -> DisposableRef;
    fn on_did_withdraw_replayable(&self, listener: ReplayableStateListener) -> DisposableRef;
}

pub static I_AGENT_STATE_SERVICE: Lazy<ServiceIdentifier<Arc<dyn IAgentStateService>>> =
    Lazy::new(|| create_decorator("agentStateService"));
