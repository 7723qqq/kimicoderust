use std::collections::HashMap;
use std::ops::Deref;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::app::event::event2::{Event2, Event2Class};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaUpdatedPayload {
    pub agent_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub patch: Value,
}

pub const SESSION_META_UPDATED_TYPE: &str = "session.meta.updated";

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMetaUpdated {
    event: Event2,
    pub payload: SessionMetaUpdatedPayload,
}

impl SessionMetaUpdated {
    pub const TYPE: &'static str = SESSION_META_UPDATED_TYPE;

    pub fn new(payload: SessionMetaUpdatedPayload) -> Self {
        let class = Event2Class {
            type_: SESSION_META_UPDATED_TYPE.to_string(),
            durable: false,
            observable: false,
            agent_domain: false,
            schema: None,
        };
        let event = Event2::new(&class, json!({ "payload": payload.clone() }), None);
        Self { event, payload }
    }
}

impl Deref for SessionMetaUpdated {
    type Target = Event2;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl From<SessionMetaUpdated> for Event2 {
    fn from(event: SessionMetaUpdated) -> Self {
        event.event
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetaUpdatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<HashMap<String, Value>>,
}
