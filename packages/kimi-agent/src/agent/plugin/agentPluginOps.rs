use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::event::event2::{Event2, Event2Class};
use crate::state::state::{ReplayableOptions, ReplayableStateKey};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginSessionStartSnapshotState {
    pub initialized: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSessionStartEventPayload {
    pub agent_id: String,
    pub content: Option<String>,
}

pub static PLUGIN_SESSION_START_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "plugin.session_start".to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "content"],
            "properties": {
                "agentId": {"type": "string"},
                "content": {}
            }
        })),
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct PluginSessionStartEvent {
    event: Event2,
    pub payload: PluginSessionStartEventPayload,
}

impl PluginSessionStartEvent {
    pub fn new(payload: PluginSessionStartEventPayload) -> Self {
        let event = Event2::new(
            &PLUGIN_SESSION_START_CLASS,
            serde_json::to_value(&payload).expect("plugin event is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        PLUGIN_SESSION_START_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for PluginSessionStartEvent {
    type Target = PluginSessionStartEventPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<PluginSessionStartEvent> for Event2 {
    fn from(event: PluginSessionStartEvent) -> Self {
        event.event
    }
}

pub static PLUGIN_SESSION_START_SNAPSHOT_KEY: LazyLock<Arc<ReplayableStateKey>> =
    LazyLock::new(|| {
        let mut key = ReplayableStateKey::new(
            "pluginSessionStartSnapshot".to_string(),
            Arc::new(|| {
                serde_json::to_value(PluginSessionStartSnapshotState::default()).unwrap()
            }),
            ReplayableOptions {
                schema: json!({}),
                durable: None,
                blobs: None,
                mutable_replay: None,
            },
        );
        key.on(
            &PLUGIN_SESSION_START_CLASS,
            Arc::new(|_state, event, _context| {
                let payload = serde_json::from_value::<PluginSessionStartEventPayload>(
                    event.payload.clone(),
                )
                .ok()?;
                serde_json::to_value(PluginSessionStartSnapshotState {
                    initialized: true,
                    content: payload.content,
                })
                .ok()
            }),
        )
        .expect("plugin session snapshot fold registration is valid");
        Arc::new(key)
    });

pub fn plugin_session_start_snapshot_key() -> Arc<ReplayableStateKey> {
    PLUGIN_SESSION_START_SNAPSHOT_KEY.clone()
}
