use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const SESSION_QUERY_TOOL_SERVICE_ID: &str = "sessionQueryTool";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionQueryOperation {
    SessionSearch,
    EventSearch,
    SessionTrace,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionQueryToolInput {
    pub operation: SessionQueryOperation,
    #[serde(default, deserialize_with = "deserialize_operation_args")]
    pub operation_args: Option<Map<String, Value>>,
}

fn deserialize_operation_args<'de, D>(deserializer: D) -> Result<Option<Map<String, Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Map::<String, Value>::deserialize(deserializer).map(Some)
}

pub fn session_query_tool_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "operation": {
                "type": "string",
                "enum": ["session_search", "event_search", "session_trace"],
                "description": "Which session-query operation to run: session_search (cross-session full-text search), event_search (within-session event search), or session_trace (fork lineage of one session)."
            },
            "operationArgs": {
                "type": "object",
                "additionalProperties": true,
                "description": "Operation-specific arguments (validated per operation)."
            }
        },
        "required": ["operation"],
        "additionalProperties": false
    })
}

pub fn format_validation_error(message: &str) -> String {
    format!("Invalid arguments: {message}")
}
