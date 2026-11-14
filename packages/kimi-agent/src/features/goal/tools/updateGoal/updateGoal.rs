use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const UPDATE_GOAL_TOOL_SERVICE_ID: &str = "updateGoalTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateGoalToolInput {
    #[serde(deserialize_with = "deserialize_status")]
    pub status: String,
}

fn deserialize_status<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let status = String::deserialize(deserializer)?;
    if !["active", "complete", "blocked"].contains(&status.as_str()) {
        return Err(serde::de::Error::custom(
            "status must be active, complete, or blocked",
        ));
    }
    Ok(status)
}

pub fn update_goal_input_schema() -> Value {
    crate::tools::goal_tools::update_goal_tool_def().input_schema
}
