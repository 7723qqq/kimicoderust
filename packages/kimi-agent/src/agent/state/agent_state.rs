use serde_json::{Map, Value};
use std::sync::Arc;

pub type StateFold = Arc<dyn Fn(&mut Value, &str) + Send + Sync>;

#[derive(Clone, Default)]
pub struct UndoableOptions {
    pub on_undo: Option<Arc<dyn Fn(&mut Value, usize) + Send + Sync>>,
}

#[derive(Clone)]
pub struct ReplayableStateMeta {
    pub durable: bool,
    pub mutable_replay: bool,
    pub undoable: Option<UndoableOptions>,
    pub folds: Vec<(String, StateFold)>,
}

impl ReplayableStateMeta {
    pub fn new(durable: bool, mutable_replay: bool) -> Self {
        Self {
            durable,
            mutable_replay,
            undoable: None,
            folds: Vec::new(),
        }
    }
}

pub type InitialValue = Arc<dyn Fn() -> Value + Send + Sync>;

#[derive(Clone)]
pub struct StateKey {
    pub name: String,
    pub initial: InitialValue,
    pub snapshot_excluded: bool,
    pub replayable: Option<ReplayableStateMeta>,
}

impl StateKey {
    pub fn new(name: impl Into<String>, initial: InitialValue) -> Self {
        Self {
            name: name.into(),
            initial,
            snapshot_excluded: false,
            replayable: None,
        }
    }

    pub fn snapshot_excluded(mut self, excluded: bool) -> Self {
        self.snapshot_excluded = excluded;
        self
    }

    pub fn replayable(mut self, meta: ReplayableStateMeta) -> Self {
        self.replayable = Some(meta);
        self
    }

    pub fn value(&self) -> Value {
        (self.initial)()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateChange {
    pub key: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateInspection {
    pub scope: String,
    pub state: Map<String, Value>,
    pub parent: Option<Box<StateInspection>>,
}

pub trait StateInspector {
    fn inspect(&self) -> StateInspection;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError {
    pub message: String,
}

impl StateError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StateError {}

pub trait IAgentStateService: Send + Sync {
    fn contribute_state(
        &self,
        key: Arc<crate::state::state::ReplayableStateKey>,
    ) -> crate::_base::di::lifecycle::DisposableRef;
    fn has(&self, key: &Arc<crate::state::state::ReplayableStateKey>) -> bool;
    fn get(&self, key: &Arc<crate::state::state::ReplayableStateKey>) -> Value;
    fn set(&self, key: &Arc<crate::state::state::ReplayableStateKey>, value: Value);
    fn on_did_change(
        &self,
        key: &Arc<crate::state::state::ReplayableStateKey>,
    ) -> crate::_base::event::Event<Value>;
    fn on_did_change_any(&self) -> crate::_base::event::Event<StateChange>;
    fn entries(&self) -> Vec<(String, Value)>;
    fn snapshot(&self) -> Map<String, Value>;
    fn inspect(&self) -> StateInspection;
    fn replayable_keys(&self) -> Vec<Arc<crate::state::state::ReplayableStateKey>>;
    fn on_did_contribute_replayable(
        &self,
        listener: Arc<dyn Fn(Arc<crate::state::state::ReplayableStateKey>) + Send + Sync>,
    ) -> crate::_base::di::lifecycle::DisposableRef;
    fn on_did_withdraw_replayable(
        &self,
        listener: Arc<dyn Fn(Arc<crate::state::state::ReplayableStateKey>) + Send + Sync>,
    ) -> crate::_base::di::lifecycle::DisposableRef;
}
