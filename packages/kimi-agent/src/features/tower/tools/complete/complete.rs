use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_COMPLETE_TOOL_SERVICE_ID: &str = "towerCompleteTool";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerCompleteToolInput {
    pub report: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_artifacts: Option<Vec<String>>,
}

impl TowerCompleteToolInput {
    pub fn validate(&self) -> Result<(), String> {
        if trim_js(&self.report).is_empty() {
            return Err("report must not be empty".to_string());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TowerCompleteToolInputWire {
    report: String,
    #[serde(default, deserialize_with = "deserialize_optional")]
    key_artifacts: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for TowerCompleteToolInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TowerCompleteToolInputWire::deserialize(deserializer)?;
        let input = Self {
            report: wire.report,
            key_artifacts: wire.key_artifacts,
        };
        input.validate().map_err(serde::de::Error::custom)?;
        Ok(input)
    }
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

pub fn tower_complete_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["report"],
        "properties": {
            "report": {
                "type": "string",
                "description": "Full completion report for the tower and the next review round"
            },
            "key_artifacts": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Required for artifact missions: committed paths of the key deliverables for human acceptance. Approval covers the entire completion tip, not only these paths"
            }
        },
        "additionalProperties": false
    })
}
