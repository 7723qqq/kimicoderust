use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::{Map, Value};

#[derive(Clone)]
pub struct Event2Class {
    pub type_: String,
    pub durable: bool,
    pub observable: bool,
    pub agent_domain: bool,
    pub schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event2 {
    pub type_: String,
    pub time: i64,
    pub payload: Value,
}

impl Event2 {
    pub fn new(cls: &Event2Class, payload: Value, time: Option<i64>) -> Self {
        Self {
            type_: cls.type_.clone(),
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            payload,
        }
    }

    pub fn serialize(&self) -> Value {
        let mut record = match &self.payload {
            Value::Object(payload) => payload.clone(),
            _ => Map::new(),
        };
        record.insert("type".to_string(), Value::String(self.type_.clone()));
        record.insert("time".to_string(), Value::from(self.time));
        Value::Object(record)
    }
}

pub static EVENT2_REGISTRY: LazyLock<Mutex<std::collections::HashMap<String, Arc<Event2Class>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

pub fn register_event2_class(cls: &Arc<Event2Class>) -> Result<(), String> {
    if !cls.durable {
        return Ok(());
    }
    if cls.schema.is_none() {
        return Err(format!(
            "Durable event '{}' must declare a payload schema",
            cls.type_
        ));
    }
    let mut registry = EVENT2_REGISTRY.lock();
    if let Some(existing) = registry.get(&cls.type_) {
        if Arc::ptr_eq(existing, cls) {
            return Ok(());
        }
        return Err(format!("Duplicate event type registered: '{}'", cls.type_));
    }
    registry.insert(cls.type_.clone(), cls.clone());
    Ok(())
}

pub fn event2_from_record(cls: &Arc<Event2Class>, record: &Value) -> Option<Event2> {
    let object = record.as_object()?;
    let payload = object
        .iter()
        .filter(|(key, _)| key.as_str() != "type" && key.as_str() != "time")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<String, Value>>();
    let schema = cls.schema.as_ref()?;
    let payload = validate_event2_payload(schema, &Value::Object(payload))?;
    Some(Event2::new(
        cls,
        payload,
        object.get("time").and_then(Value::as_i64),
    ))
}

pub fn is_observable_durable_record_type(type_: &str) -> bool {
    EVENT2_REGISTRY
        .lock()
        .get(type_)
        .map(|cls| cls.observable)
        .unwrap_or(false)
}

fn validate_event2_payload(schema: &Value, value: &Value) -> Option<Value> {
    if let Some(expected) = schema.get("const") {
        return (expected == value).then_some(value.clone());
    }
    if let Some(expected) = schema.get("enum").and_then(Value::as_array) {
        return expected.contains(value).then_some(value.clone());
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value.as_object()?;
            for required in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !object.contains_key(required) {
                    return None;
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            let additional = schema.get("additionalProperties");
            if additional == Some(&Value::Bool(false))
                && object
                    .keys()
                    .any(|key| properties.is_some_and(|properties| !properties.contains_key(key)))
            {
                return None;
            }
            let mut output = Map::new();
            for (key, value) in object {
                if let Some(property_schema) = properties.and_then(|properties| properties.get(key))
                {
                    output.insert(
                        key.clone(),
                        validate_event2_payload(property_schema, value)?,
                    );
                } else if let Some(additional_schema) =
                    additional.filter(|_| additional != Some(&Value::Bool(false)))
                {
                    if additional_schema.is_object() {
                        output.insert(
                            key.clone(),
                            validate_event2_payload(additional_schema, value)?,
                        );
                    } else {
                        output.insert(key.clone(), value.clone());
                    }
                } else if additional == Some(&Value::Bool(true)) {
                    output.insert(key.clone(), value.clone());
                }
            }
            Some(Value::Object(output))
        }
        Some("array") => {
            let items = value.as_array()?;
            let item_schema = schema.get("items");
            Some(Value::Array(
                items
                    .iter()
                    .map(|item| {
                        item_schema
                            .map(|schema| validate_event2_payload(schema, item))
                            .unwrap_or_else(|| Some(item.clone()))
                    })
                    .collect::<Option<Vec<_>>>()?,
            ))
        }
        Some("string") => value.as_str().map(|_| value.clone()),
        Some("number") | Some("integer") => {
            let number = value.as_f64()?;
            if !number.is_finite()
                || schema
                    .get("minimum")
                    .and_then(Value::as_f64)
                    .is_some_and(|minimum| number < minimum)
                || schema
                    .get("exclusiveMinimum")
                    .and_then(Value::as_f64)
                    .is_some_and(|minimum| number <= minimum)
                || schema
                    .get("maximum")
                    .and_then(Value::as_f64)
                    .is_some_and(|maximum| number > maximum)
                || schema
                    .get("exclusiveMaximum")
                    .and_then(Value::as_f64)
                    .is_some_and(|maximum| number >= maximum)
            {
                return None;
            }
            if schema.get("type").and_then(Value::as_str) == Some("integer")
                && number.fract() != 0.0
            {
                return None;
            }
            Some(value.clone())
        }
        Some("boolean") => value.as_bool().map(|_| value.clone()),
        Some("null") => value.is_null().then_some(Value::Null),
        _ => Some(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn class(type_: &str) -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: type_.to_string(),
            durable: true,
            observable: true,
            agent_domain: true,
            schema: Some(json!({
                "type": "object",
                "required": ["id"],
                "properties": {"id": {"type": "string"}},
            })),
        })
    }

    #[test]
    fn registration_is_idempotent_only_for_the_same_class() {
        let first = class("test.event2.registration");
        register_event2_class(&first).unwrap();
        register_event2_class(&first).unwrap();
        let duplicate = class("test.event2.registration");
        assert!(register_event2_class(&duplicate).is_err());
        assert!(is_observable_durable_record_type(
            "test.event2.registration"
        ));
    }

    #[test]
    fn from_record_validates_and_strips_payload() {
        let class = class("test.event2.decode");
        let event = event2_from_record(
            &class,
            &json!({"type": "ignored", "time": 42, "id": "a", "extra": true}),
        )
        .unwrap();
        assert_eq!(event.time, 42);
        assert_eq!(event.payload, json!({"id": "a"}));
        assert_eq!(event.serialize()["type"], "test.event2.decode");
        assert!(event2_from_record(&class, &json!({"extra": true})).is_none());
    }
}
