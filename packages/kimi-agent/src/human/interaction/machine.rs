use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;

use crate::human::interaction::interaction::{
    INTERACTION_TAG_SESSION_ID, Interaction, InteractionTagValue,
};

#[derive(Debug, Clone, PartialEq)]
pub struct InteractionRecord {
    pub interaction: Interaction,
    pub resolved: bool,
    pub response: Option<Value>,
}

impl InteractionRecord {
    pub fn id(&self) -> &str {
        &self.interaction.id
    }
}

#[derive(Clone)]
pub enum InteractionEvent {
    Request { record: InteractionRecord },
    Resolve { id: String, response: Value },
    Evict { id: String },
    Purge { session_id: String },
}

#[derive(Clone)]
pub enum InteractionEmitted {
    Requested { record: InteractionRecord },
    Resolved { id: String, response: Value, record: InteractionRecord },
}

impl InteractionEmitted {
    pub fn type_(&self) -> &'static str {
        match self {
            Self::Requested { .. } => "interaction.requested",
            Self::Resolved { .. } => "interaction.resolved",
        }
    }
}

#[derive(Clone, Default)]
pub struct InteractionMachineContext {
    pub records: IndexMap<String, InteractionRecord>,
}

pub struct InteractionSnapshot {
    pub context: InteractionMachineContext,
}

pub struct InteractionMachineLogic;

impl InteractionMachineLogic {
    pub fn start(&self) -> Arc<InteractionActor> {
        Arc::new(InteractionActor {
            context: Mutex::new(InteractionMachineContext::default()),
            listeners: Mutex::new(IndexMap::new()),
            next_listener_id: AtomicU64::new(0),
        })
    }
}

type Listener = Arc<dyn Fn(InteractionEmitted) + Send + Sync>;

pub struct InteractionActor {
    context: Mutex<InteractionMachineContext>,
    listeners: Mutex<IndexMap<u64, (String, Listener)>>,
    next_listener_id: AtomicU64,
}

pub struct InteractionSubscription {
    unsubscribe: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl InteractionSubscription {
    pub fn unsubscribe(&self) {
        if let Some(unsubscribe) = self.unsubscribe.lock().take() {
            unsubscribe();
        }
    }
}

impl InteractionActor {
    pub fn send(&self, event: InteractionEvent) {
        match event {
            InteractionEvent::Request { record } => {
                if self
                    .context
                    .lock()
                    .records
                    .get(record.id())
                    .is_some_and(|existing| !existing.resolved)
                {
                    return;
                }
                self.context
                    .lock()
                    .records
                    .insert(record.id().to_string(), record.clone());
                self.emit(InteractionEmitted::Requested { record });
            }
            InteractionEvent::Resolve { id, response } => {
                let record = {
                    let mut context = self.context.lock();
                    let Some(existing) = context.records.get(&id).cloned() else {
                        return;
                    };
                    if existing.resolved {
                        return;
                    }
                    let record = InteractionRecord {
                        interaction: existing.interaction,
                        resolved: true,
                        response: Some(response.clone()),
                    };
                    context.records.insert(id.clone(), record.clone());
                    record
                };
                self.emit(InteractionEmitted::Resolved {
                    id,
                    response,
                    record,
                });
            }
            InteractionEvent::Purge { session_id } => {
                self.context.lock().records.retain(|_, record| {
                    record.interaction.tags.get(INTERACTION_TAG_SESSION_ID)
                        != Some(&InteractionTagValue::String(session_id.clone()))
                });
            }
            InteractionEvent::Evict { id } => {
                if self
                    .context
                    .lock()
                    .records
                    .get(&id)
                    .is_some_and(|record| record.resolved)
                {
                    self.context.lock().records.remove(&id);
                }
            }
        }
    }

    pub fn get_snapshot(&self) -> InteractionSnapshot {
        InteractionSnapshot {
            context: self.context.lock().clone(),
        }
    }

    pub fn on(
        self: &Arc<Self>,
        type_: &str,
        listener: Listener,
    ) -> InteractionSubscription {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, (type_.to_string(), listener));
        let weak = Arc::downgrade(self);
        InteractionSubscription {
            unsubscribe: Mutex::new(Some(Box::new(move || {
                if let Some(actor) = Weak::upgrade(&weak) {
                    actor.listeners.lock().remove(&id);
                }
            }))),
        }
    }

    fn emit(&self, event: InteractionEmitted) {
        let type_ = event.type_();
        for (wanted, listener) in self.listeners.lock().values() {
            if wanted == type_ {
                listener(event.clone());
            }
        }
    }
}

pub fn create_interaction_machine() -> InteractionMachineLogic {
    InteractionMachineLogic
}
