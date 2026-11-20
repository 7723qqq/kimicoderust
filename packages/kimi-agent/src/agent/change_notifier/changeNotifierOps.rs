use std::collections::BTreeMap;
use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::agent::context_memory::context_events::{
    CONTEXT_APPLY_COMPACTION_CLASS, CONTEXT_CLEAR_CLASS,
};
use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey, define_state};

pub use crate::injection::change_notifier::*;

use super::change_notifier::SubagentModelPoolSnapshot;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeNotifierSnapshotState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_files: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_pool: Option<SubagentModelPoolSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_primary_by_workspace: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_additional_dirs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeNotifierSnapshotEventPayload {
    pub agent_id: String,
    pub agents_md_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_files: Option<BTreeMap<String, String>>,
    pub skills_hash: Option<String>,
    pub subagent_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_pool: Option<SubagentModelPoolSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_primary_by_workspace: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_additional_dirs: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangeNotifierSnapshotEvent {
    event: Event2,
    pub payload: ChangeNotifierSnapshotEventPayload,
}

impl ChangeNotifierSnapshotEvent {
    pub const TYPE: &'static str = "changeNotifier.snapshot";
    pub const DURABLE: bool = true;

    pub fn new(payload: ChangeNotifierSnapshotEventPayload) -> Self {
        let class = Self::class();
        let event = Event2::new(
            &class,
            serde_json::to_value(&payload).expect("change notifier snapshot is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        CHANGE_NOTIFIER_SNAPSHOT_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for ChangeNotifierSnapshotEvent {
    type Target = ChangeNotifierSnapshotEventPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<ChangeNotifierSnapshotEvent> for Event2 {
    fn from(event: ChangeNotifierSnapshotEvent) -> Self {
        event.event
    }
}

static CHANGE_NOTIFIER_SNAPSHOT_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: ChangeNotifierSnapshotEvent::TYPE.to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "agentsMdHash", "skillsHash", "subagentNames"],
            "properties": {
                "agentId": {"type": "string"},
                "agentsMdHash": {"type": ["string", "null"]},
                "agentsMdFiles": {
                    "type": ["object", "null"],
                    "additionalProperties": {"type": "string"},
                },
                "skillsHash": {"type": ["string", "null"]},
                "subagentNames": {"type": ["array", "null"], "items": {"type": "string"}},
                "modelPool": {
                    "type": ["object", "null"],
                    "required": ["models"],
                    "properties": {
                        "defaultModel": {"type": "string"},
                        "models": {
                            "type": "object",
                            "additionalProperties": {"type": "string"},
                        },
                    },
                },
                "workspacePrimaryByWorkspace": {
                    "type": ["object", "null"],
                    "additionalProperties": {"type": "string"},
                },
                "workspaceAdditionalDirs": {
                    "type": ["array", "null"],
                    "items": {"type": "string"},
                },
            },
        })),
    })
});

pub static CHANGE_NOTIFIER_SNAPSHOT_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        let snapshot = ChangeNotifierSnapshotEvent::class();
        let mut key = define_state("changeNotifier.snapshot", Arc::new(|| json!({})))
            .replayable(ReplayableOptions {
                schema: json!({"type": "object"}),
                durable: None,
                blobs: None,
                mutable_replay: None,
            });
        Arc::get_mut(&mut key).expect("fresh state key").undoable(None)
            .expect("change notifier snapshot can be undoable");
        for class in [
            CONTEXT_CLEAR_CLASS.clone(),
            CONTEXT_APPLY_COMPACTION_CLASS.clone(),
        ] {
            Arc::get_mut(&mut key).expect("fresh state key").on(&class, Arc::new(|state, _event, _ctx| {
                let mut next = Map::new();
                for field in ["workspacePrimaryByWorkspace", "workspaceAdditionalDirs"] {
                    if let Some(value) = state.get(field) {
                        next.insert(field.to_string(), value.clone());
                    }
                }
                Some(Value::Object(next))
            }))
            .expect("change notifier context reset fold can be registered");
        }
        Arc::get_mut(&mut key).expect("fresh state key").on(&snapshot, Arc::new(|_state, event, _ctx| {
            let mut next = Map::new();
            for field in [
                "agentsMdHash",
                "agentsMdFiles",
                "skillsHash",
                "subagentNames",
                "modelPool",
                "workspacePrimaryByWorkspace",
                "workspaceAdditionalDirs",
            ] {
                if let Some(value) = event.payload.get(field)
                    && !value.is_null()
                {
                    next.insert(field.to_string(), value.clone());
                }
            }
            Some(Value::Object(next))
        }))
        .expect("change notifier snapshot fold can be registered");
        key
    });

pub fn change_notifier_snapshot_key() -> Arc<ReplayableStateKey> {
    CHANGE_NOTIFIER_SNAPSHOT_KEY.clone()
}
