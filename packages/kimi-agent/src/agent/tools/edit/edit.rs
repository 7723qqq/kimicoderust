use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditInput {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_all: Option<bool>,
}

pub trait IEditTool: Send + Sync {}

pub fn edit_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to the text file to edit. Relative paths resolve against the working directory; a path outside the working directory must be absolute."
            },
            "old_string": {
                "type": "string",
                "minLength": 1,
                "description": "Exact content to replace from the Read output view, without the line-number prefix. Use LF for pure CRLF files; use actual \\r escapes where Read shows \\r."
            },
            "new_string": {
                "type": "string",
                "description": "Replacement text in the same Read output view. LF is written back as CRLF only for pure CRLF files."
            },
            "replace_all": {
                "type": "boolean",
                "description": "Set true only when every occurrence of old_string should be replaced."
            }
        },
        "required": ["path", "old_string", "new_string"],
        "additionalProperties": false
    })
}
