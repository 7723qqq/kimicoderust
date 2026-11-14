use serde_json::{Value, json};

pub use crate::tools::file_ops::*;

pub const DEFAULT_MAX_CHARS: usize = 100_000;
pub const DEFAULT_MAX_CHARS_LIMIT: usize = 500_000;
pub const TRANSCODE_MAX_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadInput {
    pub path: String,
    pub line_offset: Option<i64>,
    pub column_offset: Option<usize>,
    pub n_lines: Option<usize>,
    pub region: Option<ReadRegion>,
    pub full_resolution: Option<bool>,
    pub max_chars: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadOutput {
    pub content: String,
    pub line_count: usize,
}

pub fn read_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Path to a file or a kimi-file:// attachment reference in the current session. Text files are read as text; image and video files are sent to the model as multimodal content (requires a model with the matching vision capability). Relative filesystem paths resolve against the working directory; a path outside the working directory must be absolute. Directories are not supported; use `ls` via Bash for a known directory, or Glob for pattern search."
            },
            "line_offset": {
                "type": "integer",
                "description": "One-based file line number to start reading from, unlike the offset used by Grep and Glob to skip result entries. Omit to start at line 1; 0 is invalid. Negative values select a tail window: with -100, reading starts at the 100th line from the end, and n_lines can limit how many lines are returned from that point; without n_lines, it returns the last 100 lines."
            },
            "column_offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Zero-based character offset within the first line of a forward read, excluding its line-number prefix. Uses JavaScript string length in the displayed text. Copy continuation arguments from the previous result to resume a long line."
            },
            "n_lines": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "description": "Maximum number of lines to return, counted forward from line_offset; with a negative line_offset, counting starts at the computed tail-window start. Omit to read toward the end of the file. This limits line count only: max_chars can further shorten the selected range or a long line. Incomplete ranges include continuation arguments."
            },
            "region": {
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "minimum": 0, "description": "Left edge of the crop, in original-image pixels." },
                    "y": { "type": "integer", "minimum": 0, "description": "Top edge of the crop, in original-image pixels." },
                    "width": { "type": "integer", "minimum": 1, "description": "Crop width, in original-image pixels." },
                    "height": { "type": "integer", "minimum": 1, "description": "Crop height, in original-image pixels." }
                },
                "required": ["x", "y", "width", "height"],
                "additionalProperties": false,
                "description": "Images only: view just this rectangle of the image (original-image pixel coordinates). Use after a downsampled full view to inspect fine detail — a region within the size limits is delivered at full fidelity."
            },
            "full_resolution": {
                "type": "boolean",
                "description": "Images only: skip the default downscaling and view at native resolution. Fails with an explicit error when the payload would exceed the per-image byte limit; use region for files that large."
            },
            "max_chars": {
                "type": "integer",
                "exclusiveMinimum": 0,
                "description": "Maximum rendered characters, including line-number prefixes and the Read status; this is not a line count. It bounds the range selected by line_offset and n_lines and can shorten that range or a long line. Incomplete ranges include continuation arguments. Omit for the configured default; requests above the configured maximum are capped."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

pub fn read_output_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "content": { "type": "string" },
            "lineCount": { "type": "integer", "minimum": 0 }
        },
        "required": ["content", "lineCount"],
        "additionalProperties": false
    })
}
