use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable, to_disposable};
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::event::{Emitter, Event};
use crate::state::state::StateKeyBuilder;

pub struct StateKey<T> {
    pub name: String,
    pub initial: Arc<dyn Fn() -> T + Send + Sync>,
    pub snapshot_excluded: bool,
    pub replayable: Option<DynamicValue>,
}

impl<T> Clone for StateKey<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            initial: self.initial.clone(),
            snapshot_excluded: self.snapshot_excluded,
            replayable: self.replayable.clone(),
        }
    }
}

impl<T> StateKey<T> {
    pub fn new(name: impl Into<String>, initial: impl Fn() -> T + Send + Sync + 'static) -> Self {
        Self {
            name: name.into(),
            initial: Arc::new(initial),
            snapshot_excluded: false,
            replayable: None,
        }
    }
}

#[derive(Clone)]
pub struct StateChange {
    pub key: String,
    pub value: DynamicValue,
}

#[derive(Clone)]
pub struct StateInspection {
    pub scope: String,
    pub state: HashMap<String, serde_json::Value>,
    pub parent: Option<Box<StateInspection>>,
}

trait KeyEmitter: Send + Sync {
    fn fire(&self, value: DynamicValue);
    fn dispose(&self);
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

struct TypedEmitter<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    emitter: Emitter<T>,
}

impl<T> KeyEmitter for TypedEmitter<T>
where
    T: Any + Clone + Send + Sync + 'static,
{
    fn fire(&self, value: DynamicValue) {
        let value = Arc::downcast::<T>(value)
            .expect("state value type does not match its key")
            .as_ref()
            .clone();
        self.emitter.fire(value);
    }

    fn dispose(&self) {
        self.emitter.dispose();
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

pub trait IStateRegistry: Send + Sync {
    fn contribute_state_dynamic(
        &self,
        key: StateKey<DynamicValue>,
    ) -> Result<DisposableRef, BugIndicatingError>;
    fn has_dynamic(&self, key: &StateKey<DynamicValue>) -> bool;
    fn get_dynamic(&self, key: &StateKey<DynamicValue>) -> DynamicValue;
    fn set_dynamic(&self, key: &StateKey<DynamicValue>, value: DynamicValue);
    fn snapshot(&self) -> HashMap<String, serde_json::Value>;
    fn inspect(&self) -> StateInspection;
}

pub trait IStateRegistryExt: IStateRegistry {
    fn contribute_state<T>(
        &self,
        key: &StateKeyBuilder<T>,
    ) -> Result<DisposableRef, BugIndicatingError>
    where
        T: Any + Send + Sync + 'static,
    {
        self.contribute_state_dynamic(key.erased_key())
    }

    fn has<T>(&self, key: &StateKeyBuilder<T>) -> bool
    where
        T: Any + Send + Sync + 'static,
    {
        self.has_dynamic(&key.erased_key())
    }

    fn get<T>(&self, key: &StateKeyBuilder<T>) -> T
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        Arc::downcast::<T>(self.get_dynamic(&key.erased_key()))
            .expect("state value type does not match its key")
            .as_ref()
            .clone()
    }

    fn set<T>(&self, key: &StateKeyBuilder<T>, value: T)
    where
        T: Any + Send + Sync + 'static,
    {
        self.set_dynamic(&key.erased_key(), Arc::new(value));
    }
}

static NEXT_REGISTRATION_ID: AtomicU64 = AtomicU64::new(1);

pub struct StateRegistry {
    values: Mutex<IndexMap<String, DynamicValue>>,
    registrations: Mutex<HashMap<String, u64>>,
    excluded_from_snapshot: Mutex<HashSet<String>>,
    key_emitters: Mutex<HashMap<String, Arc<dyn KeyEmitter>>>,
    any_emitter: Arc<Emitter<StateChange>>,
    pub on_did_change_any: Event<StateChange>,
    store: DisposableStore,
    this: OnceLock<Weak<StateRegistry>>,
    pub(crate) inspect_scope: String,
    pub(crate) inspect_parent: Option<Arc<dyn IStateRegistry>>,
}

impl StateRegistry {
    pub fn new() -> Arc<Self> {
        Self::with_inspect("unknown", None)
    }

    pub fn with_inspect(
        scope: impl Into<String>,
        parent: Option<Arc<dyn IStateRegistry>>,
    ) -> Arc<Self> {
        let any_emitter = Arc::new(Emitter::new());
        let store = DisposableStore::new();
        let close_emitter = any_emitter.clone();
        store.add(to_disposable(move || close_emitter.dispose()));
        let registry = Arc::new(Self {
            values: Mutex::new(IndexMap::new()),
            registrations: Mutex::new(HashMap::new()),
            excluded_from_snapshot: Mutex::new(HashSet::new()),
            key_emitters: Mutex::new(HashMap::new()),
            on_did_change_any: any_emitter.event(),
            any_emitter,
            store,
            this: OnceLock::new(),
            inspect_scope: scope.into(),
            inspect_parent: parent,
        });
        let _ = registry.this.set(Arc::downgrade(&registry));
        registry
    }

    pub fn contribute_state<T>(
        &self,
        key: StateKey<T>,
    ) -> Result<DisposableRef, BugIndicatingError>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        if key.replayable.as_ref().is_some_and(replayable_is_object) {
            return Err(BugIndicatingError::new(format!(
                "replayable state key '{}' must be contributed to the Agent-scope state service",
                key.name
            )));
        }
        self.contribute_key(key)
    }

    pub fn contribute_key<T>(
        &self,
        key: StateKey<T>,
    ) -> Result<DisposableRef, BugIndicatingError>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        if self.values.lock().contains_key(&key.name) {
            return Err(BugIndicatingError::new(format!(
                "state key '{}' is already registered",
                key.name
            )));
        }
        let Some(registry) = self.this.get().cloned() else {
            return Err(BugIndicatingError::new(
                "state registry is not attached to an Arc",
            ));
        };
        let registration = NEXT_REGISTRATION_ID.fetch_add(1, Ordering::Relaxed);
        self.registrations
            .lock()
            .insert(key.name.clone(), registration);
        self.values.lock().insert(
            key.name.clone(),
            Arc::new((key.initial)()) as DynamicValue,
        );
        if key.snapshot_excluded {
            self.excluded_from_snapshot.lock().insert(key.name.clone());
        }
        let name = key.name;
        Ok(to_disposable(move || {
            let Some(registry) = registry.upgrade() else {
                return;
            };
            if registry.registrations.lock().get(&name) != Some(&registration) {
                return;
            }
            registry.registrations.lock().remove(&name);
            registry.values.lock().shift_remove(&name);
            registry.excluded_from_snapshot.lock().remove(&name);
            if let Some(emitter) = registry.key_emitters.lock().remove(&name) {
                emitter.dispose();
            }
        }))
    }

    pub fn has<T>(&self, key: &StateKey<T>) -> bool {
        self.values.lock().contains_key(&key.name)
    }

    pub fn get<T>(&self, key: &StateKey<T>) -> Result<T, BugIndicatingError>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        let value = self
            .values
            .lock()
            .get(&key.name)
            .cloned()
            .ok_or_else(|| {
                BugIndicatingError::new(format!("state key '{}' is not registered", key.name))
            })?;
        Ok(Arc::downcast::<T>(value)
            .expect("state value type does not match its key")
            .as_ref()
            .clone())
    }

    pub fn set<T>(&self, key: &StateKey<T>, value: T) -> Result<(), BugIndicatingError>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        if !self.values.lock().contains_key(&key.name) {
            return Err(BugIndicatingError::new(format!(
                "state key '{}' is not registered",
                key.name
            )));
        }
        let value = Arc::new(value) as DynamicValue;
        self.values.lock().insert(key.name.clone(), value.clone());
        let emitter = self.key_emitters.lock().get(&key.name).cloned();
        if let Some(emitter) = emitter {
            emitter.fire(value.clone());
        }
        self.any_emitter.fire(StateChange {
            key: key.name.clone(),
            value,
        });
        Ok(())
    }

    pub fn on_did_change<T>(&self, key: &StateKey<T>) -> Event<T>
    where
        T: Any + Clone + Send + Sync + 'static,
    {
        let mut emitters = self.key_emitters.lock();
        let emitter = emitters
            .entry(key.name.clone())
            .or_insert_with(|| {
                let emitter = Arc::new(TypedEmitter::<T> {
                    emitter: Emitter::new(),
                });
                let close = emitter.clone();
                self.store.add(to_disposable(move || close.dispose()));
                emitter
            });
        Arc::downcast::<TypedEmitter<T>>(emitter.clone().into_any())
            .expect("state event type does not match its key")
            .emitter
            .event()
    }

    pub fn entries(&self) -> Vec<(String, DynamicValue)> {
        self.values
            .lock()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    pub fn dispose(&self) {
        self.store.dispose();
    }
}

impl IStateRegistry for StateRegistry {
    fn contribute_state_dynamic(
        &self,
        key: StateKey<DynamicValue>,
    ) -> Result<DisposableRef, BugIndicatingError> {
        self.contribute_state(key)
    }

    fn has_dynamic(&self, key: &StateKey<DynamicValue>) -> bool {
        self.has(key)
    }

    fn get_dynamic(&self, key: &StateKey<DynamicValue>) -> DynamicValue {
        self.get(key).unwrap_or_else(|error| panic!("{error}"))
    }

    fn set_dynamic(&self, key: &StateKey<DynamicValue>, value: DynamicValue) {
        self.set(key, value)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    fn snapshot(&self) -> HashMap<String, serde_json::Value> {
        let excluded = self.excluded_from_snapshot.lock().clone();
        self.values
            .lock()
            .iter()
            .filter(|(key, _)| !excluded.contains(*key))
            .map(|(key, value)| {
                let mut seen = HashSet::new();
                (key.clone(), to_json_safe(value, &mut seen))
            })
            .collect()
    }

    fn inspect(&self) -> StateInspection {
        StateInspection {
            scope: self.inspect_scope.clone(),
            state: self.snapshot(),
            parent: self
                .inspect_parent
                .as_ref()
                .map(|parent| Box::new(parent.inspect())),
        }
    }
}

impl IDisposable for StateRegistry {
    fn dispose(&self) {
        StateRegistry::dispose(self);
    }
}

pub fn replayable_is_object(value: &DynamicValue) -> bool {
    if let Some(value) = value.downcast_ref::<serde_json::Value>() {
        return matches!(
            value,
            serde_json::Value::Array(_) | serde_json::Value::Object(_)
        );
    }
    true
}

fn to_json_safe(
    value: &DynamicValue,
    seen: &mut HashSet<usize>,
) -> serde_json::Value {
    let identity = Arc::as_ptr(value) as *const () as usize;
    if !seen.insert(identity) {
        return serde_json::Value::String("(circular)".to_string());
    }
    let result = to_json_safe_inner(value, seen);
    seen.remove(&identity);
    result
}

fn to_json_safe_inner(
    value: &DynamicValue,
    seen: &mut HashSet<usize>,
) -> serde_json::Value {
    if let Some(value) = value.downcast_ref::<serde_json::Value>() {
        return match value {
            serde_json::Value::Array(values) => serde_json::Value::Array(
                values
                    .iter()
                    .map(|value| {
                        to_json_safe(&(Arc::new(value.clone()) as DynamicValue), seen)
                    })
                    .collect(),
            ),
            serde_json::Value::Object(values) => serde_json::Value::Object(
                values
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            to_json_safe(&(Arc::new(value.clone()) as DynamicValue), seen),
                        )
                    })
                    .collect(),
            ),
            _ => value.clone(),
        };
    }
    if let Some(value) = value.downcast_ref::<String>() {
        return serde_json::Value::String(value.clone());
    }
    if let Some(value) = value.downcast_ref::<bool>() {
        return serde_json::Value::Bool(*value);
    }
    if let Some(value) = value.downcast_ref::<i64>() {
        return serde_json::Value::Number((*value).into());
    }
    if let Some(value) = value.downcast_ref::<u64>() {
        return serde_json::Value::Number((*value).into());
    }
    if let Some(value) = value.downcast_ref::<f64>() {
        return serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
    }
    if let Some(value) = value.downcast_ref::<chrono::DateTime<chrono::Utc>>() {
        return serde_json::Value::String(value.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
    }
    if let Some(values) = value.downcast_ref::<Vec<DynamicValue>>() {
        return serde_json::Value::Array(
            values
                .iter()
                .map(|value| to_json_safe(value, seen))
                .collect(),
        );
    }
    if let Some(values) = value.downcast_ref::<HashMap<String, DynamicValue>>() {
        return serde_json::Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), to_json_safe(value, seen)))
                .collect(),
        );
    }
    serde_json::Value::String("(object)".to_string())
}
