use serde::Deserialize;
use serde_json::Value;

use crate::features::tower::protocol::types::{TowerFindingSeverity, TowerFindingType};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TowerFindingToolInput {
    pub r#type: TowerFindingType,
    pub title: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub severity: Option<TowerFindingSeverity>,
    pub summary: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub location: Option<String>,
    pub details: String,
    pub suggested_fix: String,
}

pub const TOWER_FINDING_TOOL_SERVICE_ID: &str = "towerFindingTool";

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn tower_finding_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["type", "title", "summary", "details", "suggested_fix"],
        "properties": {
            "type": {
                "type": "string",
                "enum": ["bug", "improve", "vuln", "idea"],
                "description": "Finding category"
            },
            "title": {
                "type": "string",
                "description": "Short finding title"
            },
            "severity": {
                "type": "string",
                "enum": ["low", "medium", "high", "critical"]
            },
            "summary": {
                "type": "string",
                "description": "What was found, in a sentence or two"
            },
            "location": {
                "type": "string",
                "description": "File/symbol the finding concerns"
            },
            "details": {
                "type": "string",
                "description": "Full details: evidence, reproduction, impact"
            },
            "suggested_fix": {
                "type": "string",
                "description": "What you would do about it"
            }
        },
        "additionalProperties": false
    })
}
