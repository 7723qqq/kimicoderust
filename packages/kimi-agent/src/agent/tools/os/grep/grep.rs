use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    Content,
    FilesWithMatches,
    CountMatches,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_mode: Option<GrepOutputMode>,
    #[serde(rename = "-i", default, skip_serializing_if = "Option::is_none")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "-n", default, skip_serializing_if = "Option::is_none")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-A", default, skip_serializing_if = "Option::is_none")]
    pub after: Option<usize>,
    #[serde(rename = "-B", default, skip_serializing_if = "Option::is_none")]
    pub before: Option<usize>,
    #[serde(rename = "-C", default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multiline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_ignored: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepOutput {
    pub mode: GrepOutputMode,
    pub num_files: usize,
    pub filenames: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_lines: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_matches: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_limit: Option<usize>,
}

pub trait IGrepTool: Send + Sync {}

pub fn grep_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Regular expression to search for."
            },
            "path": {
                "type": "string",
                "description": "File or directory to search. Accepts an absolute path, or a path relative to the current working directory. Omit to search the current working directory. Use Read instead when you already know a concrete file path and need its contents."
            },
            "glob": {
                "type": "string",
                "description": "Optional glob filter for which files to search, e.g. `*.ts`. Matched against each file's full absolute path, so a path-anchored pattern like `src/**/*.ts` silently matches nothing — use a basename pattern (`*.ts`), or anchor with `**/` (`**/src/**/*.ts`). To scope the search to a directory, use `path` instead."
            },
            "type": {
                "type": "string",
                "description": "Optional ripgrep file type filter, such as ts or py. Prefer this over `glob` when filtering by language or file kind: it is more efficient and less error-prone than an equivalent glob pattern."
            },
            "output_mode": {
                "type": "string",
                "enum": ["content", "files_with_matches", "count_matches"],
                "description": "Shape of the result. `content` shows matching lines (honors `-A`, `-B`, `-C`, `-n`, and `head_limit`); `files_with_matches` shows only the paths of files that contain a match, most-recently-modified first (honors `head_limit`); `count_matches` shows per-file match counts as `path:count` lines, preceded by an aggregate total line. Defaults to `files_with_matches`."
            },
            "-i": {
                "type": "boolean",
                "description": "Perform a case-insensitive search. Defaults to false."
            },
            "-n": {
                "type": "boolean",
                "description": "Prefix each matching line with its line number. Applies only when `output_mode` is `content`. Defaults to true."
            },
            "-A": {
                "type": "integer",
                "minimum": 0,
                "description": "Number of lines to show after each match. Applies only when `output_mode` is `content`."
            },
            "-B": {
                "type": "integer",
                "minimum": 0,
                "description": "Number of lines to show before each match. Applies only when `output_mode` is `content`."
            },
            "-C": {
                "type": "integer",
                "minimum": 0,
                "description": "Number of lines to show before and after each match. Applies only when `output_mode` is `content`; takes precedence over `-A` and `-B`."
            },
            "head_limit": {
                "type": "integer",
                "minimum": 0,
                "description": "Limit output to the first N lines/entries after offset. Defaults to 250. Pass 0 for unlimited."
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Number of leading lines/entries to skip before applying `head_limit`. Use it together with `head_limit` to page through large result sets. Defaults to 0."
            },
            "multiline": {
                "type": "boolean",
                "description": "Enable multiline matching, where the pattern can span line boundaries and `.` also matches newlines. Defaults to false."
            },
            "include_ignored": {
                "type": "boolean",
                "description": "Also search files excluded by ignore files such as `.gitignore`, `.ignore`, and `.rgignore` (for example `node_modules` or build outputs). Sensitive files (such as `.env`) remain filtered out for safety. VCS metadata directories (`.git` and similar) are always skipped, even when this is true. Defaults to false."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

pub fn grep_output_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "mode": {
                "type": "string",
                "enum": ["content", "files_with_matches", "count_matches"]
            },
            "numFiles": { "type": "integer", "minimum": 0 },
            "filenames": { "type": "array", "items": { "type": "string" } },
            "content": { "type": "string" },
            "numLines": { "type": "integer", "minimum": 0 },
            "numMatches": { "type": "integer", "minimum": 0 },
            "appliedLimit": { "type": "integer", "minimum": 0 }
        },
        "required": ["mode", "numFiles", "filenames"],
        "additionalProperties": false
    })
}
