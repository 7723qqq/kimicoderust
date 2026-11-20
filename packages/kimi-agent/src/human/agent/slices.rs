use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::human::agent::events::{
    INPUT_CANCELLED, INPUT_DRAINED, INPUT_NOTIFIED, INPUT_REMINDED, INPUT_STEERED,
    INPUT_SUBMITTED, MESSAGE_APPENDED, NOTIFICATIONS_DRAINED, QUEUE_DRAINED, TURN_ENDED,
    TURN_STARTED,
};
use crate::human::agent::turn::{
    EntryMeta, HistoryMessage, UserEntry, UserMeta, create_system_entry, create_user_entry,
};
use crate::human::eventStore::eventStore::{CombinedState, EventStoreImpl, SliceMap};
use crate::human::eventStore::events::InternalEvent;
use crate::human::eventStore::slice::{FoldContext, Reducer, Slice};
use crate::human::store::types::BranchRef;

fn array_draft(draft: &mut Value) -> &mut Vec<Value> {
    draft.as_array_mut().unwrap()
}

fn event_value(event: &serde_json::Map<String, Value>, key: &str) -> Value {
    event.get(key).cloned().unwrap_or(Value::Null)
}

fn user_meta_with_source(meta: Option<UserMeta>, source: &str) -> UserMeta {
    let mut meta = meta.unwrap_or_default();
    if meta.source.is_none() {
        meta.source = Some(source.to_string());
    }
    meta
}

pub static HISTORY_SLICE: LazyLock<Slice<Value>> = LazyLock::new(|| Slice {
    name: "history".to_string(),
    initial_state: Arc::new(|| Value::Array(Vec::new())),
    reducers: HashMap::<String, Reducer<Value>>::from_iter([(
        MESSAGE_APPENDED.type_.clone(),
        Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
            array_draft(draft).push(event_value(event, "message"));
            None
        }) as Reducer<Value>,
    )]),
});

pub static QUEUE_SLICE: LazyLock<Slice<Value>> = LazyLock::new(|| Slice {
    name: "queue".to_string(),
    initial_state: Arc::new(|| Value::Array(Vec::new())),
    reducers: HashMap::<String, Reducer<Value>>::from_iter([
        (
            INPUT_SUBMITTED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
                let entry = if let Some(entry) = event.get("entry") {
                    let entry: UserEntry = serde_json::from_value(entry.clone()).unwrap();
                    create_user_entry(
                        entry.message,
                        user_meta_with_source(entry.meta, "input"),
                    )
                } else {
                    create_user_entry(
                        serde_json::from_value(event_value(event, "message")).unwrap(),
                        UserMeta {
                            source: Some("input".to_string()),
                            prompt_id: event.get("id").and_then(Value::as_str).map(str::to_string),
                            ..Default::default()
                        },
                    )
                };
                array_draft(draft).push(serde_json::to_value(entry).unwrap());
                None
            }) as Reducer<Value>,
        ),
        (
            INPUT_CANCELLED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
                let id = event.get("id").and_then(Value::as_str);
                let kept = array_draft(draft)
                    .iter()
                    .filter(|entry| {
                        entry
                            .get("meta")
                            .and_then(|meta| meta.get("promptId"))
                            .and_then(Value::as_str)
                            != id
                    })
                    .cloned()
                    .collect();
                Some(Value::Array(kept))
            }) as Reducer<Value>,
        ),
        (
            INPUT_STEERED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
                let id = event.get("id").and_then(Value::as_str);
                let kept = array_draft(draft)
                    .iter()
                    .filter(|entry| {
                        entry
                            .get("meta")
                            .and_then(|meta| meta.get("promptId"))
                            .and_then(Value::as_str)
                            != id
                    })
                    .cloned()
                    .collect();
                Some(Value::Array(kept))
            }) as Reducer<Value>,
        ),
        (
            QUEUE_DRAINED.type_.clone(),
            Arc::new(|draft, _, _| {
                let entries = array_draft(draft);
                if !entries.is_empty() {
                    entries.remove(0);
                }
                None
            }),
        ),
    ]),
});

pub static NOTIFICATIONS_SLICE: LazyLock<Slice<Value>> = LazyLock::new(|| Slice {
    name: "notifications".to_string(),
    initial_state: Arc::new(|| Value::Array(Vec::new())),
    reducers: HashMap::<String, Reducer<Value>>::from_iter([
        (
            INPUT_NOTIFIED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
                let entry = if let Some(entry) = event.get("entry") {
                    let entry: UserEntry = serde_json::from_value(entry.clone()).unwrap();
                    create_user_entry(
                        entry.message,
                        user_meta_with_source(entry.meta, "notify"),
                    )
                } else {
                    create_user_entry(
                        serde_json::from_value(event_value(event, "message")).unwrap(),
                        UserMeta {
                            source: Some(
                                event
                                    .get("source")
                                    .and_then(Value::as_str)
                                    .unwrap_or("notify")
                                    .to_string(),
                            ),
                            ..Default::default()
                        },
                    )
                };
                array_draft(draft).push(serde_json::to_value(entry).unwrap());
                None
            }) as Reducer<Value>,
        ),
        (
            INPUT_STEERED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
                let entry = create_user_entry(
                    serde_json::from_value(event_value(event, "message")).unwrap(),
                    UserMeta {
                        source: Some("input".to_string()),
                        ..Default::default()
                    },
                );
                array_draft(draft).push(serde_json::to_value(entry).unwrap());
                None
            }) as Reducer<Value>,
        ),
        (
            INPUT_DRAINED.type_.clone(),
            Arc::new(|_, _, _| Some(Value::Array(Vec::new()))),
        ),
        (
            NOTIFICATIONS_DRAINED.type_.clone(),
            Arc::new(|_, _, _| Some(Value::Array(Vec::new()))),
        ),
    ]),
});

fn history_meta_key(entry: &HistoryMessage) -> Option<&str> {
    match entry {
        HistoryMessage::System(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::User(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::Assistant(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
        HistoryMessage::Tool(entry) => entry.meta.as_ref().and_then(|meta| meta.key.as_deref()),
    }
}

pub static REMINDERS_SLICE: LazyLock<Slice<Value>> = LazyLock::new(|| Slice {
    name: "reminders".to_string(),
    initial_state: Arc::new(|| Value::Array(Vec::new())),
    reducers: HashMap::<String, Reducer<Value>>::from_iter([
        (
            INPUT_REMINDED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, _: &mut FoldContext| {
                let key = event.get("key").and_then(Value::as_str).unwrap_or_default();
                let mut kept = serde_json::from_value::<Vec<HistoryMessage>>(draft.clone())
                    .unwrap_or_default();
                kept.retain(|entry| history_meta_key(entry) != Some(key));
                let payload = event_value(event, "message");
                if payload.get("message").is_some() {
                    let entry: HistoryMessage = serde_json::from_value(payload).unwrap();
                    let entry = match entry {
                        HistoryMessage::System(entry) => {
                            let mut meta = entry.meta.unwrap_or_default();
                            if meta.source.is_none() {
                                meta.source = Some("reminder".to_string());
                            }
                            if meta.key.is_none() {
                                meta.key = Some(key.to_string());
                            }
                            HistoryMessage::System(create_system_entry(entry.message, meta))
                        }
                        HistoryMessage::User(entry) => {
                            let mut meta = entry.meta.unwrap_or_default();
                            if meta.source.is_none() {
                                meta.source = Some("reminder".to_string());
                            }
                            if meta.key.is_none() {
                                meta.key = Some(key.to_string());
                            }
                            HistoryMessage::User(create_user_entry(entry.message, meta))
                        }
                        other => other,
                    };
                    kept.push(entry);
                } else {
                    let role = payload.get("role").and_then(Value::as_str);
                    if role == Some("system") {
                        kept.push(HistoryMessage::System(create_system_entry(
                            serde_json::from_value(payload).unwrap(),
                            EntryMeta {
                                source: Some("reminder".to_string()),
                                key: Some(key.to_string()),
                            },
                        )));
                    } else {
                        kept.push(HistoryMessage::User(create_user_entry(
                            serde_json::from_value(payload).unwrap(),
                            UserMeta {
                                source: Some("reminder".to_string()),
                                key: Some(key.to_string()),
                                ..Default::default()
                            },
                        )));
                    }
                }
                Some(serde_json::to_value(kept).unwrap())
            }) as Reducer<Value>,
        ),
        (
            INPUT_DRAINED.type_.clone(),
            Arc::new(|_, _, _| Some(Value::Array(Vec::new()))),
        ),
    ]),
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnIndexEntry {
    pub turn_id: i64,
    pub start: BranchRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<BranchRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnIndexState {
    pub turns: Vec<TurnIndexEntry>,
    pub next_turn_id: i64,
}

pub static TURN_INDEX_SLICE: LazyLock<Slice<Value>> = LazyLock::new(|| Slice {
    name: "turnIndex".to_string(),
    initial_state: Arc::new(|| serde_json::to_value(TurnIndexState::default()).unwrap()),
    reducers: HashMap::<String, Reducer<Value>>::from_iter([
        (
            TURN_STARTED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, ctx: &mut FoldContext| {
                let turn_id = event.get("turnId").and_then(Value::as_i64).unwrap_or(0);
                let mut state: TurnIndexState = serde_json::from_value(draft.clone()).unwrap();
                state.turns.push(TurnIndexEntry {
                    turn_id,
                    start: ctx.ref_.clone(),
                    end: None,
                });
                if turn_id >= state.next_turn_id {
                    state.next_turn_id = turn_id + 1;
                }
                Some(serde_json::to_value(state).unwrap())
            }) as Reducer<Value>,
        ),
        (
            TURN_ENDED.type_.clone(),
            Arc::new(|draft: &mut Value, event: &InternalEvent, ctx: &mut FoldContext| {
                let turn_id = event.get("turnId").and_then(Value::as_i64).unwrap_or(0);
                let mut state: TurnIndexState = serde_json::from_value(draft.clone()).unwrap();
                if let Some(entry) = state.turns.iter_mut().rev().find(|turn| turn.turn_id == turn_id) {
                    entry.end = Some(ctx.ref_.clone());
                }
                if turn_id >= state.next_turn_id {
                    state.next_turn_id = turn_id + 1;
                }
                Some(serde_json::to_value(state).unwrap())
            }) as Reducer<Value>,
        ),
    ]),
});

pub fn agent_slices() -> SliceMap {
    HashMap::from_iter([
        ("history".to_string(), HISTORY_SLICE.clone()),
        ("queue".to_string(), QUEUE_SLICE.clone()),
        ("notifications".to_string(), NOTIFICATIONS_SLICE.clone()),
        ("reminders".to_string(), REMINDERS_SLICE.clone()),
        ("turnIndex".to_string(), TURN_INDEX_SLICE.clone()),
    ])
}

pub type AgentEventStore = EventStoreImpl;
pub type AgentStoreState = CombinedState;
