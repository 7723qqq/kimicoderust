use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::context_memory::context_events::{
    CONTEXT_APPLY_COMPACTION_CLASS, CONTEXT_CLEAR_CLASS,
};
use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey, define_state};

pub use crate::compaction::*;

use super::types::{CompactionBeginData, CompactionResult, CompactionSource};

macro_rules! compaction_event {
    ($name:ident, $payload:ident, $class:ident, $type:literal, $durable:literal, $observable:literal, $schema:expr) => {
        static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: $durable,
                observable: $observable,
                agent_domain: true,
                schema: $schema,
            })
        });

        #[derive(Debug, Clone, PartialEq)]
        pub struct $name {
            event: Event2,
            pub payload: $payload,
        }

        impl $name {
            pub const TYPE: &'static str = $type;
            pub const DURABLE: bool = $durable;
            pub const OBSERVABLE: bool = $observable;

            pub fn new(payload: $payload) -> Self {
                let class = Self::class();
                let event = Event2::new(
                    &class,
                    serde_json::to_value(&payload).expect("compaction event is serializable"),
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

            pub fn from_event(event: Event2) -> Option<Self> {
                let payload = serde_json::from_value(event.payload.clone()).ok()?;
                Some(Self { event, payload })
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionPhase {
    Idle,
    Running,
    Cancelled,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionState {
    pub phase: CompactionPhase,
}

impl Default for CompactionState {
    fn default() -> Self {
        Self {
            phase: CompactionPhase::Idle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullCompactionBeginPayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
    pub source: CompactionSource,
}

impl FullCompactionBeginPayload {
    pub fn new(agent_id: String, data: CompactionBeginData) -> Self {
        Self {
            agent_id,
            instruction: data.instruction,
            source: data.source,
        }
    }
}

compaction_event!(
    FullCompactionBegin,
    FullCompactionBeginPayload,
    FULL_COMPACTION_BEGIN_CLASS,
    "full_compaction.begin",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId", "source"],
        "properties": {
            "agentId": {"type": "string"},
            "instruction": {"type": "string"},
            "source": {},
        },
    }))
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentIdPayload {
    pub agent_id: String,
}

compaction_event!(
    FullCompactionCancel,
    AgentIdPayload,
    FULL_COMPACTION_CANCEL_CLASS,
    "full_compaction.cancel",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId"],
        "properties": {"agentId": {"type": "string"}},
    }))
);

compaction_event!(
    FullCompactionComplete,
    AgentIdPayload,
    FULL_COMPACTION_COMPLETE_CLASS,
    "full_compaction.complete",
    true,
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId"],
        "properties": {"agentId": {"type": "string"}},
    }))
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStartedPayload {
    pub agent_id: String,
    pub trigger: CompactionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

compaction_event!(
    CompactionStarted,
    CompactionStartedPayload,
    COMPACTION_STARTED_CLASS,
    "compaction.started",
    false,
    true,
    None
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionBlockedPayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

compaction_event!(
    CompactionBlocked,
    CompactionBlockedPayload,
    COMPACTION_BLOCKED_CLASS,
    "compaction.blocked",
    false,
    true,
    None
);

compaction_event!(
    CompactionCancelled,
    AgentIdPayload,
    COMPACTION_CANCELLED_CLASS,
    "compaction.cancelled",
    false,
    true,
    None
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionCompletedPayload {
    pub agent_id: String,
    pub result: CompactionResult,
}

compaction_event!(
    CompactionCompleted,
    CompactionCompletedPayload,
    COMPACTION_COMPLETED_CLASS,
    "compaction.completed",
    false,
    true,
    None
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStartedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub trigger: CompactionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionBlockedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCancelledEvent {
    #[serde(rename = "type")]
    pub type_: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionCompletedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub result: CompactionResult,
}

pub static FULL_COMPACTION_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let begin = FullCompactionBegin::class();
    let cancel = FullCompactionCancel::class();
    let complete = FullCompactionComplete::class();
    let mut key = define_state(
        "fullCompaction",
        Arc::new(|| json!({"phase": "idle"})),
    )
    .replayable(ReplayableOptions {
        schema: json!({}),
        durable: None,
        blobs: None,
        mutable_replay: None,
    });
    Arc::get_mut(&mut key).expect("fresh state key").on(&begin, Arc::new(|state, event, ctx| {
        let mut next = state;
        if next.get("phase").and_then(Value::as_str) != Some("running") {
            next.as_object_mut()?.insert("phase".to_string(), json!("running"));
        }
        let agent_id = event.payload.get("agentId")?.as_str()?.to_string();
        let source = event.payload.get("source")?.clone();
        let instruction = event
            .payload
            .get("instruction")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut payload = json!({"agentId": agent_id, "trigger": source});
        if let Some(instruction) = instruction {
            payload
                .as_object_mut()?
                .insert("instruction".to_string(), json!(instruction));
        }
        ctx.emit(Event2::new(
            &CompactionStarted::class(),
            payload,
            None,
        ));
        Some(next)
    }))
    .expect("full compaction begin fold can be registered");
    for class in [cancel, complete] {
        Arc::get_mut(&mut key).expect("fresh state key").on(&class, Arc::new(|state, _event, _ctx| {
            if state.get("phase").and_then(Value::as_str) == Some("idle") {
                return None;
            }
            let mut next = state;
            next.as_object_mut()?.insert("phase".to_string(), json!("idle"));
            Some(next)
        }))
        .expect("full compaction finish fold can be registered");
    }
    key
});

pub fn full_compaction_key() -> Arc<ReplayableStateKey> {
    FULL_COMPACTION_KEY.clone()
}

pub static FULL_COMPACTION_WIRE_RANGES_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        let mut key = define_state("fullCompaction.wireRanges", Arc::new(|| json!([])))
            .replayable(ReplayableOptions {
                schema: json!({}),
                durable: None,
                blobs: None,
                mutable_replay: None,
            });
        Arc::get_mut(&mut key).expect("fresh state key").on(&CONTEXT_APPLY_COMPACTION_CLASS, Arc::new(|state, event, _ctx| {
            let range = event.payload.get("wireLines")?;
            let start = range.get("start").and_then(Value::as_u64)?;
            let end = range.get("end").and_then(Value::as_u64)?;
            let mut ranges = state.as_array().cloned().unwrap_or_default();
            ranges.push(json!({"start": start, "end": end}));
            Some(Value::Array(ranges))
        }))
        .expect("compaction wire range fold can be registered");
        Arc::get_mut(&mut key).expect("fresh state key").on(&CONTEXT_CLEAR_CLASS, Arc::new(|state, _event, _ctx| {
            let ranges = state.as_array()?;
            if ranges.is_empty() {
                None
            } else {
                Some(json!([]))
            }
        }))
        .expect("context clear fold can be registered");
        key
    });

pub fn full_compaction_wire_ranges_key() -> Arc<ReplayableStateKey> {
    FULL_COMPACTION_WIRE_RANGES_KEY.clone()
}

