use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::human::interaction::interaction::InteractionKind;

pub const INTERACTION_REQUEST_EVENT: &str = "interaction.request";
pub const INTERACTION_RESOLVED_EVENT: &str = "interaction.resolved";

pub fn interaction_request_event(
    agent_id: &str,
    id: &str,
    kind: &str,
    tool_call_id: Option<&str>,
    request: Value,
) -> Value {
    let mut event = json!({
        "type": INTERACTION_REQUEST_EVENT,
        "agentId": agent_id,
        "id": id,
        "kind": kind,
        "request": request,
    });
    if let Some(tool_call_id) = tool_call_id {
        event["toolCallId"] = json!(tool_call_id);
    }
    event
}

pub fn interaction_resolved_event(agent_id: &str, id: &str, response: Value) -> Value {
    json!({
        "type": INTERACTION_RESOLVED_EVENT,
        "agentId": agent_id,
        "id": id,
        "response": response,
    })
}

pub static INTERACTION_REQUEST_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: INTERACTION_REQUEST_EVENT.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "id", "kind"],
            "properties": {
                "agentId": {"type": "string"},
                "id": {"type": "string"},
                "kind": {"enum": ["approval", "question", "user_tool"]},
                "toolCallId": {"type": "string"},
                "request": {},
            },
        })),
    })
});

pub static INTERACTION_RESOLVED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: INTERACTION_RESOLVED_EVENT.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "id"],
            "properties": {
                "agentId": {"type": "string"},
                "id": {"type": "string"},
                "response": {},
            },
        })),
    })
});

pub fn interaction_event_classes() -> Vec<Arc<Event2Class>> {
    vec![
        INTERACTION_REQUEST_CLASS.clone(),
        INTERACTION_RESOLVED_CLASS.clone(),
    ]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionRequestEventPayload {
    pub agent_id: String,
    pub id: String,
    pub kind: InteractionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub request: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionResolvedEventPayload {
    pub agent_id: String,
    pub id: String,
    pub response: Value,
}

macro_rules! interaction_event {
    ($name:ident, $payload:ty, $class:ident) => {
        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("interaction event is serializable"),
                    None,
                );
                Self { event, payload }
            }

            pub fn class() -> Arc<Event2Class> {
                $class.clone()
            }

            pub fn as_event(&self) -> &Event2 {
                &self.event
            }
        }

        impl Deref for $name {
            type Target = $payload;

            fn deref(&self) -> &Self::Target {
                &self.payload
            }
        }

        impl From<$name> for Event2 {
            fn from(event: $name) -> Self {
                event.event
            }
        }
    };
}

interaction_event!(
    InteractionRequestEvent,
    InteractionRequestEventPayload,
    INTERACTION_REQUEST_CLASS
);
interaction_event!(
    InteractionResolvedEvent,
    InteractionResolvedEventPayload,
    INTERACTION_RESOLVED_CLASS
);

pub fn register_interaction_event_classes() -> Result<(), String> {
    register_event2_class(&INTERACTION_REQUEST_CLASS)?;
    register_event2_class(&INTERACTION_RESOLVED_CLASS)
}
