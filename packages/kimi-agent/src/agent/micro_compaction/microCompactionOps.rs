pub use crate::server::engine::*;

use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::context_memory::context_events::{CONTEXT_APPLY_COMPACTION_CLASS, CONTEXT_CLEAR_CLASS};
use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MicroCompactionAppliedPayload {
    pub cutoff: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MicroCompactionClampedPayload {
    pub max_cutoff: f64,
}

macro_rules! micro_compaction_event {
    ($name:ident, $payload:ty, $class:ident, $type:literal, $field:literal) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: true,
                observable: false,
                agent_domain: false,
                schema: Some(json!({
                    "type": "object",
                    "required": [$field],
                    "properties": {$field: {"type": "number", "minimum": 0}},
                })),
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("micro compaction event is serializable"),
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

micro_compaction_event!(
    MicroCompactionApplied,
    MicroCompactionAppliedPayload,
    MICRO_COMPACTION_APPLIED_CLASS,
    "micro_compaction.apply",
    "cutoff"
);
micro_compaction_event!(
    MicroCompactionClamped,
    MicroCompactionClampedPayload,
    MICRO_COMPACTION_CLAMPED_CLASS,
    "micro_compaction.clamp",
    "maxCutoff"
);

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MicroCompactionReplayState {
    pub cutoff: f64,
}

fn event_number(event: &Event2, field: &str) -> Option<Value> {
    let value = event.payload.get(field)?;
    let number = value.as_f64()?;
    (number.is_finite() && number >= 0.0).then_some(value.clone())
}

pub static MICRO_COMPACTION_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "microCompaction".to_string(),
        Arc::new(|| json!({"cutoff": 0})),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    key.on(
        &MICRO_COMPACTION_APPLIED_CLASS,
        Arc::new(|state, event, _context| {
            let cutoff = event_number(&event, "cutoff")?;
            state
                .as_object_mut()?
                .insert("cutoff".to_string(), cutoff);
            Some(state)
        }),
    )
    .expect("micro compaction apply fold registration is valid");
    key.on(
        &MICRO_COMPACTION_CLAMPED_CLASS,
        Arc::new(|state, event, _context| {
            let max_cutoff = event_number(&event, "maxCutoff")?.as_f64()?;
            let current = state.get("cutoff")?.as_f64()?;
            state
                .as_object_mut()?
                .insert("cutoff".to_string(), json!(current.min(max_cutoff)));
            Some(state)
        }),
    )
    .expect("micro compaction clamp fold registration is valid");
    for class in [&CONTEXT_CLEAR_CLASS, &CONTEXT_APPLY_COMPACTION_CLASS] {
        key.on(class, Arc::new(|state, _event, _context| {
            state
                .as_object_mut()?
                .insert("cutoff".to_string(), json!(0));
            Some(state)
        }))
        .expect("micro compaction reset fold registration is valid");
    }
    Arc::new(key)
});

pub fn micro_compaction_key() -> Arc<ReplayableStateKey> {
    MICRO_COMPACTION_KEY.clone()
}

