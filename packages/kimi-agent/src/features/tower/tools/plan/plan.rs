use serde::Deserialize;
use serde_json::Value;

use crate::features::tower::protocol::store::TowerPlanInput;
use crate::features::tower::protocol::types::TowerMissionKind;

pub const TOWER_PLAN_TOOL_SERVICE_ID: &str = "towerPlanTool";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerPlanToolInput {
    pub missions: Vec<TowerPlanInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TowerPlanMissionWire {
    title: String,
    scope: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    tasks: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    context: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    deps: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_optional")]
    kind: Option<TowerMissionKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TowerPlanToolInputWire {
    missions: Vec<TowerPlanMissionWire>,
}

impl<'de> Deserialize<'de> for TowerPlanToolInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = TowerPlanToolInputWire::deserialize(deserializer)?;
        if wire.missions.is_empty() {
            return Err(serde::de::Error::custom(
                "missions must contain at least one mission",
            ));
        }
        if wire.missions.iter().any(|mission| mission.scope.is_empty()) {
            return Err(serde::de::Error::custom(
                "scope must contain at least one entry",
            ));
        }
        Ok(Self {
            missions: wire
                .missions
                .into_iter()
                .map(|mission| TowerPlanInput {
                    title: mission.title,
                    scope: mission.scope,
                    tasks: mission.tasks,
                    context: mission.context,
                    deps: mission.deps,
                    kind: mission.kind,
                })
                .collect(),
        })
    }
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub fn tower_plan_tool_input_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["missions"],
        "properties": {
            "missions": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["title", "scope"],
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Short mission title; becomes the branch/worktree slug. Printable ASCII English only — any non-ASCII character is rejected. Include a unique identifier word (a business code, a module name, a ticket id)."
                        },
                        "scope": {
                            "type": "array",
                            "minItems": 1,
                            "items": { "type": "string" },
                            "description": "Files/globs this mission may touch (e.g. \"src/build/**\"). Scopes of different build and artifact missions should be disjoint; when they overlap, the store auto-adds a dependency that serializes their merges."
                        },
                        "tasks": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Checklist the worker will tick off via TowerMission task_done — write each task as a verifiable item"
                        },
                        "context": {
                            "type": "string",
                            "description": "The user's own key sentences about this mission, copied verbatim — the tower's paraphrase supplements them, never replaces them. Fill this whenever the requirement could be misread; never paste the full conversation history."
                        },
                        "deps": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Mission ids (e.g. \"M1\") that must merge before this one can merge"
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["build", "survey", "artifact"],
                            "description": "\"survey\" = read-only, zero-diff investigation with no reserved scope or review. \"artifact\" = committed deliverables with reserved scope and explicit human acceptance instead of independent review. Default \"build\"."
                        }
                    },
                    "additionalProperties": false
                }
            }
        },
        "additionalProperties": false
    })
}
