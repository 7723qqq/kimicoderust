use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TowerSendToolInput {
    pub to: String,
    pub subject: String,
    pub body: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub scope: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub action: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    pub consent_ref: Option<String>,
}

pub const TOWER_SEND_TOOL_SERVICE_ID: &str = "towerSendTool";

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn tower_send_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["to", "subject", "body"],
        "properties": {
            "to": {
                "type": "string",
                "description": "Recipient: a roster agent name, \"tower\", or \"all\" (broadcast)"
            },
            "subject": {
                "type": "string",
                "description": "One-line subject; keep it greppable"
            },
            "body": {
                "type": "string",
                "description": "Full message body (markdown)"
            },
            "scope": {
                "type": "string",
                "description": "Optional scope tag (e.g. the mission id)"
            },
            "action": {
                "type": "string",
                "description": "Optional action tag for machine routing"
            },
            "consent_ref": {
                "type": "string",
                "description": "Optional reference to a consent/approval record this message relies on"
            }
        },
        "additionalProperties": false
    })
}
