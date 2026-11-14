use std::ops::Deref;

use serde::{Deserialize, Serialize};

use crate::app::event::event2::{Event2, Event2Class};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForkedPayload {
    pub agent_id: String,
}

pub const FORKED_TYPE: &str = "forked";

#[derive(Debug, Clone, PartialEq)]
pub struct Forked {
    event: Event2,
    pub payload: ForkedPayload,
}

impl Forked {
    pub const TYPE: &'static str = FORKED_TYPE;
    pub const DURABLE: bool = true;

    pub fn new(payload: ForkedPayload) -> Self {
        let class = Event2Class {
            type_: FORKED_TYPE.to_string(),
            durable: true,
            observable: true,
            agent_domain: false,
            schema: None,
        };
        let event = Event2::new(
            &class,
            serde_json::to_value(&payload).expect("Forked payload is serializable"),
            None,
        );
        Self { event, payload }
    }
}

impl Deref for Forked {
    type Target = Event2;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}
