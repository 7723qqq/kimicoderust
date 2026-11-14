use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};

pub const PLUGIN_CHANGED_TYPE: &str = "event.plugin.changed";

#[derive(Debug, Clone, PartialEq)]
pub struct PluginChanged {
    pub payload: HashMap<String, Value>,
    pub time: i64,
}

impl PluginChanged {
    pub const TYPE: &'static str = PLUGIN_CHANGED_TYPE;

    pub fn new(time: Option<i64>) -> Self {
        Self {
            payload: HashMap::new(),
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: PLUGIN_CHANGED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: Some(json!({
                "type": "object",
                "required": ["payload"],
                "properties": {
                    "payload": {"type": "object", "additionalProperties": false}
                }
            })),
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: PLUGIN_CHANGED_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
}

impl Default for PluginChangedEvent {
    fn default() -> Self {
        Self {
            type_: PLUGIN_CHANGED_TYPE.to_string(),
        }
    }
}
