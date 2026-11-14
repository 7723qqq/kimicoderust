use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use futures_util::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::descriptors::DynamicValue;
use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::wire::record::{PartsTransformer, RecordDehydrator, WireAsyncError};

use super::errors::{StateError, STATE_DUPLICATE_FOLD, STATE_DURABILITY_MISMATCH};

pub use crate::_base::state::state_registry::StateKey;
pub use crate::wire::record::PartsTransformer as PublicPartsTransformer;

pub type StateFold = Arc<dyn Fn(Value, &Event2, &mut dyn FoldContext) -> Option<Value> + Send + Sync>;
pub type EventApplier = Arc<dyn Fn(&Event2, &mut dyn FoldContext) + Send + Sync>;

pub struct StateBlobCodec {
    pub dehydrate: RecordDehydrator,
    pub rehydrate: Arc<dyn Fn(Value, PartsTransformer) -> BoxFuture<'static, Result<Value, WireAsyncError>> + Send + Sync>,
}

pub trait FoldContext: Send {
    fn silent(&self) -> bool;
    fn checkpoint(&mut self);
    fn clear_checkpoints(&mut self);
    fn undo_to_checkpoint(&mut self, count: u64);
    fn emit(&mut self, event: Event2);
}

pub struct ReplayableOptions {
    pub schema: Value,
    pub durable: Option<bool>,
    pub blobs: Option<StateBlobCodec>,
    pub mutable_replay: Option<bool>,
}

pub struct UndoableOptions {
    pub on_undo: Option<StateFold>,
}

pub struct ReplayableStateMeta {
    pub schema: Value,
    pub durable: bool,
    pub blobs: Option<StateBlobCodec>,
    pub mutable_replay: bool,
    pub undoable: Option<UndoableOptions>,
    pub folds: HashMap<usize, StateFold>,
}

pub struct ReplayableStateKey {
    pub snapshot_excluded: bool,
    pub initial: Arc<dyn Fn() -> Value + Send + Sync>,
    pub name: String,
    pub replayable: ReplayableStateMeta,
}

impl ReplayableStateKey {
    pub fn new(name: String, initial: Arc<dyn Fn() -> Value + Send + Sync>, opts: ReplayableOptions) -> Self {
        Self {
            snapshot_excluded: true,
            initial,
            name,
            replayable: ReplayableStateMeta {
                schema: opts.schema,
                durable: opts.durable.unwrap_or(true),
                blobs: opts.blobs,
                mutable_replay: opts.mutable_replay == Some(true),
                undoable: None,
                folds: HashMap::new(),
            },
        }
    }

    pub fn state_key(self: &Arc<Self>) -> StateKey {
        StateKey {
            name: self.name.clone(),
            initial: self.initial.clone(),
            snapshot_excluded: self.snapshot_excluded,
            replayable: Some(self.clone() as DynamicValue),
        }
    }

    pub fn undoable(&mut self, opts: Option<UndoableOptions>) -> Result<&mut Self, StateError> {
        if self.replayable.undoable.is_some() {
            return Err(StateError::new(
                "internal",
                format!("State key '{}' is already undoable", self.name),
            ));
        }
        if !self.replayable.durable {
            return Err(StateError::new(
                "internal",
                format!("Transient state key '{}' cannot be undoable", self.name),
            ));
        }
        self.replayable.undoable = Some(opts.unwrap_or(UndoableOptions { on_undo: None }));
        Ok(self)
    }

    pub fn on(&mut self, cls: &Arc<Event2Class>, fold: StateFold) -> Result<&mut Self, StateError> {
        let address = Arc::as_ptr(cls) as usize;
        if self.replayable.folds.contains_key(&address) {
            let mut details = serde_json::Map::new();
            details.insert("state".to_string(), self.name.clone().into());
            details.insert("type".to_string(), cls.type_.clone().into());
            return Err(StateError::with_options(
                STATE_DUPLICATE_FOLD,
                format!("State '{}' already folds event '{}'", self.name, cls.type_),
                kimi_core::primitives::errors::Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            ));
        }
        if !self.replayable.durable && cls.durable {
            let mut details = serde_json::Map::new();
            details.insert("state".to_string(), self.name.clone().into());
            details.insert("type".to_string(), cls.type_.clone().into());
            return Err(StateError::with_options(
                STATE_DURABILITY_MISMATCH,
                format!("Transient state '{}' cannot fold durable event '{}'", self.name, cls.type_),
                kimi_core::primitives::errors::Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            ));
        }
        register_event2_class(cls)?;
        self.replayable.folds.insert(address, fold);
        Ok(self)
    }
}

pub struct StateKeyBuilder<T = Value> {
    pub name: String,
    pub initial: Arc<dyn Fn() -> T + Send + Sync>,
}

pub type StateDefinition<T> = StateKeyBuilder<T>;

impl StateKeyBuilder<Value> {
    pub fn replayable(self, opts: ReplayableOptions) -> Arc<ReplayableStateKey> {
        Arc::new(ReplayableStateKey::new(self.name, self.initial, opts))
    }
}

pub fn define_state<T: Send + Sync + 'static>(
    name: &str,
    initial: Arc<dyn Fn() -> T + Send + Sync>,
) -> StateKeyBuilder<T> {
    StateKeyBuilder {
        name: name.to_string(),
        initial,
    }
}

pub struct UndoableProtocol {
    pub append_message: Arc<Event2Class>,
    pub apply_compaction: Arc<Event2Class>,
    pub clear: Arc<Event2Class>,
    pub undo: Arc<Event2Class>,
    pub is_undo_anchor: Arc<dyn Fn(&Value) -> bool + Send + Sync>,
    pub is_valid_undo_count: Arc<dyn Fn(f64) -> bool + Send + Sync>,
}

static UNDOABLE_PROTOCOL: LazyLock<Mutex<Option<UndoableProtocol>>> = LazyLock::new(|| Mutex::new(None));

pub fn register_undoable_protocol(protocol: UndoableProtocol) -> Result<(), StateError> {
    let mut current = UNDOABLE_PROTOCOL.lock();
    if current.is_some() {
        return Err(StateError::new("internal", "The undoable protocol is already registered"));
    }
    *current = Some(protocol);
    let protocol = current.as_ref().unwrap();
    for cls in [
        &protocol.append_message,
        &protocol.apply_compaction,
        &protocol.clear,
        &protocol.undo,
    ] {
        register_event2_class(cls)?;
    }
    Ok(())
}

pub fn expanded_state_folds(key: &ReplayableStateKey) -> Result<HashMap<usize, StateFold>, StateError> {
    let meta = &key.replayable;
    if meta.mutable_replay && meta.undoable.as_ref().and_then(|options| options.on_undo.as_ref()).is_none() {
        return Err(StateError::new(
            "internal",
            format!("State key '{}' requires custom onUndo for mutable replay", key.name),
        ));
    }
    let Some(undoable) = &meta.undoable else {
        return Ok(meta.folds.clone());
    };
    let protocol = UNDOABLE_PROTOCOL.lock();
    let Some(protocol) = protocol.as_ref() else {
        return Err(StateError::new(
            "internal",
            format!(
                "State key '{}' is undoable but no undoable protocol is registered (the contextMemory domain registers it at import time)",
                key.name
            ),
        ));
    };
    let undo_address = Arc::as_ptr(&protocol.undo) as usize;
    if meta.folds.contains_key(&undo_address) {
        return Err(StateError::new(
            "internal",
            format!(
                "Undoable state key '{}' must not fold the undo event itself; use .undoable({{ onUndo }}) to customize the rollback",
                key.name
            ),
        ));
    }
    let custom = undoable.on_undo.is_some();
    let mut folds = meta.folds.clone();
    let domain_append = folds.get(&Arc::as_ptr(&protocol.append_message) as usize).cloned();
    let is_undo_anchor = protocol.is_undo_anchor.clone();
    folds.insert(
        Arc::as_ptr(&protocol.append_message) as usize,
        Arc::new(move |state, event, ctx| {
            if !custom && is_undo_anchor(event.payload.get("message").unwrap_or(&Value::Null)) {
                ctx.checkpoint();
                return None;
            }
            match &domain_append {
                Some(fold) => fold(state, event, ctx),
                None => None,
            }
        }),
    );
    for cls in [&protocol.apply_compaction, &protocol.clear] {
        let domain = folds.get(&Arc::as_ptr(cls) as usize).cloned();
        folds.insert(
            Arc::as_ptr(cls) as usize,
            Arc::new(move |state, event, ctx| {
                ctx.clear_checkpoints();
                match &domain {
                    Some(fold) => fold(state, event, ctx),
                    None => None,
                }
            }),
        );
    }
    let on_undo = undoable.on_undo.clone();
    let is_valid = protocol.is_valid_undo_count.clone();
    folds.insert(
        undo_address,
        Arc::new(move |state, event, ctx| {
            let count = event
                .payload
                .get("count")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(f64::NAN);
            if !is_valid(count) {
                return None;
            }
            if let Some(on_undo) = &on_undo {
                return on_undo(state, event, ctx);
            }
            ctx.undo_to_checkpoint(count as u64);
            None
        }),
    );
    Ok(folds)
}

pub fn expanded_model_appliers(
    owner: &str,
    undoable: bool,
    appliers: &HashMap<usize, EventApplier>,
    on_undo: Option<Arc<dyn Fn(u64) + Send + Sync>>,
) -> Result<HashMap<usize, EventApplier>, StateError> {
    if !undoable {
        return Ok(appliers.clone());
    }
    let protocol = UNDOABLE_PROTOCOL.lock();
    let Some(protocol) = protocol.as_ref() else {
        return Err(StateError::new(
            "internal",
            format!(
                "Agent model '{owner}' is undoable but no undoable protocol is registered (the contextMemory domain registers it at import time)"
            ),
        ));
    };
    let undo_address = Arc::as_ptr(&protocol.undo) as usize;
    if appliers.contains_key(&undo_address) {
        return Err(StateError::new(
            "internal",
            format!("Undoable agent model '{owner}' must not apply the undo event itself; override onUndo on the model to customize the rollback"),
        ));
    }
    let custom = on_undo.is_some();
    let mut expanded = appliers.clone();
    let domain_append = expanded.get(&Arc::as_ptr(&protocol.append_message) as usize).cloned();
    let is_undo_anchor = protocol.is_undo_anchor.clone();
    expanded.insert(
        Arc::as_ptr(&protocol.append_message) as usize,
        Arc::new(move |event, ctx| {
            if !custom && is_undo_anchor(event.payload.get("message").unwrap_or(&Value::Null)) {
                ctx.checkpoint();
                return;
            }
            if let Some(applier) = &domain_append {
                applier(event, ctx);
            }
        }),
    );
    for cls in [&protocol.apply_compaction, &protocol.clear] {
        let domain = expanded.get(&Arc::as_ptr(cls) as usize).cloned();
        expanded.insert(
            Arc::as_ptr(cls) as usize,
            Arc::new(move |event, ctx| {
                ctx.clear_checkpoints();
                if let Some(applier) = &domain {
                    applier(event, ctx);
                }
            }),
        );
    }
    let on_undo = on_undo.clone();
    let is_valid = protocol.is_valid_undo_count.clone();
    expanded.insert(
        undo_address,
        Arc::new(move |event, ctx| {
            let count = event
                .payload
                .get("count")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(f64::NAN);
            if !is_valid(count) {
                return;
            }
            if let Some(on_undo) = &on_undo {
                on_undo(count as u64);
            } else {
                ctx.undo_to_checkpoint(count as u64);
            }
        }),
    );
    Ok(expanded)
}

pub fn expanded_runtime_folds(
    owner: &str,
    undoable: bool,
    folds: &HashMap<usize, StateFold>,
) -> Result<HashMap<usize, StateFold>, StateError> {
    if !undoable {
        return Ok(folds.clone());
    }
    let protocol = UNDOABLE_PROTOCOL.lock();
    let Some(protocol) = protocol.as_ref() else {
        return Err(StateError::new(
            "internal",
            format!(
                "Agent runtime '{owner}' is undoable but no undoable protocol is registered (the contextMemory domain registers it at import time)"
            ),
        ));
    };
    let undo_address = Arc::as_ptr(&protocol.undo) as usize;
    if folds.contains_key(&undo_address) {
        return Err(StateError::new(
            "internal",
            format!("Undoable agent runtime '{owner}' must not fold the undo event itself"),
        ));
    }
    let mut expanded = folds.clone();
    let domain_append = expanded.get(&Arc::as_ptr(&protocol.append_message) as usize).cloned();
    let is_undo_anchor = protocol.is_undo_anchor.clone();
    expanded.insert(
        Arc::as_ptr(&protocol.append_message) as usize,
        Arc::new(move |state, event, ctx| {
            if is_undo_anchor(event.payload.get("message").unwrap_or(&Value::Null)) {
                ctx.checkpoint();
                return None;
            }
            match &domain_append {
                Some(fold) => fold(state, event, ctx),
                None => None,
            }
        }),
    );
    for cls in [&protocol.apply_compaction, &protocol.clear] {
        let domain = expanded.get(&Arc::as_ptr(cls) as usize).cloned();
        expanded.insert(
            Arc::as_ptr(cls) as usize,
            Arc::new(move |state, event, ctx| {
                ctx.clear_checkpoints();
                match &domain {
                    Some(fold) => fold(state, event, ctx),
                    None => None,
                }
            }),
        );
    }
    let is_valid = protocol.is_valid_undo_count.clone();
    expanded.insert(
        undo_address,
        Arc::new(move |_state, event, ctx| {
            let count = event
                .payload
                .get("count")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(f64::NAN);
            if is_valid(count) {
                ctx.undo_to_checkpoint(count as u64);
            }
            None
        }),
    );
    Ok(expanded)
}

pub type DeepReadonly = Value;
