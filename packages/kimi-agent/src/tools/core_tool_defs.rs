//! Tool definitions for the core native tools (Read / Grep / Glob / Write /
//! Edit / Bash / FetchURL / WebSearch), so the REPL's LLM can discover and
//! call them. Descriptions and input schemas mirror the v2 agent tool
//! definitions (`packages/agent-core-v2/src/agent/tools/...`): descriptions
//! are the rendered `.md` templates (placeholders replaced with the v2
//! constants) and schemas are the v2 `toInputJsonSchema` output of the zod
//! input schemas.

use std::sync::OnceLock;

use serde_json::json;

use crate::turn_loop::types::ToolInfo;

const READ_DESCRIPTION: &str = r#"Read a text file from the active environment's filesystem. Do not fully trust comments and documentation.

The path may be a `kimi-file://` attachment reference. Its bytes come from the current session's storage, independently of the workspace environment. Next Read keeps the reference so pagination also works after a fork. For a binary attachment, the error includes a server-local path when available; a converter must be able to access that filesystem. ReadMediaFile accepts the same reference for images and videos.

If the user provides a concrete file path to a text file, call Read directly. Do not `Glob`, `ls`, or otherwise pre-check known text file paths; missing or invalid file paths return errors you can handle.

When you need several files, prefer to read them in parallel: emit multiple `Read` calls in a single response instead of reading one file per turn.

- Relative paths resolve against the working directory; a path outside the working directory must be absolute.
- Output is `<line-number>\t<content>` per line. A `<system>` status block may follow; it is not part of the file.
- Results are bounded by `max_chars`: 100000 by default, up to 500000, and by about 500000 divided by four estimated tokens.
- If the status block reports an incomplete range, continue with its `Next Read` arguments exactly. Do not answer from a partial page when the task requires the remaining content.
- **Images and videos** (max 100MB) are sent to the model as multimodal media instead of text; this requires a model with the matching vision capability (`image_in` / `video_in`). `region` and `full_resolution` apply only to images; `line_offset` and `n_lines` apply only to text files.
- Large images are downsampled by default when automatic compression can safely fit them within model limits, which can blur fine detail (small text, dense UI). Compute absolute coordinates from the original dimensions reported in the `<system>` block, never by measuring the displayed copy. When the `<system>` tag reports downsampling and you need that detail, call Read again with the `region` parameter (original-image pixel coordinates) to view a crop at full fidelity, or set `full_resolution` to true when the whole file fits the per-image byte limit. Re-reading the same file without these parameters just reproduces the same downsampled image.
- If automatic compression cannot safely produce an image within model limits, the tool returns an error and does not send the original image. Follow the error: use Bash or an available image-processing tool to create a smaller copy, then read that copy. Do not retry the unchanged file.
- After a successful `Edit`/`Write`, do not re-read solely to prove the write landed.

This engine's own additions, which v2 has no reason to state:

- Sensitive files (`.env` files, credential stores, SSH private keys, and similar secrets) are refused. Templates and public keys are exempt: `.env.example` / `.env.sample` / `.env.template` and public keys such as `id_rsa.pub` read normally.
- UTF-16 LE/BE text (with or without a BOM) is detected and transcoded to UTF-8 for display; the status block names the detected encoding, and Edit/Write on such a file still expect UTF-8. Other encodings (for example GBK), binary files, and files containing NUL bytes are refused.
- `line_offset` accepts a negative value to read from the end of the file (for example, -100 reads the last 100 lines).
- Pure CRLF files are displayed with LF line endings; mixed or lone carriage returns are shown as `\r` and need exact escapes in a later Edit.
"#;
const GREP_DESCRIPTION: &str = r#"Search file contents using regular expressions (powered by ripgrep).

Use Grep when the task is to find unknown content or unknown file locations. Do not use shell `grep` or `rg` directly; this tool applies workspace path policy, output limits, and sensitive-file filtering.
ALWAYS use Grep tool instead of running `grep` or `rg` from a shell — direct shell calls bypass workspace policy, output limits, and sensitive-file filtering.
If you already know a concrete file path and need to inspect its contents, use Read directly instead.

Write patterns in ripgrep regex syntax, which differs from POSIX `grep` syntax. For example, braces are special, so escape them as `\{` to match a literal `{`.

Hidden files (dotfiles such as `.gitlab-ci.yml` or `.eslintrc.json`) are searched by default. To also search files excluded by `.gitignore` (such as `node_modules` or build outputs), set `include_ignored` to `true`. Sensitive files (such as `.env`) are always skipped for safety, even when `include_ignored` is `true`.
"#;

const GLOB_DESCRIPTION: &str = r#"Find files by glob pattern, sorted by modification time (most recent first).

Powered by ripgrep. Matches are files only — directories themselves are never listed; to find a directory, glob for a file inside it (e.g. `**/fixtures/**`).

Good patterns:
- `*.ts` — recursive from the search root; `src/*.ts` — only directly inside `src/`
- `src/**/*.ts` — recursive beneath `src/`
- `*.{ts,tsx}` and `{src,test}/**/*.ts` — brace expansion

Follow returned continuation offsets with unchanged search arguments. If results are partial, narrow the search and retry.

Avoid broad searches in dependency or build-output directories, especially with `include_ignored`; prefer a narrow `path` or `pattern`.
"#;
const WRITE_DESCRIPTION: &str = r#"Create, append to, or replace a file entirely.

- Missing parent directories are created automatically (like `mkdir(parents=True, exist_ok=True)`).
- Mode defaults to overwrite; append adds content at EOF without adding a newline.
- Use Write only when the file does not exist, you intend a complete replacement, or the new contents have little continuity with the old contents.
- Do not create unsolicited documentation files (`*.md` write-ups, `README`s, summaries) just because a task finished — write one only when the user asks for it, or when a task or project instruction requires it (e.g. the plan-mode plan file, created with Write when plan mode directs you to, or a changeset the repo mandates).
- Read before overwriting an existing file.
- Write ignores the Read/Edit line-number view. NEVER include line prefixes.
- Write outputs content literally, including supplied line endings: \n stays LF, \r\n stays CRLF.
- For new content too large for one call, overwrite the first chunk, then append subsequent chunks. Never chunk Write to modify an existing file.
"#;

const EDIT_DESCRIPTION: &str = r#"Perform exact replacements in existing files.

- Drop the line-number prefix and tab; match only file content.
- `old_string` must be unique unless `replace_all` is set.
- If `old_string` is ambiguous, add surrounding context.
- Multiple Edit calls may run in one response only when they do not target the same file.
- DO NOT issue consecutive Edit calls on the same file. A previous Edit can invalidate a later Edit's `old_string`, causing `old_string not found`. Read the file again before the next Edit.

Line endings, which v2 does not have to state: for pure CRLF files Read shows LF, so write `old_string` and `new_string` with LF and Edit restores CRLF; for mixed endings Read shows carriage returns as \r and those positions need actual \r escapes.
"#;
const BASH_DESCRIPTION: &str = r#"Execute a `${SHELL_NAME}` command. Use this for shell semantics — pipes, env, and anything the dedicated tools do not cover.

- `ls` / `find` (list files) → `Glob`
- `grep` / `rg` (search file contents) → `Grep`
- `echo` / `printf` (talk to the user) → just output text directly

Each call runs in a fresh shell: variables and `cd` do not persist, so pass `cwd` or use absolute paths. stdout and stderr are combined and may be truncated; a non-zero exit ends the output with `Command failed with exit code: N`. Do not run interactive commands or commands that never exit. Set `timeout` for possibly long-running commands. A foreground command that hits its timeout is moved to the background instead of being killed, and you are notified when it completes. The user can also move a running foreground command to the background at any time.

For long-running builds, tests, watchers, or servers that the conversation should not wait on, set `run_in_background=true` with a short `description`; the call returns a task ID. Set `disable_timeout=true` only when the task should run without a timeout. You are notified automatically when the task completes, so return control to the user instead of waiting on it. Use `TaskOutput` only for a non-blocking snapshot, and `TaskStop` only to cancel.

- Use `&&` for commands that depend on each other. Issue independent read-only commands (separate `git show`, `ls`, or status checks) as parallel Bash calls in one response instead of chaining them or stitching their output with `echo` separators.
- Avoid `..` paths outside the working directory, and do not modify files outside it unless explicitly instructed.
- Do not use superuser privileges unless explicitly instructed. Run git-mutating commands such as `git commit`, `git push`, `git reset`, and `git rebase` only when the user asks for them.
- Command availability depends on the host; run `which <command>` before relying on an uncommon one. For GitHub work (PRs, issues, CI runs, API queries), prefer the `gh` CLI when installed.
"#;
const FETCH_URL_DESCRIPTION: &str = r#"Fetch content from a URL. The content is returned either as the main text extracted from the page, or as the full response body verbatim; a note at the top of the result states which of the two you received, so you can judge how complete it is. Use this when you need to read a specific web page.

Only fully-formed public `http`/`https` URLs are supported; other schemes and private or loopback addresses are not fetched. Very large pages may be truncated or refused. The fetch carries no login or session for the target site, so pages behind authentication (private repositories, internal dashboards) return a login page or an error instead of the real content — if the text you get back looks like a generic landing or sign-in page, treat that as the login wall, not the answer, and reach the content through a credentialed route (an authenticated CLI or MCP tool) instead.
"#;

const WEB_SEARCH_DESCRIPTION: &str = r#"Search the web for information. Use this when you need up-to-date information from the internet.

Each result includes its title, its URL, and a snippet, plus its source site and publication date when available. Results are short summaries, not full pages — when a result looks relevant, call the FetchURL tool on its URL to read the full page content. Fetch only the few URLs you actually need. Prefer specific queries, and refine the query if the results don't contain what you need.

When you rely on a result in your answer, cite its source URL so the user can verify it.
"#;

/// Windows-only suffix appended to the Glob description, mirroring the v2
/// `GlobTool.description` getter (pathClass === 'win32').
const GLOB_WINDOWS_PATH_HINT: &str = r#"

Windows note: the `path` argument accepts both Windows paths (e.g. `C:\Users\foo`) and POSIX-style paths (e.g. `/c/Users/foo`). Matched paths are returned in Windows backslash form; convert them to forward slashes before using them in a Bash command."#;

fn glob_description() -> String {
    if cfg!(windows) {
        format!("{GLOB_DESCRIPTION}{GLOB_WINDOWS_PATH_HINT}")
    } else {
        GLOB_DESCRIPTION.to_string()
    }
}

/// The Bash tool's description with `${SHELL_NAME}` substituted — v2 rendered
/// the same placeholder from its environment probe, so the tool names the
/// shell it actually runs.
fn bash_description() -> String {
    let (shell_name, _) = crate::prompt::environment::detect_shell(None);
    BASH_DESCRIPTION.replace("${SHELL_NAME}", &shell_name)
}

/// Tool definitions for the core native tools, so the model can discover
/// and call them (used by the standalone REPL).
/// All core native tool definitions. Built once and cached: the agent rebuilds
/// its tool table every step, and these schemas are static, so re-running the
/// `json!` construction each time is pure waste.
pub fn core_tool_defs() -> Vec<ToolInfo> {
    static CACHE: OnceLock<Vec<ToolInfo>> = OnceLock::new();
    CACHE.get_or_init(build_core_tool_defs).clone()
}

fn build_core_tool_defs() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "Read".into(),
            description: READ_DESCRIPTION.into(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to a file. Text files are read as text; image and video files are sent to the model as multimodal content (requires a model with the matching vision capability). Relative paths resolve against the working directory; a path outside the working directory must be absolute. Directories are not supported; use `ls` via Bash for a known directory, or Glob for pattern search."
                    },
                    "line_offset": {
                        "description": "The line number to start reading from. Omit to start at line 1. Negative values read from the end of the file (for example, -100 reads the last 100 lines).",
                        "anyOf": [
                            {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 9007199254740991i64
                            },
                            {
                                "type": "integer",
                                "exclusiveMaximum": 0
                            }
                        ]
                    },
                    "column_offset": {
                        "description": "Zero-based character offset within the first line of a forward read, excluding its line-number prefix. Uses character length in the displayed text. Copy continuation arguments from the previous result to resume a long line.",
                        "type": "integer",
                        "minimum": 0
                    },
                    "n_lines": {
                        "description": "The number of lines to read. Omit to read toward the end of the file. Results are bounded by max_chars, with continuation arguments when the requested range is incomplete. Text files only.",
                        "type": "integer",
                        "exclusiveMinimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "max_chars": {
                        "description": "Maximum characters in the returned text, including line numbers and status. Omit for the configured default; requests above the configured maximum are capped.",
                        "type": "integer",
                        "minimum": 1
                    },
                    "region": {
                        "description": "Images only: view just this rectangle of the image (original-image pixel coordinates). Use after a downsampled full view to inspect fine detail — a region within the size limits is delivered at full fidelity.",
                        "type": "object",
                        "properties": {
                            "x": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 9007199254740991i64,
                                "description": "Left edge of the crop, in original-image pixels."
                            },
                            "y": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 9007199254740991i64,
                                "description": "Top edge of the crop, in original-image pixels."
                            },
                            "width": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 9007199254740991i64,
                                "description": "Crop width, in original-image pixels."
                            },
                            "height": {
                                "type": "integer",
                                "minimum": 1,
                                "maximum": 9007199254740991i64,
                                "description": "Crop height, in original-image pixels."
                            }
                        },
                        "required": [
                            "x",
                            "y",
                            "width",
                            "height"
                        ],
                        "additionalProperties": false
                    },
                    "full_resolution": {
                        "description": "Images only: skip the default downscaling and view at native resolution. Fails with an explicit error when the payload would exceed the per-image byte limit; use region for files that large.",
                        "type": "boolean"
                    }
                },
                "required": [
                    "path"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "Grep".into(),
            description: GREP_DESCRIPTION.into(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression to search for."
                    },
                    "path": {
                        "description": "File or directory to search. Accepts an absolute path, or a path relative to the current working directory. Omit to search the current working directory. Use Read instead when you already know a concrete file path and need its contents.",
                        "type": "string"
                    },
                    "glob": {
                        "description": "Optional glob filter for which files to search, e.g. `*.ts`. Matched against each file's full absolute path, so a path-anchored pattern like `src/**/*.ts` silently matches nothing — use a basename pattern (`*.ts`), or anchor with `**/` (`**/src/**/*.ts`). To scope the search to a directory, use `path` instead.",
                        "type": "string"
                    },
                    "type": {
                        "description": "Optional ripgrep file type filter, such as ts or py. Prefer this over `glob` when filtering by language or file kind: it is more efficient and less error-prone than an equivalent glob pattern.",
                        "type": "string"
                    },
                    "output_mode": {
                        "description": "Shape of the result. `content` shows matching lines (honors `-A`, `-B`, `-C`, `-n`, and `head_limit`); `files_with_matches` shows only the paths of files that contain a match, most-recently-modified first (honors `head_limit`); `count_matches` shows per-file match counts as `path:count` lines, preceded by an aggregate total line. Defaults to `files_with_matches`.",
                        "type": "string",
                        "enum": [
                            "content",
                            "files_with_matches",
                            "count_matches"
                        ]
                    },
                    "-i": {
                        "description": "Perform a case-insensitive search. Defaults to false.",
                        "type": "boolean"
                    },
                    "-n": {
                        "description": "Prefix each matching line with its line number. Applies only when `output_mode` is `content`. Defaults to true.",
                        "type": "boolean"
                    },
                    "-A": {
                        "description": "Number of lines to show after each match. Applies only when `output_mode` is `content`.",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "-B": {
                        "description": "Number of lines to show before each match. Applies only when `output_mode` is `content`.",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "-C": {
                        "description": "Number of lines to show before and after each match. Applies only when `output_mode` is `content`; takes precedence over `-A` and `-B`.",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "head_limit": {
                        "description": "Limit output to the first N lines/entries after offset. Defaults to 250. Pass 0 for unlimited.",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "offset": {
                        "description": "Number of leading lines/entries to skip before applying `head_limit`. Use it together with `head_limit` to page through large result sets. Defaults to 0.",
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "multiline": {
                        "description": "Enable multiline matching, where the pattern can span line boundaries and `.` also matches newlines. Defaults to false.",
                        "type": "boolean"
                    },
                    "include_ignored": {
                        "description": "Also search files excluded by ignore files such as `.gitignore`, `.ignore`, and `.rgignore` (for example `node_modules` or build outputs). Sensitive files (such as `.env`) remain filtered out for safety. VCS metadata directories (`.git` and similar) are always skipped, even when this is true. Defaults to false.",
                        "type": "boolean"
                    }
                },
                "required": [
                    "pattern"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "Glob".into(),
            description: glob_description(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern to match files."
                    },
                    "path": {
                        "description": "Directory to search. Accepts an absolute path, or a path relative to the current working directory. Defaults to the current working directory.",
                        "type": "string"
                    },
                    "head_limit": {
                        "description": "Maximum number of matching paths to return after offset. Defaults to 100. Pass 0 to remove the match-count limit. The character limit still applies: large pages are saved for Read, and a continuation offset is provided when more paths remain. Search time and output capture limits still apply.",
                        "type": "integer",
                        "minimum": 0
                    },
                    "offset": {
                        "description": "Number of matching paths to skip. Defaults to 0. Each call searches the current filesystem again; changes can shift results between pages.",
                        "type": "integer",
                        "minimum": 0
                    },
                    "include_ignored": {
                        "description": "Also match files excluded by ignore files such as `.gitignore`, `.ignore`, and `.rgignore` (for example `node_modules` or build outputs). Sensitive files (such as `.env`) remain filtered out for safety. VCS metadata directories (`.git` and similar) are always skipped, even when this is true. Defaults to false.",
                        "type": "boolean"
                    },
                    "include_dirs": {
                        "description": "Deprecated and ignored. Results are always files-only — directories are never listed. Accepted only so older calls that still pass this flag are not rejected by parameter validation.",
                        "type": "boolean"
                    }
                },
                "required": [
                    "pattern"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "Write".into(),
            description: WRITE_DESCRIPTION.into(),
            input_schema: json!({
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
                        "description": "Write mode. Defaults to overwrite. append adds content to the end exactly as provided and does not add a newline.",
                        "type": "string",
                        "enum": [
                            "overwrite",
                            "append"
                        ]
                    }
                },
                "required": [
                    "path",
                    "content"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "Edit".into(),
            description: EDIT_DESCRIPTION.into(),
            input_schema: json!({
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
                        "description": "Set true only when every occurrence of old_string should be replaced.",
                        "type": "boolean"
                    }
                },
                "required": [
                    "path",
                    "old_string",
                    "new_string"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "Bash".into(),
            description: bash_description(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "minLength": 1,
                        "description": "The command to execute."
                    },
                    "cwd": {
                        "description": "The working directory in which to run the command. When omitted, the command runs in the session's working directory.",
                        "type": "string"
                    },
                    "timeout": {
                        "default": 60,
                        "description": "Optional timeout in seconds for the command to execute. Foreground default 60s, max 300s. Background default 600s, max 86400s. Ignored for background commands when disable_timeout=true.",
                        "type": "integer",
                        "exclusiveMinimum": 0,
                        "maximum": 9007199254740991i64
                    },
                    "description": {
                        "description": "A short description for the background task. Required when run_in_background is true.",
                        "type": "string"
                    },
                    "run_in_background": {
                        "description": "Whether to run the command as a background task.",
                        "type": "boolean"
                    },
                    "disable_timeout": {
                        "description": "If true, do not apply a timeout to the command. Only applies when run_in_background is true.",
                        "type": "boolean"
                    }
                },
                "required": [
                    "command"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "FetchURL".into(),
            description: FETCH_URL_DESCRIPTION.into(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch content from."
                    }
                },
                "required": [
                    "url"
                ],
                "additionalProperties": false
            }),
        },
        ToolInfo {
            name: "WebSearch".into(),
            description: WEB_SEARCH_DESCRIPTION.into(),
            input_schema: json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The query text to search for."
                    }
                },
                "required": [
                    "query"
                ],
                "additionalProperties": false
            }),
        },
    ]
}

const NOTIFY_USER_DESCRIPTION: &str = r#"Show the end user a short progress update without ending your turn. Main-agent and subagent updates appear together in the TUI's Updates panel, with source labels added automatically. The panel keeps every update in order until the main agent starts a new turn. It remains visible when work ends, and the user can page through the complete messages.

**When to use:**
1. Early in a multi-step task, describe your approach so the user can follow your work.
2. Report meaningful findings and phase conclusions, distinguishing confirmed results from hypotheses.
3. Before a long-running step, say what you are waiting for and why.
4. When blocked, explain the blocker and your next step.

**How to use:**
- Write one or two sentences of light Markdown in the end user's language. Avoid repeating unchanged status or narrating individual tool calls.
- When working as a subagent, describe only your own subtask. Its completion does not mean the whole task is complete.
- Do not add an agent name or source prefix; the UI supplies it.
- Batch the update with your next tool calls when possible.
- This tool informs the end user; it does not automatically send a message to your parent agent. Keep all important findings in your final reply or final handoff to the parent.
- Do not use an update to ask questions, request decisions, or deliver the final answer. Subagents must leave questions for the parent agent in their handoff.
"#;

/// The tool name the nudge streak scans for (`NOTIFY_USER_TOOL_NAME` upstream).
pub const NOTIFY_USER_TOOL_NAME: &str = "NotifyUser";

/// Tool definition for the native NotifyUser tool.
pub fn notify_user_tool_def() -> ToolInfo {
    ToolInfo {
        name: "NotifyUser".into(),
        description: NOTIFY_USER_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "Short progress update in light Markdown to show the user."
                }
            },
            "required": ["message"],
            "additionalProperties": false
        }),
    }
}

const LSP_DESCRIPTION: &str = r#"Query a Language Server (LSP) for code intelligence, such as definition locations, references, hover documentation, and document symbols.

Supported actions:
- 'definition': Find where a symbol at line and character position is defined.
- 'references': Find all reference locations for a symbol at line and character position.
- 'hover': Get documentation and type information for a symbol at line and character position.
- 'symbols': Get a hierarchical symbol tree (functions, classes, traits) for the target file.
"#;

/// Tool definition for the native LSP tool.
pub fn lsp_tool_def() -> ToolInfo {
    ToolInfo {
        name: "lsp".into(),
        description: LSP_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "LSP action to perform: 'definition', 'references', 'hover', or 'symbols'."
                },
                "path": {
                    "type": "string",
                    "description": "Path to the target file."
                },
                "line": {
                    "type": "integer",
                    "description": "0-based line number (for definition, references, hover)."
                },
                "character": {
                    "type": "integer",
                    "description": "0-based character column (for definition, references, hover)."
                },
                "include_declaration": {
                    "type": "boolean",
                    "description": "Optional: whether to include declaration in references (defaults to true)."
                }
            },
            "required": ["action", "path"],
            "additionalProperties": false
        }),
    }
}

/// All native tool definitions including LSP, NotifyUser and the six memory
/// tools.
///
/// Invariant: every entry here must satisfy [`crate::tools::is_native_tool_name`].
/// This list is what `GET /api/v1/tools` reports as active builtins, so an entry
/// the toolset cannot execute is a promise the engine cannot keep.
pub fn all_native_tool_defs() -> Vec<ToolInfo> {
    let mut defs = core_tool_defs();
    defs.push(lsp_tool_def());
    defs.push(notify_user_tool_def());
    defs.extend(memory_tool_defs());
    defs
}

const MEMORY_READ_DESCRIPTION: &str = r#"Read one or more memory files.

Returns each file's content and its version token — pass that token as `if_version` on the next write to the same file. Read a file before updating it, and read whenever a question concerns the user or their world: the listing says which files exist, not what is in them.
"#;

const MEMORY_WRITE_DESCRIPTION: &str = r#"Create a memory file, or replace one in full.

`memory_write` replaces the whole file — any line you leave out is deleted. Use `if_version: "new"` only for a path that is not in the listing; otherwise pass the version `memory_read` returned.
"#;

const MEMORY_STR_REPLACE_DESCRIPTION: &str = r#"Change one part of a memory file.

`old_str` must match the file's text exactly once; zero or several matches are rejected and return the current content. Use it for a small edit — use `memory_write` when the change touches many lines.
"#;

const MEMORY_APPEND_DESCRIPTION: &str = r#"Add a line to the end of a memory file.

Use it for a fact the file does not cover yet; use `memory_str_replace` to change a line that is already there.
"#;

const MEMORY_LIST_DESCRIPTION: &str = r#"Refresh the memory listing.

Returns every file with its size, last update, version, and a one-line preview. Use it when the injected listing may be stale, or with `path_prefix` to inspect a scope the listing does not cover (another project, or a session).
"#;

const MEMORY_DELETE_DESCRIPTION: &str = r#"Remove a whole memory file.

Only call it when the user explicitly asks to forget something — never proactively, not to clean up, deduplicate, or drop a file that looks stale. Read the file first to get its `if_version`.
"#;

/// Tool definitions for the six memory tools, in the order the memory section
/// lists them.
pub fn memory_tool_defs() -> Vec<ToolInfo> {
    vec![
        memory_read_tool_def(),
        memory_write_tool_def(),
        memory_str_replace_tool_def(),
        memory_append_tool_def(),
        memory_list_tool_def(),
        memory_delete_tool_def(),
    ]
}

/// Tool definition for `memory_read`.
pub fn memory_read_tool_def() -> ToolInfo {
    ToolInfo {
        name: "memory_read".into(),
        description: MEMORY_READ_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path": {
                    "description": "Memory path, or an array of up to 20 of them.",
                    "anyOf": [
                        { "type": "string" },
                        {
                            "type": "array",
                            "items": { "type": "string" },
                            "maxItems": 20
                        }
                    ]
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    }
}

/// Tool definition for `memory_write`.
pub fn memory_write_tool_def() -> ToolInfo {
    ToolInfo {
        name: "memory_write".into(),
        description: MEMORY_WRITE_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Memory path, e.g. `global/profile.md` or `projects/<id>/areas/<name>.md`."
                },
                "content": {
                    "type": "string",
                    "description": "The file's full new content, frontmatter included."
                },
                "if_version": {
                    "type": "string",
                    "description": "The version `memory_read` returned, or \"new\" for a path that is not in the listing."
                }
            },
            "required": ["path", "content", "if_version"],
            "additionalProperties": false
        }),
    }
}

/// Tool definition for `memory_str_replace`.
pub fn memory_str_replace_tool_def() -> ToolInfo {
    ToolInfo {
        name: "memory_str_replace".into(),
        description: MEMORY_STR_REPLACE_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Memory path to edit."
                },
                "old_str": {
                    "type": "string",
                    "description": "Text to replace; must match the file exactly once."
                },
                "new_str": {
                    "type": "string",
                    "description": "Replacement text."
                },
                "if_version": {
                    "type": "string",
                    "description": "The version `memory_read` returned."
                }
            },
            "required": ["path", "old_str", "new_str", "if_version"],
            "additionalProperties": false
        }),
    }
}

/// Tool definition for `memory_append`.
pub fn memory_append_tool_def() -> ToolInfo {
    ToolInfo {
        name: "memory_append".into(),
        description: MEMORY_APPEND_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Memory path to append to."
                },
                "content": {
                    "type": "string",
                    "description": "The line to add at the end of the file."
                },
                "if_version": {
                    "type": "string",
                    "description": "The version `memory_read` returned, or \"new\" to create the file."
                }
            },
            "required": ["path", "content", "if_version"],
            "additionalProperties": false
        }),
    }
}

/// Tool definition for `memory_list`.
pub fn memory_list_tool_def() -> ToolInfo {
    ToolInfo {
        name: "memory_list".into(),
        description: MEMORY_LIST_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path_prefix": {
                    "type": "string",
                    "description": "Optional path prefix, e.g. `global/people/` or `sessions/<id>/`. Defaults to the global scope plus the current project."
                },
                "cursor": {
                    "type": "string",
                    "description": "Optional cursor from a previous `memory_list` result, to read the next page."
                }
            },
            "additionalProperties": false
        }),
    }
}

/// Tool definition for `memory_delete`.
pub fn memory_delete_tool_def() -> ToolInfo {
    ToolInfo {
        name: "memory_delete".into(),
        description: MEMORY_DELETE_DESCRIPTION.into(),
        input_schema: json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Memory path to remove."
                },
                "if_version": {
                    "type": "string",
                    "description": "The version `memory_read` returned."
                }
            },
            "required": ["path", "if_version"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs_by_name() -> Vec<(String, ToolInfo)> {
        core_tool_defs()
            .into_iter()
            .map(|d| (d.name.clone(), d))
            .collect()
    }

    #[test]
    fn test_core_tool_defs_expose_all_eight_tools() {
        let defs = core_tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Read",
                "Grep",
                "Glob",
                "Write",
                "Edit",
                "Bash",
                "FetchURL",
                "WebSearch"
            ]
        );
    }

    #[test]
    fn test_descriptions_render_v2_placeholder_values() {
        let defs = defs_by_name();
        let get = |name: &str| {
            defs.iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("missing def: {name}"))
                .1
                .description
                .clone()
        };

        // Read: the page budget is characters, not lines or bytes (v2
        // `read.md`: "Results are bounded by `max_chars`"), and a negative
        // `line_offset` has no schema ceiling.
        let read = get("Read");
        assert!(read.contains("Results are bounded by `max_chars`"));
        assert!(!read.contains("truncated mid-line"));
        assert!(!read.contains("1000-line cap"));
        assert!(read.contains("(max 100MB)"));
        assert!(!read.contains("absolute value cannot exceed"));

        // Bash: ${SHELL_NAME} / ${DEFAULT_TIMEOUT_S} / ${MAX_TIMEOUT_S} /
        // ${DEFAULT_BACKGROUND_TIMEOUT_S} / ${MAX_BACKGROUND_TIMEOUT_S}.
        let bash = get("Bash");
        let (shell_name, _) = crate::prompt::environment::detect_shell(None);
        assert!(bash.contains(&format!("Execute a `{shell_name}` command")));
        assert!(!bash.contains("${SHELL_NAME}"));
        assert!(bash.contains("moved to the background instead of being killed"));
        assert!(bash.contains("set `run_in_background=true` with a short `description`"));
        assert!(bash.contains("Issue independent read-only commands"));
        assert!(!bash.contains("Foreground commands default to 60s"));

        // Glob: v2's template, including the continuation-offset instruction.
        assert!(get("Glob").contains("Follow returned continuation offsets"));
        assert!(!get("Glob").contains("first 100 matching paths"));

        for (name, def) in &defs {
            assert!(!def.description.is_empty(), "empty description for {name}");
        }
    }

    #[test]
    fn test_input_schemas_have_object_shape_and_required_fields() {
        let defs = defs_by_name();
        for (name, def) in &defs {
            assert_eq!(def.input_schema["type"], "object", "type for {name}");
            assert!(
                def.input_schema["properties"].is_object(),
                "properties for {name}"
            );
            assert_eq!(
                def.input_schema["additionalProperties"], false,
                "additionalProperties for {name}"
            );
        }

        let required = |name: &str| {
            defs.iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("missing def: {name}"))
                .1
                .input_schema["required"]
                .clone()
        };
        assert_eq!(required("Read"), json!(["path"]));
        assert_eq!(required("Grep"), json!(["pattern"]));
        assert_eq!(required("Glob"), json!(["pattern"]));
        assert_eq!(required("Write"), json!(["path", "content"]));
        assert_eq!(
            required("Edit"),
            json!(["path", "old_string", "new_string"])
        );
        assert_eq!(required("Bash"), json!(["command"]));
        assert_eq!(required("FetchURL"), json!(["url"]));
        assert_eq!(required("WebSearch"), json!(["query"]));
    }

    #[test]
    fn test_field_descriptions_match_v2_zod_describe_text() {
        let defs = defs_by_name();
        let get = |name: &str| {
            defs.iter()
                .find(|(n, _)| n == name)
                .unwrap_or_else(|| panic!("missing def: {name}"))
                .1
                .input_schema
                .clone()
        };

        // Read.path — verbatim v2 zod .describe() text.
        assert_eq!(
            get("Read")["properties"]["path"]["description"],
            "Path to a file. Text files are read as text; image and video files are sent to the model as multimodal content (requires a model with the matching vision capability). Relative paths resolve against the working directory; a path outside the working directory must be absolute. Directories are not supported; use `ls` via Bash for a known directory, or Glob for pattern search."
        );

        // Grep.pattern — verbatim v2 zod .describe() text.
        assert_eq!(
            get("Grep")["properties"]["pattern"]["description"],
            "Regular expression to search for."
        );

        // Bash.timeout — the v2 .describe() text with rendered constants.
        assert_eq!(
            get("Bash")["properties"]["timeout"]["description"],
            "Optional timeout in seconds for the command to execute. Foreground default 60s, max 300s. Background default 600s, max 86400s. Ignored for background commands when disable_timeout=true."
        );

        // Read.line_offset — v2's `z.union([positive, negative])` maps to anyOf
        // with no lower bound on the negative branch; the fork used to pin a
        // -1000 floor here that v2 never had.
        let read_schema = get("Read");
        let any_of = read_schema["properties"]["line_offset"]["anyOf"]
            .as_array()
            .expect("line_offset anyOf");
        assert_eq!(any_of[0]["type"], "integer");
        assert_eq!(any_of[0]["minimum"], 1);
        assert_eq!(any_of[1]["type"], "integer");
        assert_eq!(any_of[1]["exclusiveMaximum"], 0);
        assert!(
            any_of[1].get("minimum").is_none(),
            "v2's negative branch has no floor"
        );

        // Read.region — nested object with its own required list.
        let region = &read_schema["properties"]["region"];
        assert_eq!(region["type"], "object");
        assert_eq!(region["required"], json!(["x", "y", "width", "height"]));
        assert_eq!(region["additionalProperties"], false);
    }

    #[test]
    fn test_lsp_tool_def_schema() {
        let lsp = lsp_tool_def();
        assert_eq!(lsp.name, "lsp");
        assert_eq!(lsp.input_schema["required"], json!(["action", "path"]));
        assert_eq!(lsp.input_schema["additionalProperties"], false);

        let all = all_native_tool_defs();
        assert_eq!(all.len(), 16);
        assert!(all.iter().any(|d| d.name == "lsp"));
        assert!(all.iter().any(|d| d.name == "NotifyUser"));
    }

    #[test]
    fn memory_tool_defs_match_the_memory_section() {
        let defs = memory_tool_defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "memory_read",
                "memory_write",
                "memory_str_replace",
                "memory_append",
                "memory_list",
                "memory_delete"
            ]
        );
        for def in &defs {
            assert_eq!(def.input_schema["type"], "object");
            assert_eq!(def.input_schema["additionalProperties"], false);
            assert!(!def.description.is_empty());
        }
        assert_eq!(
            memory_read_tool_def().input_schema["required"],
            json!(["path"])
        );
        assert_eq!(
            memory_write_tool_def().input_schema["required"],
            json!(["path", "content", "if_version"])
        );
        assert_eq!(
            memory_str_replace_tool_def().input_schema["required"],
            json!(["path", "old_str", "new_str", "if_version"])
        );
        assert_eq!(
            memory_append_tool_def().input_schema["required"],
            json!(["path", "content", "if_version"])
        );
        assert_eq!(
            memory_delete_tool_def().input_schema["required"],
            json!(["path", "if_version"])
        );
        // `memory_list` takes no required argument: it defaults to the global
        // scope plus the current project.
        assert!(
            memory_list_tool_def()
                .input_schema
                .get("required")
                .is_none()
        );
        assert_eq!(
            memory_read_tool_def().input_schema["properties"]["path"]["anyOf"][1]["maxItems"],
            20
        );
    }

    /// `GET /api/v1/tools` reports this list as active builtins. An entry the
    /// toolset refuses (`handles()` → false) is forwarded to a host that has no
    /// tool runtime, so it can only ever fail — which is exactly what happened
    /// to `Lsp` while it was missing from `NATIVE_TOOL_NAMES`.
    #[test]
    fn advertised_native_tools_are_executable() {
        for def in all_native_tool_defs() {
            assert!(
                crate::tools::is_native_tool_name(&def.name),
                "{} is advertised by all_native_tool_defs() but is not a natively executable tool name",
                def.name
            );
        }
    }

    #[test]
    fn test_notify_user_tool_def_schema() {
        let nu = notify_user_tool_def();
        assert_eq!(nu.name, "NotifyUser");
        assert_eq!(nu.input_schema["required"], json!(["message"]));
        assert_eq!(nu.input_schema["additionalProperties"], false);
        assert_eq!(nu.input_schema["properties"]["message"]["type"], "string");
    }
}
