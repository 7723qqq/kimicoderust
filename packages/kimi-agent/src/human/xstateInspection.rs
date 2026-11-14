use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde_json::Value;

pub type XstateInspectionEventType = String;
pub type XstateInspectionListener = Arc<dyn Fn(XstateInspectionEnvelope) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct XstateInspectionEnvelope {
    pub type_: XstateInspectionEventType,
    pub timestamp: i64,
    pub actor_session_id: String,
    pub actor_id: Option<String>,
    pub logic_id: Option<String>,
    pub event_type: Option<String>,
    pub state_value: Option<Value>,
    pub unhandled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct XstateInspectionLogic {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct XstateInspectionActorRef {
    pub session_id: String,
    pub id: Option<String>,
    pub logic: Option<XstateInspectionLogic>,
}

#[derive(Debug, Clone, Default)]
pub struct XstateInspectionEvent {
    pub type_: String,
    pub actor_ref: XstateInspectionActorRef,
    pub snapshot_value: Option<Value>,
    pub event_type: Option<String>,
    pub action_type: Option<String>,
    pub transition_count: usize,
}

pub trait XstateInspectionCollector: Send + Sync {
    fn subscribe(&self, listener: XstateInspectionListener) -> Box<dyn FnOnce() + Send>;
    fn publish(&self, event: XstateInspectionEvent);
}

pub struct XstateInspectionCollectorImpl {
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    listeners: Arc<Mutex<BTreeMap<u64, XstateInspectionListener>>>,
    next_listener_id: AtomicU64,
}

impl XstateInspectionCollectorImpl {
    fn new(now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>) -> Self {
        Self {
            now: now.unwrap_or_else(|| Arc::new(|| chrono::Utc::now().timestamp_millis())),
            listeners: Arc::new(Mutex::new(BTreeMap::new())),
            next_listener_id: AtomicU64::new(0),
        }
    }
}

impl XstateInspectionCollector for XstateInspectionCollectorImpl {
    fn subscribe(&self, listener: XstateInspectionListener) -> Box<dyn FnOnce() + Send> {
        let id = {
            let mut listeners = self.listeners.lock();
            if let Some(id) = listeners
                .iter()
                .find(|(_, current)| Arc::ptr_eq(current, &listener))
                .map(|(id, _)| *id)
            {
                id
            } else {
                let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
                listeners.insert(id, listener);
                id
            }
        };
        let listeners = self.listeners.clone();
        Box::new(move || {
            listeners.lock().remove(&id);
        })
    }

    fn publish(&self, event: XstateInspectionEvent) {
        if self.listeners.lock().is_empty() {
            return;
        }
        let unhandled = event.type_ == "@xstate.microstep"
            && event.transition_count == 0
            && event
                .event_type
                .as_deref()
                .is_some_and(|event_type| !event_type.starts_with("xstate."));
        let envelope = XstateInspectionEnvelope {
            type_: event.type_.clone(),
            timestamp: (self.now)(),
            actor_session_id: event.actor_ref.session_id,
            actor_id: event.actor_ref.id,
            logic_id: event.actor_ref.logic.and_then(|logic| logic.id),
            event_type: if event.event_type.is_some() {
                event.event_type
            } else if event.type_ == "@xstate.action" {
                event.action_type
            } else {
                None
            },
            state_value: event.snapshot_value,
            unhandled: unhandled.then_some(true),
        };
        let mut previous_id = None;
        loop {
            let next = self
                .listeners
                .lock()
                .iter()
                .find(|(id, _)| previous_id.is_none_or(|previous| **id > previous))
                .map(|(id, listener)| (*id, listener.clone()));
            let Some((id, listener)) = next else {
                break;
            };
            listener(envelope.clone());
            previous_id = Some(id);
        }
    }
}

pub fn create_xstate_inspection_collector(
    now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
) -> Arc<dyn XstateInspectionCollector> {
    Arc::new(XstateInspectionCollectorImpl::new(now))
}

pub static XSTATE_INSPECTION_COLLECTOR: LazyLock<Arc<dyn XstateInspectionCollector>> =
    LazyLock::new(|| create_xstate_inspection_collector(None));
