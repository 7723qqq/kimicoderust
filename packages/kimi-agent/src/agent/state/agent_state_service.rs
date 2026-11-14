use super::agent_state::{StateChange, StateError, StateInspection, StateInspector, StateKey};
use parking_lot::Mutex;
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

type Listener<A> = Arc<dyn Fn(&A) -> Result<(), StateError> + Send + Sync>;

pub struct Listeners<A: 'static> {
    next_id: AtomicU64,
    items: Mutex<Vec<(u64, Listener<A>)>>,
}

impl<A: 'static> Default for Listeners<A> {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            items: Mutex::new(Vec::new()),
        }
    }
}

impl<A: 'static> Listeners<A> {
    pub fn add<F>(self: &Arc<Self>, listener: F) -> ListenerRegistration<A>
    where
        F: Fn(&A) -> Result<(), StateError> + Send + Sync + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.items.lock().push((id, Arc::new(listener)));
        ListenerRegistration {
            owner: self.clone(),
            id,
        }
    }

    fn remove(&self, id: u64) {
        self.items.lock().retain(|(item, _)| *item != id);
    }

    pub fn fire(&self, value: &A) -> Result<(), StateError> {
        let listeners: Vec<Listener<A>> = self
            .items
            .lock()
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect();
        for listener in listeners {
            listener(value)?;
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.items.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct ListenerRegistration<A: 'static> {
    owner: Arc<Listeners<A>>,
    id: u64,
}

impl<A: 'static> ListenerRegistration<A> {
    pub fn dispose(&self) {
        self.owner.remove(self.id);
    }
}

impl<A: 'static> Drop for ListenerRegistration<A> {
    fn drop(&mut self) {
        self.owner.remove(self.id);
    }
}

struct RegistryEntry {
    name: String,
    value: Value,
    token: u64,
    snapshot_excluded: bool,
}

pub struct AgentStateService {
    entries: Mutex<Vec<RegistryEntry>>,
    next_token: AtomicU64,
    key_emitters: Mutex<Vec<(String, Arc<Listeners<Value>>)>>,
    any_emitter: Arc<Listeners<StateChange>>,
    replayables: Mutex<Vec<StateKey>>,
    contribute_listeners: Arc<Listeners<StateKey>>,
    withdraw_listeners: Arc<Listeners<StateKey>>,
    inspect_scope: String,
    inspect_parent: Option<Arc<dyn StateInspector + Send + Sync>>,
}

impl Default for AgentStateService {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentStateService {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next_token: AtomicU64::new(1),
            key_emitters: Mutex::new(Vec::new()),
            any_emitter: Arc::new(Listeners::default()),
            replayables: Mutex::new(Vec::new()),
            contribute_listeners: Arc::new(Listeners::default()),
            withdraw_listeners: Arc::new(Listeners::default()),
            inspect_scope: "agent".to_string(),
            inspect_parent: None,
        }
    }

    pub fn with_parent(parent: Arc<dyn StateInspector + Send + Sync>) -> Self {
        let mut service = Self::new();
        service.inspect_parent = Some(parent);
        service
    }

    pub fn contribute_state(
        self: &Arc<Self>,
        key: StateKey,
    ) -> Result<StateRegistration, StateError> {
        if key.replayable.is_none() {
            return self.contribute_key(key);
        }
        let registration = self.contribute_key(key.clone())?;
        self.replayables.lock().push(key.clone());
        if let Err(error) = self.contribute_listeners.fire(&key) {
            registration.dispose();
            self.replayables.lock().retain(|item| item.name != key.name);
            let _ = self.withdraw_listeners.fire(&key);
            return Err(error);
        }
        registration.mark_replayable();
        Ok(registration)
    }

    fn contribute_key(self: &Arc<Self>, key: StateKey) -> Result<StateRegistration, StateError> {
        let mut entries = self.entries.lock();
        if entries.iter().any(|entry| entry.name == key.name) {
            return Err(StateError::new(format!(
                "state key '{}' is already registered",
                key.name
            )));
        }
        let token = self.next_token.fetch_add(1, Ordering::SeqCst);
        entries.push(RegistryEntry {
            name: key.name.clone(),
            value: key.value(),
            token,
            snapshot_excluded: key.snapshot_excluded,
        });
        drop(entries);
        Ok(StateRegistration {
            service: Arc::downgrade(self),
            name: key.name,
            token,
            replayable: AtomicBool::new(false),
            disposed: Mutex::new(false),
        })
    }

    pub fn has(&self, key: &StateKey) -> bool {
        self.entries
            .lock()
            .iter()
            .any(|entry| entry.name == key.name)
    }

    pub fn get(&self, key: &StateKey) -> Result<Value, StateError> {
        self.entries
            .lock()
            .iter()
            .find(|entry| entry.name == key.name)
            .map(|entry| entry.value.clone())
            .ok_or_else(|| StateError::new(format!("state key '{}' is not registered", key.name)))
    }

    pub fn set(&self, key: &StateKey, value: Value) -> Result<(), StateError> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|entry| entry.name == key.name)
                .ok_or_else(|| {
                    StateError::new(format!("state key '{}' is not registered", key.name))
                })?;
            entry.value = value.clone();
        }
        if let Some(emitter) = self.key_emitter(&key.name) {
            emitter.fire(&value)?;
        }
        self.any_emitter.fire(&StateChange {
            key: key.name.clone(),
            value,
        })
    }

    pub fn on_did_change(self: &Arc<Self>, key: &StateKey) -> Arc<Listeners<Value>> {
        if let Some(emitter) = self.key_emitter(&key.name) {
            return emitter;
        }
        let emitter = Arc::new(Listeners::default());
        self.key_emitters
            .lock()
            .push((key.name.clone(), emitter.clone()));
        emitter
    }

    fn key_emitter(&self, name: &str) -> Option<Arc<Listeners<Value>>> {
        self.key_emitters
            .lock()
            .iter()
            .find(|(item, _)| item == name)
            .map(|(_, emitter)| emitter.clone())
    }

    pub fn on_did_change_any(&self) -> Arc<Listeners<StateChange>> {
        self.any_emitter.clone()
    }

    pub fn entries(&self) -> Vec<(String, Value)> {
        self.entries
            .lock()
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect()
    }

    pub fn snapshot(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for entry in self.entries.lock().iter() {
            if entry.snapshot_excluded {
                continue;
            }
            out.insert(entry.name.clone(), entry.value.clone());
        }
        out
    }

    pub fn replayable_keys(&self) -> Vec<StateKey> {
        self.replayables.lock().clone()
    }

    pub fn on_did_contribute_replayable(self: &Arc<Self>) -> Arc<Listeners<StateKey>> {
        self.contribute_listeners.clone()
    }

    pub fn on_did_withdraw_replayable(self: &Arc<Self>) -> Arc<Listeners<StateKey>> {
        self.withdraw_listeners.clone()
    }

    fn withdraw_key(&self, name: &str, token: u64, replayable: bool) {
        {
            let mut entries = self.entries.lock();
            let Some(index) = entries
                .iter()
                .position(|entry| entry.name == name && entry.token == token)
            else {
                return;
            };
            entries.remove(index);
        }
        self.key_emitters.lock().retain(|(item, _)| item != name);
        if !replayable {
            return;
        }
        let withdrawn = {
            let mut replayables = self.replayables.lock();
            match replayables.iter().position(|item| item.name == name) {
                Some(index) => Some(replayables.remove(index)),
                None => None,
            }
        };
        if let Some(key) = withdrawn {
            let _ = self.withdraw_listeners.fire(&key);
        }
    }
}

impl StateInspector for AgentStateService {
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

#[derive(Debug)]
pub struct StateRegistration {
    service: Weak<AgentStateService>,
    name: String,
    token: u64,
    replayable: AtomicBool,
    disposed: Mutex<bool>,
}

impl StateRegistration {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn mark_replayable(&self) {
        self.replayable.store(true, Ordering::SeqCst);
    }

    pub fn dispose(&self) {
        let mut disposed = self.disposed.lock();
        if *disposed {
            return;
        }
        *disposed = true;
        if let Some(service) = self.service.upgrade() {
            service.withdraw_key(
                &self.name,
                self.token,
                self.replayable.load(Ordering::SeqCst),
            );
        }
    }
}

impl Drop for StateRegistration {
    fn drop(&mut self) {
        self.dispose();
    }
}
