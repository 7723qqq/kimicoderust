use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::features::tower::protocol::types::TowerAgentKind;

pub const TOWER_SPAWN_TOOL_SERVICE_ID: &str = "towerSpawnTool";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TowerSpawnToolInput {
    pub name: String,
    pub kind: TowerAgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl TowerSpawnToolInput {
    pub fn validate(&self) -> Result<(), String> {
        match self.kind {
            TowerAgentKind::Worker
                if self.mission_id.as_deref().map(trim_js).unwrap_or("").is_empty() =>
            {
                Err("worker spawns require mission_id".to_string())
            }
            TowerAgentKind::Reviewer
                if self.review_target.as_deref().map(trim_js).unwrap_or("").is_empty() =>
            {
                Err("reviewer spawns require review_target".to_string())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TowerSpawnToolInputWire {
    name: String,
    kind: TowerAgentKind,
    #[serde(default, deserialize_with = "deserialize_optional")]
    model: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    mission_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    review_target: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    instructions: Option<String>,
}

impl<'de> Deserialize<'de> for TowerSpawnToolInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TowerSpawnToolInputWire::deserialize(deserializer)?;
        let input = Self {
            name: wire.name,
            kind: wire.kind,
            model: wire.model,
            mission_id: wire.mission_id,
            review_target: wire.review_target,
            instructions: wire.instructions,
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

pub fn tower_spawn_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["name", "kind"],
        "properties": {
            "name": {
                "type": "string",
                "description": "Unique tower name for the agent (e.g. \"agent-build\", \"reviewer-a\"). Used for inbox addressing and mission ownership."
            },
            "kind": {
                "type": "string",
                "enum": ["worker", "reviewer"],
                "description": "workers execute a mission in their worktree; reviewers review one branch"
            },
            "model": {
                "type": "string",
                "description": "Model for this new spawn: one of the aliases listed under \"Available models\" in this tool description. Pass this only when the user explicitly requests a model; when omitted, the subagent binds the pool default, or inherits the tower model when no pool is configured."
            },
            "mission_id": {
                "type": "string",
                "description": "Required for workers: the mission id (e.g. \"M1\") from TowerPlan"
            },
            "review_target": {
                "type": "string",
                "description": "Required for reviewers: the branch to review (e.g. \"feat/vulkan-build\")"
            },
            "instructions": {
                "type": "string",
                "description": "Extra tower instructions appended to the generated briefing"
            }
        },
        "additionalProperties": false
    })
}
