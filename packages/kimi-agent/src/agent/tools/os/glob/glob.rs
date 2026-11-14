use serde_json::{Value, json};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobInput {
    pub pattern: String,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub path: Option<String>,
    pub include_ignored: Option<bool>,
    pub include_dirs: Option<bool>,
}

pub const DEFAULT_HEAD_LIMIT: usize = 100;
pub const WINDOWS_PATH_HINT: &str = "\n\nWindows note: the `path` argument accepts both Windows paths (e.g. `C:\\Users\\foo`) and POSIX-style paths (e.g. `/c/Users/foo`). Matched paths are returned in Windows backslash form; convert them to forward slashes before using them in a Bash command.";

pub fn glob_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Glob pattern to match files."
            },
            "head_limit": {
                "type": "integer",
                "minimum": 0,
                "description": "Maximum number of matching paths to return after offset. Defaults to 100. Pass 0 to remove the match-count limit. The character limit still applies: large pages are saved for Read, and a continuation offset is provided when more paths remain. Search time and output capture limits still apply."
            },
            "offset": {
                "type": "integer",
                "minimum": 0,
                "description": "Number of matching paths to skip. Defaults to 0. Each call searches the current filesystem again; changes can shift results between pages."
            },
            "path": {
                "type": "string",
                "description": "Directory to search. Accepts an absolute path, or a path relative to the current working directory. Defaults to the current working directory."
            },
            "include_ignored": {
                "type": "boolean",
                "description": "Also match files excluded by ignore files such as `.gitignore`, `.ignore`, and `.rgignore` (for example `node_modules` or build outputs). Sensitive files (such as `.env`) remain filtered out for safety. VCS metadata directories (`.git` and similar) are always skipped, even when this is true. Defaults to false."
            },
            "include_dirs": {
                "type": "boolean",
                "description": "Deprecated and ignored. Results are always files-only — directories are never listed. Accepted only so older calls that still pass this flag are not rejected by parameter validation."
            }
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}
