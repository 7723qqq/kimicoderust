use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GET_GOAL_TOOL_SERVICE_ID: &str = "getGoalTool";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GetGoalToolInput {}

pub fn get_goal_input_schema() -> Value {
    crate::tools::get_goal::get_goal_tool_def().input_schema
}
