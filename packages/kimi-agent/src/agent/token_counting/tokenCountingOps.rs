pub use crate::session::token_counting::*;

use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCountingMeasuredPayload {
    pub agent_id: String,
    pub length: usize,
    pub tokens: u64,
}

pub type TokenCountingTruncatedPayload = TokenCountingMeasuredPayload;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCountingRebasedPayload {
    pub agent_id: String,
    pub length: usize,
    pub tokens: u64,
    pub measured: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCountingTurnRecordedPayload {
    pub agent_id: String,
    pub length: usize,
    pub tokens: u64,
    pub turn_id: u64,
}

fn size_schema() -> Value {
    json!({
        "type": "object",
        "required": ["agentId", "length", "tokens"],
        "properties": {
            "agentId": {"type": "string"},
            "length": {"type": "number"},
            "tokens": {"type": "number"},
        }
    })
}

macro_rules! token_counting_event {
    ($name:ident, $class:ident, $type:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: true,
                observable: false,
                agent_domain: true,
                schema: Some($schema),
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: Value,
        }

        impl $name {
            pub const TYPE: &'static str = $type;

            pub fn new(payload: Value) -> Self {
                let event = Event2::new(
                    &$class,
                    payload.clone(),
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
            type Target = Value;

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

token_counting_event!(
    TokenCountingMeasured,
    TOKEN_COUNTING_MEASURED_CLASS,
    "token_counting.measured",
    size_schema()
);
token_counting_event!(
    TokenCountingTruncated,
    TOKEN_COUNTING_TRUNCATED_CLASS,
    "token_counting.truncated",
    size_schema()
);
token_counting_event!(
    TokenCountingRebased,
    TOKEN_COUNTING_REBASED_CLASS,
    "token_counting.rebased",
    {
        let mut schema = size_schema();
        schema["required"].as_array_mut().unwrap().push(json!("measured"));
        schema["properties"]["measured"] = json!({"type": "boolean"});
        schema
    }
);
token_counting_event!(
    TokenCountingTurnRecorded,
    TOKEN_COUNTING_TURN_RECORDED_CLASS,
    "token_counting.turn_recorded",
    {
        let mut schema = size_schema();
        schema["required"].as_array_mut().unwrap().push(json!("turnId"));
        schema["properties"]["turnId"] = json!({"type": "number"});
        schema
    }
);

pub fn anchors_equal(left: &[TokenAnchor], right: &[TokenAnchor]) -> bool {
    left == right
}

pub fn normalize_anchor_length<T>(length: T) -> usize
where
    T: Into<f64>,
{
    let length = length.into();
    if !length.is_finite() {
        return 0;
    }
    length.floor().max(0.0) as usize
}
