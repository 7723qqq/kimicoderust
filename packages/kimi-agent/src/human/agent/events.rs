use std::sync::{Arc, LazyLock};

use serde_json::Value;

use crate::human::agent::historySchema::{
    history_message_schema, system_entry_schema, system_message_schema, user_entry_schema,
    user_message_schema,
};
use crate::human::eventStore::events::{
    EventDefinition, EventFactory, ExternalEvent, define_event,
};

fn factory(type_: &str, schema: impl Fn(&serde_json::Map<String, Value>) -> Result<(), String> + Send + Sync + 'static) -> EventFactory {
    define_event(EventDefinition {
        type_: type_.to_string(),
        schema: Arc::new(schema),
    })
}

fn integer(value: &Value) -> bool {
    value.as_f64().is_some_and(|value| value.fract() == 0.0)
}

fn optional_string(obj: &serde_json::Map<String, Value>, key: &str) -> Result<(), String> {
    if obj.get(key).is_some_and(|value| !value.is_string()) {
        return Err(format!("expected '{key}' to be a string"));
    }
    Ok(())
}

fn required_string(obj: &serde_json::Map<String, Value>, key: &str) -> Result<(), String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(|_| ())
        .ok_or_else(|| format!("expected '{key}' to be a string"))
}

pub static MESSAGE_APPENDED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("message.appended", |payload| {
        history_message_schema(payload.get("message").ok_or_else(|| "missing message".to_string())?)
    })
});
pub type MessageAppended = ExternalEvent;

pub static TURN_STARTED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("turn.started", |payload| {
        if !payload.get("turnId").is_some_and(integer) {
            return Err("expected turnId to be an integer".to_string());
        }
        optional_string(payload, "queueItemId")
    })
});
pub type TurnStarted = ExternalEvent;

pub static TURN_ENDED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("turn.ended", |payload| {
        if !payload.get("turnId").is_some_and(integer) {
            return Err("expected turnId to be an integer".to_string());
        }
        if !payload.get("outcome").and_then(Value::as_str).is_some_and(|outcome| {
            matches!(outcome, "done" | "failed" | "aborted")
        }) {
            return Err("invalid outcome".to_string());
        }
        optional_string(payload, "errorMessage")
    })
});
pub type TurnEnded = ExternalEvent;

pub static INPUT_SUBMITTED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("input.submitted", |payload| {
        if let Some(entry) = payload.get("entry")
            && user_entry_schema(entry).is_ok()
        {
            return Ok(());
        }
        optional_string(payload, "id")?;
        user_message_schema(payload.get("message").ok_or_else(|| "missing message".to_string())?)
    })
});
pub type InputSubmitted = ExternalEvent;

pub static INPUT_NOTIFIED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("input.notified", |payload| {
        if let Some(entry) = payload.get("entry")
            && user_entry_schema(entry).is_ok()
        {
            return Ok(());
        }
        optional_string(payload, "source")?;
        user_message_schema(payload.get("message").ok_or_else(|| "missing message".to_string())?)
    })
});
pub type InputNotified = ExternalEvent;

pub static INPUT_REMINDED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("input.reminded", |payload| {
        required_string(payload, "key")?;
        let message = payload.get("message").ok_or_else(|| "missing message".to_string())?;
        if user_entry_schema(message).is_ok()
            || system_entry_schema(message).is_ok()
            || user_message_schema(message).is_ok()
            || system_message_schema(message).is_ok()
        {
            Ok(())
        } else {
            Err("invalid reminder message".to_string())
        }
    })
});
pub type InputReminded = ExternalEvent;

pub static INPUT_STEERED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("input.steered", |payload| {
        required_string(payload, "id")?;
        user_message_schema(payload.get("message").ok_or_else(|| "missing message".to_string())?)
    })
});
pub type InputSteered = ExternalEvent;

pub static INPUT_CANCELLED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("input.cancelled", |payload| required_string(payload, "id"))
});
pub type InputCancelled = ExternalEvent;

pub static QUEUE_DRAINED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("queue.drained", |payload| optional_string(payload, "id"))
});
pub type QueueDrained = ExternalEvent;

pub static INPUT_DRAINED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("input.drained", |_| Ok(()))
});
pub type InputDrained = ExternalEvent;

pub static NOTIFICATIONS_DRAINED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("notifications.drained", |_| Ok(()))
});
pub type NotificationsDrained = ExternalEvent;

pub static STATE_UPDATED: LazyLock<EventFactory> = LazyLock::new(|| {
    factory("state.updated", |payload| required_string(payload, "name"))
});
pub type StateUpdated = ExternalEvent;
