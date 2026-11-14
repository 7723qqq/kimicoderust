use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

pub const INTERRUPTION_REMINDER_VARIANT: &str = "interruption";

pub type InterruptionReminderState = ();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InterruptionReminderRecordedPayload {
    pub agent_id: String,
    pub turn_id: u64,
}

static INTERRUPTION_REMINDER_RECORDED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "interruptionReminder.recorded".to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "turnId"],
            "properties": {
                "agentId": {"type": "string"},
                "turnId": {"type": "integer", "minimum": 0},
            },
        })),
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct InterruptionReminderRecorded {
    event: Event2,
    pub payload: InterruptionReminderRecordedPayload,
}

impl InterruptionReminderRecorded {
    pub const TYPE: &'static str = "interruptionReminder.recorded";
    pub const DURABLE: bool = true;

    pub fn new(payload: InterruptionReminderRecordedPayload) -> Self {
        let event = Event2::new(
            &INTERRUPTION_REMINDER_RECORDED_CLASS,
            serde_json::to_value(&payload).expect("interruption reminder event is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        INTERRUPTION_REMINDER_RECORDED_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for InterruptionReminderRecorded {
    type Target = InterruptionReminderRecordedPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<InterruptionReminderRecorded> for Event2 {
    fn from(event: InterruptionReminderRecorded) -> Self {
        event.event
    }
}

pub static INTERRUPTION_REMINDER_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "interruptionReminder".to_string(),
        Arc::new(|| Value::Null),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    key.on(
        &INTERRUPTION_REMINDER_RECORDED_CLASS,
        Arc::new(|_state, _event, _ctx| None),
    )
    .expect("interruption reminder fold can be registered");
    Arc::new(key)
});

pub fn interruption_reminder_key() -> Arc<ReplayableStateKey> {
    INTERRUPTION_REMINDER_KEY.clone()
}
