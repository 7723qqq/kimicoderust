use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_INBOX_TOOL_SERVICE_ID: &str = "towerInboxTool";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerInboxToolInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_limit")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub include_read: Option<bool>,
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

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn tower_inbox_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "description": "Max messages to return (default 20), newest first"
            },
            "include_read": {
                "type": "boolean",
                "description": "Include previously acknowledged messages for history lookup; only returned messages are acknowledged"
            }
        },
        "additionalProperties": false
    })
}
