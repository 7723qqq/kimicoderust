use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::features::goal::types::{GoalActor, GoalBudgetLimits, GoalChange, GoalSnapshot};

pub const GOAL_CREATE_EVENT: &str = "goal.create";
pub const GOAL_UPDATE_EVENT: &str = "goal.update";
pub const GOAL_CLEAR_EVENT: &str = "goal.clear";
pub const GOAL_UPDATED_EVENT: &str = "goal.updated";

pub struct GoalCreate;

impl GoalCreate {
    pub const EVENT_TYPE: &'static str = GOAL_CREATE_EVENT;
}

pub struct GoalUpdate;

impl GoalUpdate {
    pub const EVENT_TYPE: &'static str = GOAL_UPDATE_EVENT;
}

pub struct GoalClear;

impl GoalClear {
    pub const EVENT_TYPE: &'static str = GOAL_CLEAR_EVENT;
}

pub struct GoalUpdated;

impl GoalUpdated {
    pub const EVENT_TYPE: &'static str = GOAL_UPDATED_EVENT;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalUpdatedEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub snapshot: Option<GoalSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<GoalChange>,
}

pub fn goal_create_event(
    agent_id: &str,
    goal_id: &str,
    objective: &str,
    completion_criterion: Option<&str>,
    wall_clock_resumed_at: Option<i64>,
    created_at: Option<i64>,
    updated_at: Option<i64>,
    status: Option<&str>,
    actor: Option<GoalActor>,
    budget_limits: Option<GoalBudgetLimits>,
) -> Value {
    let mut event = json!({
        "type": GOAL_CREATE_EVENT,
        "agentId": agent_id,
        "goalId": goal_id,
        "objective": objective,
    });
    if let Some(value) = completion_criterion {
        event["completionCriterion"] = json!(value);
    }
    if let Some(value) = wall_clock_resumed_at {
        event["wallClockResumedAt"] = json!(value);
    }
    if let Some(value) = created_at {
        event["createdAt"] = json!(value);
    }
    if let Some(value) = updated_at {
        event["updatedAt"] = json!(value);
    }
    if let Some(value) = status {
        event["status"] = json!(value);
    }
    if let Some(value) = actor {
        event["actor"] = json!(value);
    }
    if let Some(value) = budget_limits {
        event["budgetLimits"] = json!(value);
    }
    event
}

pub fn goal_update_event(agent_id: &str, fields: Value) -> Value {
    let mut event = fields;
    event["type"] = json!(GOAL_UPDATE_EVENT);
    event["agentId"] = json!(agent_id);
    event
}

pub fn goal_clear_event(agent_id: &str) -> Value {
    json!({
        "type": GOAL_CLEAR_EVENT,
        "agentId": agent_id,
    })
}

pub fn goal_updated_event(
    agent_id: &str,
    snapshot: Option<GoalSnapshot>,
    change: Option<GoalChange>,
) -> Value {
    let mut event = json!({
        "type": GOAL_UPDATED_EVENT,
        "agentId": agent_id,
        "snapshot": snapshot,
    });
    if let Some(change) = change {
        event["change"] = json!(change);
    }
    event
}
