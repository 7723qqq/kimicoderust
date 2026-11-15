pub use crate::session::sqlite_store::*;

use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};

use crate::app::event::event2::{Event2, Event2Class};

pub const CONTEXT_UNDONE_TYPE: &str = "context.undone";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUndonePayload {
    pub agent_id: String,
    pub turns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_turn_id: Option<u64>,
}

pub static CONTEXT_UNDONE_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: CONTEXT_UNDONE_TYPE.to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct ContextUndone {
    event: Event2,
    pub payload: ContextUndonePayload,
}

impl ContextUndone {
    pub const TYPE: &'static str = CONTEXT_UNDONE_TYPE;
    pub const OBSERVABLE: bool = true;

    pub fn new(payload: ContextUndonePayload) -> Self {
        let event = Event2::new(
            &CONTEXT_UNDONE_CLASS,
            serde_json::to_value(&payload).expect("context.undone is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        CONTEXT_UNDONE_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }

    pub fn from_event(event: Event2) -> Option<Self> {
        let payload = serde_json::from_value(event.payload.clone()).ok()?;
        Some(Self { event, payload })
    }
}

impl Deref for ContextUndone {
    type Target = ContextUndonePayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<ContextUndone> for Event2 {
    fn from(event: ContextUndone) -> Self {
        event.event
    }
}
