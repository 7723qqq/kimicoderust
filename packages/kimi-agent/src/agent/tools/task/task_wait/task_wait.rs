use serde_json::{Value, json};

pub const WAIT_FOR_MAX_TIMEOUT_S: u64 = 90;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitForInput {
    pub timeout: u64,
    pub task_id: Option<String>,
}

pub fn wait_for_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "timeout": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "maximum": WAIT_FOR_MAX_TIMEOUT_S,
                "description": "Maximum time to wait, in seconds (1-90). Pick it from how long you expect the task to take, not the maximum. A timeout is not an error: the tool returns the tasks that are still running."
            },
            "task_id": {
                "type": "string",
                "description": "The background task ID to wait for. When omitted, the wait ends as soon as any background task that was running at call time finishes."
            }
        },
        "required": ["timeout"],
        "additionalProperties": false
    })
}
