use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::features::tower::protocol::types::TowerMissionStatus;

pub const TOWER_MISSION_TOOL_SERVICE_ID: &str = "towerMissionTool";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerMissionToolInput {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub status: Option<TowerMissionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub blocker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub clear_blockers: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub task_done: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub task_drop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub task_drop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub scope: Option<Vec<String>>,
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn tower_mission_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["id"],
        "properties": {
            "id": {
                "type": "string",
                "description": "Mission id (e.g. \"M1\")"
            },
            "status": {
                "type": "string",
                "enum": ["planned", "active", "completed", "blocked", "paused", "merged", "abandoned"],
                "description": "New lifecycle status. Workers may update only their own mission. \"abandoned\" is tower-only and gives the mission up without merging. \"merged\" cannot be set here; only TowerMerge records it after the merge gate succeeds. Merged and abandoned are terminal and cannot be reopened by a generic patch."
            },
            "note": {
                "type": "string",
                "description": "Append a decision-log note"
            },
            "blocker": {
                "type": "string",
                "description": "Report a blocker (also sets status to blocked)"
            },
            "clear_blockers": {
                "type": "boolean",
                "description": "Clear all recorded blockers"
            },
            "task_done": {
                "type": "string",
                "description": "Mark the first open task containing this text as done"
            },
            "task_drop": {
                "type": "string",
                "description": "Mark the first open task containing this text as dropped — the escape hatch for legitimately descoped tasks; requires task_drop_reason and is recorded in the mission notes and the activity log"
            },
            "task_drop_reason": {
                "type": "string",
                "description": "Mandatory with task_drop: why the task is legitimately descoped"
            },
            "scope": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Tower only: replace the mission scope globs (picomatch — `**` crosses directories). Logged; widens what the merge gate accepts."
            }
        },
        "additionalProperties": false
    })
}
