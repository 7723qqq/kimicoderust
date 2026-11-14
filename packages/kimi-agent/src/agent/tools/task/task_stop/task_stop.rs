use serde_json::{Value, json};

pub fn task_stop_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "task_id": {
                "type": "string",
                "description": "The background task ID to stop."
            },
            "reason": {
                "type": "string",
                "default": "Stopped by TaskStop",
                "description": "Short reason recorded when the task is stopped."
            }
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}
