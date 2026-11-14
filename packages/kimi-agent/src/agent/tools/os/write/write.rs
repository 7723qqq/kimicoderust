use serde_json::{Value, json};

pub use crate::tools::file_ops::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    Overwrite,
    Append,
}

impl WriteMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overwrite => "overwrite",
            Self::Append => "append",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteInput {
    pub path: String,
    pub content: String,
    pub mode: Option<WriteMode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteOutput {
    pub bytes_written: usize,
}

pub fn write_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to the file to create, append to, or completely overwrite. Relative paths resolve against the working directory; a path outside the working directory must be absolute. Missing parent directories are created automatically."
            },
            "content": {
                "type": "string",
                "description": "Raw full file content to write exactly as provided. This does not use the Read/Edit text view."
            },
            "mode": {
                "type": "string",
                "enum": ["overwrite", "append"],
                "description": "Write mode. Defaults to overwrite. append adds content to the end exactly as provided and does not add a newline."
            }
        },
        "required": ["path", "content"],
        "additionalProperties": false
    })
}

pub fn write_output_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "bytesWritten": { "type": "integer", "minimum": 0 }
        },
        "required": ["bytesWritten"],
        "additionalProperties": false
    })
}
