use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_TEARDOWN_TOOL_SERVICE_ID: &str = "towerTeardownTool";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerTeardownToolInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub force: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub exclude: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub dry_run: Option<bool>,
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn tower_teardown_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "force": {
                "type": "boolean",
                "description": "Remove worktrees even when they contain uncommitted changes — a worktree whose roster agent is still running is kept regardless"
            },
            "exclude": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Worktree names to keep — they are skipped and reported, whatever their state"
            },
            "dry_run": {
                "type": "boolean",
                "description": "Print the would-remove / would-keep list with reasons without changing anything on disk or in state"
            }
        },
        "additionalProperties": false
    })
}
