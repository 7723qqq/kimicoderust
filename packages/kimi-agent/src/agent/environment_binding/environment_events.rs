use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};

use crate::app::event::event2::{Event2, Event2Class};
use crate::environment::environment::EnvironmentStatus;

pub const ENVIRONMENT_STATUS_CHANGED_EVENT: &str = "environment.status.changed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStatusChangedPayload {
    pub agent_id: String,
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EnvironmentStatus>,
}

pub static ENVIRONMENT_STATUS_CHANGED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: ENVIRONMENT_STATUS_CHANGED_EVENT.to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStatusChanged {
    pub agent_id: String,
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EnvironmentStatus>,
}

impl EnvironmentStatusChanged {
    pub const TYPE: &'static str = ENVIRONMENT_STATUS_CHANGED_EVENT;

    pub fn class() -> Arc<Event2Class> {
        ENVIRONMENT_STATUS_CHANGED_CLASS.clone()
    }

    pub fn to_event2(&self) -> Event2 {
        Event2::new(
            &ENVIRONMENT_STATUS_CHANGED_CLASS,
            serde_json::to_value(self).expect("environment status event is serializable"),
            None,
        )
    }
}

impl From<EnvironmentStatusChanged> for Event2 {
    fn from(event: EnvironmentStatusChanged) -> Self {
        event.to_event2()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentStatusChangedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub environment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<EnvironmentStatus>,
}

impl EnvironmentStatusChangedEvent {
    pub fn new(environment_id: String, status: Option<EnvironmentStatus>) -> Self {
        Self {
            type_: ENVIRONMENT_STATUS_CHANGED_EVENT.to_string(),
            environment_id,
            status,
        }
    }
}
