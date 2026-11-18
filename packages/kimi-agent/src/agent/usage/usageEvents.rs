use std::ops::Deref;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::permission_policy::types::PermissionMode;
use crate::agent::r#loop::turn_events::TurnEndReason;
use crate::app::event::event2::{Event2, Event2Class};
use crate::features::helloWorld::agent::helloWorld::HelloWorldStage;
use crate::features::tower::tower::TowerExitReason;

use super::usage::UsageStatus;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatusUpdatedPayload {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello_world_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello_world_stage: Option<HelloWorldStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_exit_reason: Option<TowerExitReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionMode>,
}

pub static AGENT_STATUS_UPDATED_CLASS: LazyLock<Arc<Event2Class>> = LazyLock::new(|| {
    Arc::new(Event2Class {
        type_: AgentStatusUpdated::TYPE.to_string(),
        durable: false,
        observable: true,
        agent_domain: true,
        schema: None,
    })
});

#[derive(Debug, Clone, PartialEq)]
pub struct AgentStatusUpdated {
    event: Event2,
    pub payload: AgentStatusUpdatedPayload,
}

impl AgentStatusUpdated {
    pub const TYPE: &'static str = "agent.status.updated";
    pub const DURABLE: bool = false;
    pub const OBSERVABLE: bool = true;

    pub fn new(payload: AgentStatusUpdatedPayload) -> Self {
        let event = Event2::new(
            &AGENT_STATUS_UPDATED_CLASS,
            serde_json::to_value(&payload).expect("agent status event is serializable"),
            None,
        );
        Self { event, payload }
    }

    pub fn class() -> Arc<Event2Class> {
        AGENT_STATUS_UPDATED_CLASS.clone()
    }

    pub fn as_event(&self) -> &Event2 {
        &self.event
    }
}

impl Deref for AgentStatusUpdated {
    type Target = AgentStatusUpdatedPayload;

    fn deref(&self) -> &Self::Target {
        &self.payload
    }
}

impl From<AgentStatusUpdated> for Event2 {
    fn from(event: AgentStatusUpdated) -> Self {
        event.event
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInterruptedReason {
    Aborted,
    MaxSteps,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentPhase {
    Idle,
    #[serde(rename_all = "camelCase")]
    Running {
        turn_id: u64,
        step: u64,
        step_id: String,
        since: u64,
    },
    #[serde(rename_all = "camelCase")]
    ToolCall {
        turn_id: u64,
        step: u64,
        tool_call_id: String,
        name: String,
        since: u64,
    },
    #[serde(rename_all = "camelCase")]
    Retrying {
        turn_id: u64,
        step: u64,
        step_id: String,
        failed_attempt: u64,
        next_attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_code: Option<u16>,
        since: u64,
    },
    #[serde(rename_all = "camelCase")]
    AwaitingApproval {
        turn_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        approval: Option<Value>,
        since: u64,
    },
    #[serde(rename_all = "camelCase")]
    Interrupted {
        turn_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step: Option<u64>,
        reason: AgentInterruptedReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        at: u64,
    },
    #[serde(rename_all = "camelCase")]
    Ended {
        turn_id: u64,
        reason: TurnEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        at: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatusUpdatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello_world_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello_world_stage: Option<HelloWorldStage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_mode: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tower_exit_reason: Option<TowerExitReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<AgentPhase>,
}
