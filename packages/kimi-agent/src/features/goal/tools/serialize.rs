use serde_json::{Value, json};

use super::super::types::{GoalSnapshot, GoalToolResult};

pub fn goal_for_model(goal: &GoalSnapshot) -> Value {
    let mut value = serde_json::to_value(goal).unwrap_or(Value::Null);
    if let Some(object) = value.as_object_mut() {
        object.remove("goalId");
    }
    value
}

pub fn goal_result_for_model(result: &GoalToolResult) -> Value {
    json!({
        "goal": result.goal.as_ref().map(goal_for_model)
    })
}
