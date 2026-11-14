use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::IDisposable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPendingInteraction {
    None,
    Approval,
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnOutcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivityState {
    pub busy: bool,
    pub main_turn_active: bool,
    pub pending_interaction: SessionPendingInteraction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<SessionTurnOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityCause {
    TurnStarted,
    TurnEnded,
    Background,
    Interaction,
    AgentLifecycle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionActivityChangedEvent {
    pub state: SessionActivityState,
    pub cause: SessionActivityCause,
}

pub trait ISessionActivityView: Send + Sync {
    fn state(&self) -> SessionActivityState;
    fn last_active_at(&self) -> i64;
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(SessionActivityChangedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable>;
}
