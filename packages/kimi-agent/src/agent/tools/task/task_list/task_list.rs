use serde_json::{Value, json};

pub fn task_list_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "active_only": {
                "type": "boolean",
                "default": true,
                "description": "Whether to list only non-terminal background tasks."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": 100,
                "default": 20,
                "description": "Maximum number of tasks to return."
            }
        },
        "additionalProperties": false
    })
}
