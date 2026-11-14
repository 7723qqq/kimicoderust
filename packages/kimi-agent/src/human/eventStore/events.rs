use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::human::store::types::StoreError;

pub type ExternalEvent = Map<String, Value>;
pub type InternalEvent = Map<String, Value>;
pub type EventSchema = Arc<dyn Fn(&Map<String, Value>) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
pub struct EventFactory {
    pub type_: String,
    pub schema: EventSchema,
}

impl EventFactory {
    pub fn create(&self, mut payload: ExternalEvent) -> ExternalEvent {
        payload.insert("type".to_string(), Value::String(self.type_.clone()));
        payload.insert(
            "time".to_string(),
            Value::from(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            ),
        );
        payload
    }
}

pub struct EventDefinition {
    pub type_: String,
    pub schema: EventSchema,
}

static REGISTRY: LazyLock<Mutex<HashMap<String, EventSchema>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn define_event(def: EventDefinition) -> EventFactory {
    let mut registry = REGISTRY.lock();
    if registry.contains_key(&def.type_) {
        std::panic::panic_any(StoreError::new(
            "duplicate-event",
            format!("duplicate event type '{}'", def.type_),
        ));
    }
    registry.insert(def.type_.clone(), def.schema.clone());
    EventFactory {
        type_: def.type_,
        schema: def.schema,
    }
}

pub fn event_schema_for(type_: &str) -> Option<EventSchema> {
    REGISTRY.lock().get(type_).cloned()
}

fn payload_of(event: &ExternalEvent) -> ExternalEvent {
    event
        .iter()
        .filter(|(key, _)| key.as_str() != "type" && key.as_str() != "time")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn parse_event(type_: &str, record: &Value) -> Option<ExternalEvent> {
    let schema = event_schema_for(type_)?;
    let record = record.as_object()?;
    schema(&payload_of(record)).ok()?;
    Some(record.clone())
}

pub fn validate_event(event: &ExternalEvent) -> Option<StoreError> {
    let type_ = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("undefined");
    let Some(schema) = event_schema_for(type_) else {
        return Some(StoreError::new(
            "unregistered-event",
            format!("event '{type_}' is not a registered external event"),
        ));
    };
    if let Err(error) = schema(&payload_of(event)) {
        return Some(StoreError::new(
            "schema",
            format!("event '{type_}' failed schema validation: {error}"),
        ));
    }
    None
}
