use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::repoMapTool::RepoMapToolExecution;

pub const REPO_MAP_TOOL_NAME: &str = "RepoMap";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapInput {
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_path")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_depth")]
    pub depth: Option<usize>,
}

fn deserialize_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let path = String::deserialize(deserializer)?;
    if path.is_empty() {
        return Err(serde::de::Error::custom("path must not be empty"));
    }
    Ok(Some(path))
}

fn deserialize_depth<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    let depth = value
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("depth must be an integer from 1 to 20"))?;
    if !depth.is_finite() || depth.fract() != 0.0 || !(1.0..=20.0).contains(&depth) {
        return Err(serde::de::Error::custom(
            "depth must be an integer from 1 to 20",
        ));
    }
    Ok(Some(depth as usize))
}

pub fn repo_map_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "minLength": 1,
                "description": "Repository-relative path prefix to list, for example \"packages/agent-core-v2\". Omit to list the whole repository."
            },
            "depth": {
                "type": "integer",
                "minimum": 1,
                "maximum": 20,
                "description": "How deep to list below the prefix: depth 1 returns the direct children, depth 2 their children, and so on. The default is 3."
            }
        }
    })
}

pub trait IRepoMapTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn repo_mutation(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn resolve_execution(&self, args: RepoMapInput) -> RepoMapToolExecution;
}
