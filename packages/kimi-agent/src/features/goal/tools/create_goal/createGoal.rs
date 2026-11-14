use serde_json::Value;

pub use super::super::super::types::CreateGoalInput;

pub const CREATE_GOAL_TOOL_SERVICE_ID: &str = "createGoalTool";

pub fn create_goal_input_schema() -> Value {
    crate::tools::create_goal::create_goal_tool_def().input_schema
}
