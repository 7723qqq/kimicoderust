use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::event::event2::{Event2, Event2Class};
use crate::human::llm::usage::TokenUsage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsageRecordScope {
    Session,
    Turn,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageModelState {
    pub by_model: std::collections::HashMap<String, TokenUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecordPayload {
    pub agent_id: String,
    pub model: String,
    pub usage: TokenUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_scope: Option<UsageRecordScope>,
}

pub static USAGE_RECORD_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: "usage.record".to_string(),
        durable: true,
        observable: false,
        agent_domain: true,
        schema: Some(json!({
            "type": "object",
            "required": ["agentId", "model", "usage"],
            "properties": {
                "agentId": {"type": "string"},
                "model": {"type": "string"},
                "usage": {},
                "usageScope": {}
            }
        })),
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct UsageRecord {
    event: Event2,
    pub payload: UsageRecordPayload,
}

impl UsageRecord {
    pub fn new(payload: UsageRecordPayload) -> Self {
        let event = Event2::new(
            &USAGE_RECORD_CLASS,
            serde_json::to_value(&payload).expect("usage event is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        USAGE_RECORD_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for UsageRecord {
    type Target = UsageRecordPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<UsageRecord> for Event2 {
    fn from(event: UsageRecord) -> Self {
        event.event
    }
}

pub fn copy_usage(usage: &TokenUsage) -> TokenUsage {
    usage.clone()
}
