use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::tools::skill::{MAX_SKILL_QUERY_DEPTH, nested_skill_too_deep_error};

pub const SKILL_TOOL_SERVICE_ID: &str = "skillTool";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkillToolInput {
    pub skill: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub args: Option<String>,
}

pub fn skill_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "skill": {
                "type": "string",
                "description": "The exact name of the skill to invoke, copied verbatim from the current skill listing."
            },
            "path": {
                "type": "string",
                "description": "Optional exact Path from the current skill listing, used to select a same-name variant."
            },
            "args": {
                "type": "string",
                "description": "Optional argument string for the skill, written like a command line (e.g. `-m \"fix bug\"`, `123`, a file path). It is split on whitespace (quotes group a token) and expanded into the skill's placeholders ($NAME, $1, $ARGUMENTS); if the skill body has no placeholders, the whole string is still appended as a trailing `ARGUMENTS:` line. Omit it only when there is nothing to pass."
            }
        },
        "required": ["skill"],
        "additionalProperties": false
    })
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}
