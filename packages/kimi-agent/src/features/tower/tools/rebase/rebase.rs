use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_REBASE_TOOL_SERVICE_ID: &str = "towerRebaseTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerRebaseToolInput {
    pub mission: String,
}

pub fn tower_rebase_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["mission"],
        "properties": {
            "mission": {
                "type": "string",
                "description": "Mission id (e.g. \"M3\") whose branch should be rebased onto the current base"
            }
        },
        "additionalProperties": false
    })
}
