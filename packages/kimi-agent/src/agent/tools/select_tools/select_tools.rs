use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::{Value, json};

pub const SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY: &str = "Provide names to load or a query to search.";

pub static SELECT_TOOLS_INPUT_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "type": "object",
        "properties": {
            "names": {
                "type": "array",
                "items": { "type": "string" },
                "minItems": 1,
                "description": "Exact tool names to load, taken from the latest announcement or a prior search result."
            },
            "query": {
                "type": "string",
                "minLength": 1,
                "description": "Capability description to search for when the exact tool name is unknown; returns candidates without loading them."
            }
        },
        "additionalProperties": false
    })
});

pub fn select_tools_schema() -> Value {
    SELECT_TOOLS_INPUT_SCHEMA.clone()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SelectToolsInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
}

impl SelectToolsInput {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(names) = &self.names
            && names.is_empty()
        {
            return Err(SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY.to_string());
        }
        if self.query.as_deref() == Some("") {
            return Err(SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY.to_string());
        }
        if self.names.is_none() && self.query.is_none() {
            return Err(SELECT_TOOLS_PROVIDE_NAMES_OR_QUERY.to_string());
        }
        Ok(())
    }
}
