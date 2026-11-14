use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_STATUS_TOOL_SERVICE_ID: &str = "towerStatusTool";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerStatusToolInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_limit")]
    pub limit: Option<usize>,
}

fn deserialize_limit<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let number = value
        .as_f64()
        .filter(|number| number.is_finite() && number.fract() == 0.0 && *number > 0.0)
        .ok_or_else(|| serde::de::Error::custom("limit must be a positive integer"))?;
    Ok(Some(number as usize))
}

pub fn tower_status_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "description": "Maximum entries to return in each detail list (default 20), newest first"
            }
        },
        "additionalProperties": false
    })
}
