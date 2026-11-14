use std::ops::Deref;

use async_trait::async_trait;

use crate::_base::di::cascade_engine::{CascadeAction, UnitState};
use crate::app::event::event2::{Event2, Event2Class};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct DebugCascadeEntry {
    pub scope_path: String,
    pub seq: u64,
    pub reason: String,
    pub changes: Vec<DebugCascadeChange>,
    pub affected: Vec<String>,
    pub torn_down: Vec<String>,
    pub rebuilt: Vec<String>,
    pub failed: Vec<String>,
    pub abort_waited: bool,
    pub abort_timed_out: bool,
    pub duration_ms: u64,
}

#[derive(Clone)]
pub struct DebugCascadeChange {
    pub token: String,
    pub action: CascadeAction,
}

#[derive(Clone)]
pub struct DebugPendingUnit {
    pub token: String,
    pub missing: Vec<String>,
}

#[derive(Clone)]
pub struct DebugFailedUnit {
    pub token: String,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct DebugPendingGroup {
    pub scope_path: String,
    pub waiting: Vec<DebugPendingUnit>,
    pub failed: Vec<DebugFailedUnit>,
}

#[derive(Clone)]
pub struct DiUnitChangedPayload {
    pub scope: String,
    pub token: String,
    pub state: UnitState,
    pub error: Option<String>,
}

pub const DI_UNIT_CHANGED_EVENT: &str = "event.di.unit_changed";

#[derive(Clone)]
pub struct DiUnitChanged {
    event: Event2,
    pub payload: DiUnitChangedPayload,
}

impl DiUnitChanged {
    pub fn new(payload: DiUnitChangedPayload) -> Self {
        let class = Event2Class {
            type_: DI_UNIT_CHANGED_EVENT.to_string(),
            durable: true,
            observable: true,
            agent_domain: false,
            schema: None,
        };
        Self {
            event: Event2::new(&class, serde_json::to_value(&payload).unwrap(), None),
            payload,
        }
    }
}

impl Deref for DiUnitChanged {
    type Target = Event2;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

#[async_trait]
pub trait IDebugCascadeService: Send + Sync {
    fn history(&self) -> Vec<DebugCascadeEntry>;
    fn pending(&self) -> Vec<DebugPendingGroup>;
    async fn unprovide(&self, scope_path: &str, token: &str) -> Result<(), WireAsyncError>;
    async fn update(&self, scope_path: &str, token: &str, config: Option<serde_json::Value>) -> Result<(), WireAsyncError>;
    async fn dispose(&self, scope_path: Option<&str>, token: Option<&str>) -> Result<(), WireAsyncError>;
}
