//! The `write` action wired to the memory store (v2
//! `app/memory/tools/memoryTool.ts:236-293`, git memory off).
//!
//! v2 overwrites unconditionally. This engine's store has an `if_version`
//! optimistic lock, so the handler supplies it internally: `"new"` when the
//! path is unused, otherwise the current content's version token. The model
//! never sees a parameter v2 does not declare.

use std::path::Path;

use serde_json::Value;

use crate::tools::memory_paths::{MemoryScope, content_version};
use crate::tools::memory_store;
use crate::tools::memory_tool::MemoryContext;
use crate::tools::memory_write::{prepare_write, written_message};
use crate::turn_loop::types::ExecutableToolResult;

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

fn parse_scope(value: Option<&Value>) -> Option<MemoryScope> {
    match value.and_then(Value::as_str) {
        Some("global") => Some(MemoryScope::Global),
        Some("project") => Some(MemoryScope::Project),
        Some("session") => Some(MemoryScope::Session),
        _ => None,
    }
}

fn scope_id(scope: MemoryScope, context: &MemoryContext) -> String {
    match scope {
        MemoryScope::Global => String::new(),
        MemoryScope::Project => context.project_id.clone(),
        // The session-scoped id is not carried by MemoryContext; the caller
        // passes it through `session_id` when it has one.
        MemoryScope::Session => String::new(),
    }
}

/// Run `Memory({ action: "write", path, content, scope? })`.
pub fn write_action(
    context: &MemoryContext,
    args: &Value,
    session_id: &str,
) -> ExecutableToolResult {
    let path = args.get("path").and_then(Value::as_str);
    let content = args.get("content").and_then(Value::as_str);
    let scope = parse_scope(args.get("scope"));
    let effective_scope = scope.unwrap_or(MemoryScope::Project);
    let id = if effective_scope == MemoryScope::Session {
        session_id.to_string()
    } else {
        scope_id(effective_scope, context)
    };

    let request = match prepare_write(path, content, scope, &id) {
        Ok(request) => request,
        Err(refusal) => return error_result(refusal.message()),
    };

    // v2 overwrites unconditionally; the store's optimistic lock is supplied
    // internally. Resolve once here so a path-policy error is not mistaken for
    // a missing file and turned into `"new"`.
    let if_version = match memory_store::resolve_read_path(&context.base, &request.rel_path) {
        Ok((_, full_path)) => match std::fs::read_to_string(&full_path) {
            Ok(current) => content_version(&current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "new".to_string(),
            Err(error) => return error_result(error.to_string()),
        },
        Err(error) => return error_result(error),
    };
    if let Err(error) = memory_store::write(
        &context.base,
        &request.rel_path,
        &request.content,
        &if_version,
    ) {
        return error_result(error);
    }
    ok_result(written_message(&request))
}

/// Convenience entry matching the six existing tool functions' signature.
pub fn execute_write(workspace_root: &Path, args: &Value) -> ExecutableToolResult {
    match MemoryContext::resolve(workspace_root) {
        Ok(context) => write_action(&context, args, ""),
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
    fn a_missing_path_is_refused_before_any_io() {
        let dir = tempfile::tempdir().unwrap();
        let result = write_action(&context(dir.path()), &serde_json::json!({}), "");
        assert!(result.is_error);
        assert!(!result.content.is_empty());
    }

    #[test]
    fn a_new_file_is_written_to_the_project_scope() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let result = write_action(
            &context,
            &serde_json::json!({
                "path": "auth",
                "content": "# Authentication\n\nUse JWT."
            }),
            "",
        );
        assert!(!result.is_error, "{}", result.content);
        let file = context
            .base
            .join("projects")
            .join(&context.project_id)
            .join("auth.md");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "# Authentication\n\nUse JWT."
        );
        assert!(result.content.contains("Authentication"));
    }

    #[test]
    fn a_second_write_overwrites_without_a_version_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let args = serde_json::json!({ "path": "note.md", "content": "first" });
        assert!(!write_action(&context, &args, "").is_error);
        let args = serde_json::json!({ "path": "note.md", "content": "second" });
        let result = write_action(&context, &args, "");
        assert!(!result.is_error, "{}", result.content);
        let file = context
            .base
            .join("projects")
            .join(&context.project_id)
            .join("note.md");
        assert_eq!(fs::read_to_string(&file).unwrap(), "second");
    }

    #[test]
    fn an_explicit_global_scope_writes_under_global() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let result = write_action(
            &context,
            &serde_json::json!({
                "path": "std.md",
                "scope": "global",
                "content": "body"
            }),
            "",
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(context.base.join("global/std.md").exists());
    }
}
