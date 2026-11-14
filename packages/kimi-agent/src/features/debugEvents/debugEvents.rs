use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEBUG_EVENTS_SERVICE_ID: &str = "debugEventsService";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DebugEventEntryKind {
    Disposer,
    Effect,
    Ledger,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugEventSubscription {
    pub scope_path: String,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u64>,
    pub label: String,
    pub kind: DebugEventEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugEventBusSnapshot {
    pub scope_path: String,
    pub all: u64,
    pub per_type: BTreeMap<String, u64>,
    pub per_agent: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugEventSubscriptions {
    pub subscriptions: Vec<DebugEventSubscription>,
    pub buses: Vec<DebugEventBusSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global_listeners: Option<u64>,
}

pub trait IDebugEventsService {
    fn subscriptions(&self) -> DebugEventSubscriptions;
}
