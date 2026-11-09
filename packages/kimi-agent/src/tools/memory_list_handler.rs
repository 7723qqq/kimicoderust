use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::tools::memory_list::{ListEntry, render_list, scope_prefix};
use crate::tools::memory_paths::MemoryScope;
use crate::tools::memory_store;
use crate::tools::memory_tool::MemoryContext;
use crate::turn_loop::types::ExecutableToolResult;

fn ok_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
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

pub fn list_action(
    context: &MemoryContext,
    args: &Value,
    session_id: &str,
) -> ExecutableToolResult {
    let scope = parse_scope(args.get("scope"));
    let prefix = scope_prefix(scope, &context.project_id, session_id);

    let prefixes: Vec<&str> = match prefix.as_deref() {
        None => vec!["", "projects/", "sessions/"],
        Some(prefix) => vec![prefix],
    };
    let mut by_path: BTreeMap<String, ListEntry> = BTreeMap::new();
    for candidate in prefixes {
        for entry in memory_store::list_entries_for(&context.base, &context.project_id, candidate) {
            by_path.insert(
                entry.rel_path.clone(),
                ListEntry {
                    path: entry.rel_path,
                    title: Some(entry.title),
                    memory_type: Some(entry.memory_type.as_str().to_string()),
                },
            );
        }
    }
    let entries: Vec<ListEntry> = by_path.into_values().collect();
    ok_result(render_list(&entries, prefix.as_deref()))
}

pub fn execute_list(workspace_root: &Path, args: &Value) -> ExecutableToolResult {
    match MemoryContext::resolve(workspace_root) {
        Ok(context) => list_action(&context, args, ""),
        Err(error) => ExecutableToolResult {
            spill: None,
            parts: Vec::new(),
            delivery: None,
            stop_turn: false,
            content: error,
            is_error: true,
            note: None,
            display: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn context(dir: &Path) -> MemoryContext {
        MemoryContext::new(dir.join("memory"), dir)
    }

    fn write(context: &MemoryContext, rel: &str, body: &str) {
        let path = context.base.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn an_empty_store_uses_v2s_no_files_result() {
        let dir = tempfile::tempdir().unwrap();
        let result = list_action(&context(dir.path()), &serde_json::json!({}), "");
        assert!(
            result.content.contains("No memory files found"),
            "{}",
            result.content
        );
    }

    #[test]
    fn the_default_list_covers_all_three_scope_roots() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        write(&context, "global/g.md", "# Global\n");
        write(&context, "projects/other/p.md", "# Other Project\n");
        write(&context, "sessions/sess-1/s.md", "# Session\n");
        let result = list_action(&context, &serde_json::json!({}), "sess-1");
        assert!(!result.is_error);
        assert!(
            result.content.contains("Memory files (3):"),
            "{}",
            result.content
        );
        assert!(result.content.contains("global/g.md"));
        assert!(result.content.contains("projects/other/p.md"));
        assert!(result.content.contains("sessions/sess-1/s.md"));
    }

    #[test]
    fn an_explicit_global_scope_lists_only_global() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        write(&context, "global/g.md", "# Global\n");
        write(&context, "projects/other/p.md", "# Other Project\n");
        let result = list_action(
            &context,
            &serde_json::json!({ "scope": "global" }),
            "sess-1",
        );
        assert!(
            result.content.contains("Memory files (1):"),
            "{}",
            result.content
        );
        assert!(result.content.contains("global/g.md"));
        assert!(!result.content.contains("projects/other/p.md"));
    }

    #[test]
    fn a_project_scope_is_limited_to_the_caller_project() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        write(&context, "projects/other/p.md", "# Other Project\n");
        write(
            &context,
            &format!("projects/{}/own.md", context.project_id),
            "# Own\n",
        );
        let result = list_action(
            &context,
            &serde_json::json!({ "scope": "project" }),
            "sess-1",
        );
        assert!(
            result.content.contains("Memory files (1):"),
            "{}",
            result.content
        );
        assert!(result.content.contains("own.md"));
        assert!(!result.content.contains("other/p.md"));
    }
}
