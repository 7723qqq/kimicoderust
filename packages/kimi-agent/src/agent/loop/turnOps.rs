use std::collections::BTreeSet;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::context_memory::context_events::{
    CONTEXT_APPEND_LOOP_EVENT_CLASS, CONTEXT_APPLY_COMPACTION_CLASS, CONTEXT_CLEAR_CLASS,
    CONTEXT_UNDO_CLASS,
};
pub use crate::agent::r#loop::turn_events::{TurnEndReason, TurnInterruptReason};
use crate::app::event::event2::{Event2, Event2Class, register_event2_class};
use crate::human::llm::message::ContentPart;
use crate::state::state::{ReplayableOptions, ReplayableStateKey};
use kimi_core::primitives::errors::KimiErrorPayload;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPromptPayload {
    pub agent_id: String,
    pub input: Vec<ContentPart>,
    pub origin: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerPayload {
    pub agent_id: String,
    pub input: Vec<ContentPart>,
    pub origin: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnCancelTarget {
    Active,
    Queued,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCancelReason {
    UserCancelled,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCancelPayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TurnCancelTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<TurnCancelReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndedPayload {
    pub agent_id: String,
    pub turn_id: u64,
    pub reason: TurnEndReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<KimiErrorPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_reason: Option<TurnInterruptReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

macro_rules! turn_event {
    ($name:ident, $payload:ident, $class:ident, $type:literal, $observable:literal, $schema:expr) => {
        pub static $class: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
            Arc::new(Event2Class {
                type_: $type.to_string(),
                durable: true,
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
            pub const DURABLE: bool = true;
            pub const OBSERVABLE: bool = $observable;

            pub fn new(payload: $payload) -> Self {
                let event = Event2::new(
                    &$class,
                    serde_json::to_value(&payload).expect("turn operation is serializable"),
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

turn_event!(
    TurnPrompt,
    TurnPromptPayload,
    TURN_PROMPT_CLASS,
    "turn.prompt",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "input", "origin"],
        "properties": {
            "agentId": {"type": "string"},
            "input": {},
            "origin": {},
            "promptId": {"type": "string"},
            "turnId": {"type": "number"}
        }
    }))
);
turn_event!(
    TurnSteer,
    TurnSteerPayload,
    TURN_STEER_CLASS,
    "turn.steer",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "input", "origin"],
        "properties": {
            "agentId": {"type": "string"},
            "input": {},
            "origin": {},
            "messageId": {"type": "string"},
            "promptIds": {"type": "array", "items": {"type": "string"}},
            "turnId": {"type": "number"}
        }
    }))
);
turn_event!(
    TurnCancel,
    TurnCancelPayload,
    TURN_CANCEL_CLASS,
    "turn.cancel",
    false,
    Some(json!({
        "type": "object",
        "required": ["agentId"],
        "properties": {
            "agentId": {"type": "string"},
            "turnId": {"type": "number"},
            "target": {"enum": ["active", "queued"]},
            "reason": {"enum": ["user_cancelled", "aborted"]}
        }
    }))
);
turn_event!(
    TurnEnded,
    TurnEndedPayload,
    TURN_ENDED_CLASS,
    "turn.ended",
    true,
    Some(json!({
        "type": "object",
        "required": ["agentId", "turnId", "reason"],
        "properties": {
            "agentId": {"type": "string"},
            "turnId": {"type": "number"},
            "reason": {"enum": ["completed", "cancelled", "failed", "blocked"]},
            "error": {},
            "durationMs": {"type": "number"},
            "interruptReason": {"enum": ["user_cancelled", "aborted", "max_steps", "error", "filtered", "blocked"]},
            "stopReason": {"type": "string"},
            "traceId": {"type": "string"}
        }
    }))
);

pub fn register_turn_operation_classes() -> Result<(), String> {
    for class in [
        &TURN_PROMPT_CLASS,
        &TURN_STEER_CLASS,
        &TURN_CANCEL_CLASS,
        &TURN_ENDED_CLASS,
    ] {
        register_event2_class(class)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnModelLastEnded {
    pub turn_id: u64,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnModelState {
    pub next_turn_id: u64,
    pub cancelled_turn_ids: Vec<u64>,
    pub anchor_turn_ids: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ended: Option<TurnModelLastEnded>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEndedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,
    pub turn_id: u64,
    pub reason: TurnEndReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<KimiErrorPayload>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_reason: Option<TurnInterruptReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
}

fn advance_turn_clock(
    state: &TurnModelState,
    mut next_turn_id: u64,
    cancelled_turn_ids: Option<Vec<u64>>,
) -> TurnModelState {
    let mut pending = cancelled_turn_ids
        .unwrap_or_else(|| state.cancelled_turn_ids.clone())
        .into_iter()
        .filter(|turn_id| *turn_id >= next_turn_id)
        .collect::<BTreeSet<_>>();
    while pending.remove(&next_turn_id) {
        next_turn_id += 1;
    }
    TurnModelState {
        next_turn_id,
        cancelled_turn_ids: pending.into_iter().collect(),
        ..state.clone()
    }
}

fn is_undo_anchor_origin(origin: Option<&Value>) -> bool {
    if origin.is_some_and(|origin| origin.get("inTurn").and_then(Value::as_bool) == Some(true)) {
        return false;
    }
    let Some(origin) = origin else {
        return true;
    };
    let kind = origin.get("kind").and_then(Value::as_str);
    if kind == Some("user") || kind.is_none() {
        return true;
    }
    matches!(kind, Some("skill_activation" | "plugin_command"))
        && origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
}

fn state_value(state: &TurnModelState) -> Option<Value> {
    serde_json::to_value(state).ok()
}

fn parse_turn_id(value: &str) -> Option<u64> {
    let value = value.trim_start();
    let negative = value.starts_with('-');
    let value = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    let digits = value
        .bytes()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let parsed = value[..digits].parse().ok();
    if negative && parsed != Some(0) {
        return None;
    }
    parsed
}

pub fn fold_turn_state(current: Value, event: &Event2) -> Option<Value> {
    let mut state = serde_json::from_value::<TurnModelState>(current).ok()?;
    match event.type_.as_str() {
        "context.append_loop_event" => {
            let recorded = event.payload.get("event")?;
            if recorded.get("type").and_then(Value::as_str) == Some("tool.result") {
                return None;
            }
            let turn_id = recorded
                .get("turnId")
                .and_then(Value::as_str)
                .and_then(parse_turn_id)?;
            let original = state.clone();
            if turn_id >= state.next_turn_id {
                state = advance_turn_clock(&state, turn_id + 1, None);
            }
            if state
                .last_ended
                .as_ref()
                .is_some_and(|last| turn_id > last.turn_id)
            {
                state.last_ended = None;
            }
            (state != original).then(|| state_value(&state)).flatten()
        }
        "turn.prompt" => {
            let assigned = event
                .payload
                .get("turnId")
                .and_then(Value::as_u64)
                .unwrap_or(state.next_turn_id);
            let next = advance_turn_clock(&state, assigned + 1, None);
            if is_undo_anchor_origin(event.payload.get("origin")) {
                let mut anchors = state.anchor_turn_ids.clone();
                anchors.push(assigned);
                state.anchor_turn_ids = anchors;
            }
            state.next_turn_id = next.next_turn_id;
            state.cancelled_turn_ids = next.cancelled_turn_ids;
            state_value(&state)
        }
        "turn.steer" => None,
        "context.undo" => {
            let count = event.payload.get("count").and_then(Value::as_u64)? as usize;
            let first_removed = state
                .anchor_turn_ids
                .len()
                .checked_sub(count)
                .and_then(|index| state.anchor_turn_ids.get(index))
                .copied();
            let keep = state.anchor_turn_ids.len().saturating_sub(count);
            state.anchor_turn_ids.truncate(keep);
            if state.last_ended.as_ref().is_some_and(|last| {
                first_removed.is_none_or(|first| last.turn_id >= first)
            }) {
                state.last_ended = None;
            }
            state_value(&state)
        }
        "context.apply_compaction" | "context.clear" => {
            state.anchor_turn_ids.clear();
            state_value(&state)
        }
        "turn.cancel" => {
            if event.payload.get("target").is_none() {
                return None;
            }
            let turn_id = event.payload.get("turnId").and_then(Value::as_u64)?;
            if turn_id < state.next_turn_id {
                return None;
            }
            let mut cancelled = state.cancelled_turn_ids.clone();
            cancelled.push(turn_id);
            state = advance_turn_clock(&state, state.next_turn_id, Some(cancelled));
            state_value(&state)
        }
        "turn.ended" => {
            let turn_id = event.payload.get("turnId").and_then(Value::as_u64)?;
            let reason = event.payload.get("reason").and_then(Value::as_str)?.to_string();
            state.last_ended = Some(TurnModelLastEnded {
                turn_id,
                reason,
                duration_ms: event.payload.get("durationMs").and_then(Value::as_f64),
            });
            state_value(&state)
        }
        _ => None,
    }
}

pub static TURN_REPLAYABLE_KEY: LazyLock<Arc<ReplayableStateKey>> = LazyLock::new(|| {
    let mut key = ReplayableStateKey::new(
        "turn".to_string(),
        Arc::new(|| serde_json::to_value(TurnModelState::default).unwrap_or(Value::Null)),
        ReplayableOptions {
            schema: json!({}),
            durable: None,
            blobs: None,
            mutable_replay: None,
        },
    );
    for class in [
        &CONTEXT_APPEND_LOOP_EVENT_CLASS,
        &CONTEXT_UNDO_CLASS,
        &CONTEXT_APPLY_COMPACTION_CLASS,
        &CONTEXT_CLEAR_CLASS,
        &TURN_PROMPT_CLASS,
        &TURN_STEER_CLASS,
        &TURN_CANCEL_CLASS,
        &TURN_ENDED_CLASS,
    ] {
        key.on(class, Arc::new(|state, event, _| fold_turn_state(state, event)))
            .unwrap_or_else(|error| panic!("failed to register turn state fold: {error}"));
    }
    Arc::new(key)
});

pub fn turn_replayable_key() -> Arc<ReplayableStateKey> {
    TURN_REPLAYABLE_KEY.clone()
}
