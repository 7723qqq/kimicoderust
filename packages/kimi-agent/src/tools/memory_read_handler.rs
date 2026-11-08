//! The `read` action wired to the memory store (v2
//! `app/memory/tools/memoryTool.ts:215-234`).
//!
//! This is the first handler with a real I/O path. It reads through
//! [`memory_store::read`], which owns path resolution and the store boundary,
//! then renders v2's seven-line result. The fork has no indexed `MemoryEntry`
//! with `scopeId` / `updatedAt`, so those come from the parsed relative path
//! and the file's mtime.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::tools::memory_paths::{detect_type, extract_title, parse_memory_path};
use crate::tools::memory_read::{normalize_path, not_found, read_path_required, render_read};
use crate::tools::memory_store;
use crate::tools::memory_tool::MemoryContext;
use crate::turn_loop::types::ExecutableToolResult;

/// An error-shaped result with no output, kept local so this module does not
/// reach into another tool's helper visibility.
fn error_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        delivery: None,
        stop_turn: false,
        content: text,
        is_error: true,
        note: None,
        display: None,
    }
}

fn ok_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        delivery: None,
        stop_turn: false,
        content: text,
        is_error: false,
        note: None,
        display: None,
    }
}

/// Run `Memory({ action: "read", path })`. An unknown or missing `path` is a
/// model-visible result, exactly as v2's handler returns.
pub fn read_action(context: &MemoryContext, args: &Value) -> ExecutableToolResult {
    let Some(raw_path) = args.get("path").and_then(Value::as_str) else {
        return error_result(read_path_required());
    };
    let rel_path = normalize_path(raw_path);
    let (canonical_path, full_path) =
        match memory_store::resolve_read_path(&context.base, &rel_path) {
            Ok(resolved) => resolved,
            Err(error) => return error_result(error),
        };
    if !full_path.exists() {
        return error_result(not_found(&canonical_path));
    }
    let (body, _version) = match memory_store::read(&context.base, &canonical_path) {
        Ok(read) => read,
        Err(error) => return error_result(error),
    };

    let parsed = match parse_memory_path(&canonical_path) {
        Some(parsed) => parsed,
        None => return error_result(not_found(&canonical_path)),
    };
    let file_name = parsed
        .file_name
        .rsplit('/')
        .next()
        .unwrap_or(&parsed.file_name);
    let title = extract_title(&body, file_name);
    let memory_type = detect_type(&body).as_str().to_string();
    let updated = std::fs::metadata(&full_path)
        .and_then(|meta| meta.modified())
        .map(DateTime::<Utc>::from)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default();

    ok_result(render_read(
        &canonical_path,
        parsed.scope.as_str(),
        &parsed.scope_id,
        &memory_type,
        &title,
        &updated,
        &body,
    ))
}

/// Convenience entry matching the six existing tool functions' signature.
pub fn execute_read(workspace_root: &Path, args: &Value) -> ExecutableToolResult {
    match MemoryContext::resolve(workspace_root) {
        Ok(context) => read_action(&context, args),
        Err(error) => error_result(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn context(dir: &Path) -> MemoryContext {
        MemoryContext::new(dir.join("memory"), dir)
    }

    #[test]
    fn a_missing_path_is_a_model_visible_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_action(&context(dir.path()), &serde_json::json!({}));
        assert!(result.is_error);
        assert!(!result.content.is_empty());
    }

    #[test]
    fn a_missing_file_is_reported_with_the_normalized_path() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_action(
            &context(dir.path()),
            &serde_json::json!({ "path": "global/absent" }),
        );
        assert!(result.is_error);
        assert!(
            result.content.contains("global/absent.md"),
            "{}",
            result.content
        );
    }

    #[test]
    fn a_real_file_renders_v2s_seven_lines() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let file = context.base.join("global/auth-pattern.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "# Authentication Pattern\n\nUse JWT.\n").unwrap();

        let result = read_action(
            &context,
            &serde_json::json!({ "path": "global/auth-pattern" }),
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.starts_with("# Authentication Pattern\n"));
        assert!(result.content.contains("path: global/auth-pattern.md"));
        assert!(result.content.contains("scope: global\n"));
        assert!(result.content.contains("type: note\n"));
        assert!(
            result.content.contains("\n\nUse JWT.\n") || result.content.ends_with("Use JWT.\n")
        );
    }
}
