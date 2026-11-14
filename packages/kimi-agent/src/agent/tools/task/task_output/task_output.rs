use serde_json::{Value, json};

pub fn task_output_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "The background task ID to inspect."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
