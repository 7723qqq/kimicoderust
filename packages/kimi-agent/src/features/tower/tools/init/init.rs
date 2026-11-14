use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const TOWER_INIT_TOOL_SERVICE_ID: &str = "towerInitTool";

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerInitToolInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_base")]
    pub base: Option<String>,
}

fn deserialize_base<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    if value.is_empty() {
        return Err(serde::de::Error::custom("base must contain at least one character"));
    }
    Ok(Some(value))
}

pub fn tower_init_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "base": {
                "type": "string",
                "minLength": 1,
                "description": "Local branch that missions fork from and merge back into (e.g. \"develop\"). Defaults to the branch currently checked out in the main worktree. On an existing workspace an explicit base re-anchors it — allowed anytime, even with open missions, whose branches then rebase onto the new base before merging. Remote-tracking refs (e.g. \"origin/main\") and tags are not accepted — create a local branch first."
            }
        },
        "additionalProperties": false
    })
}
