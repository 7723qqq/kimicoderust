use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_MERGE_TOOL_SERVICE_ID: &str = "towerMergeTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerMergeToolInput {
    pub branch: String,
}

pub fn tower_merge_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["branch"],
        "properties": {
            "branch": {
                "type": "string",
                "description": "The mission branch to merge into the base branch (e.g. \"feat/vulkan-build\")"
            }
        },
        "additionalProperties": false
    })
}
