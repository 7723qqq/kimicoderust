use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::event::event2::{Event2, Event2Class};

use super::types::{CapabilityId, CapabilityInstallProgress};

pub const CAPABILITY_CHANGED_TYPE: &str = "event.capability.changed";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityChangedPayload {
    pub capability_id: CapabilityId,
    pub install: CapabilityInstallProgress,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CapabilityChanged {
    pub payload: CapabilityChangedPayload,
    pub time: i64,
}

impl CapabilityChanged {
    pub const TYPE: &'static str = CAPABILITY_CHANGED_TYPE;

    pub fn new(payload: CapabilityChangedPayload) -> Self {
        Self::with_time(payload, None)
    }

    pub fn with_time(payload: CapabilityChangedPayload, time: Option<i64>) -> Self {
        Self {
            payload,
            time: time.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
        }
    }

    pub fn class() -> Arc<Event2Class> {
        Arc::new(Event2Class {
            type_: CAPABILITY_CHANGED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        })
    }

    pub fn to_event2(&self) -> Event2 {
        Event2 {
            type_: CAPABILITY_CHANGED_TYPE.to_string(),
            time: self.time,
            payload: json!({"payload": self.payload}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub capability_id: String,
    pub install: CapabilityInstallProgress,
}

impl Default for CapabilityChangedEvent {
    fn default() -> Self {
        Self {
            type_: CAPABILITY_CHANGED_TYPE.to_string(),
            capability_id: String::new(),
            install: CapabilityInstallProgress::default(),
        }
    }
}
