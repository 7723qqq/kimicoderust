use std::path::Path;

use serde_json::Value;

use crate::tools::memory_lock::MemoryLock;
use crate::tools::memory_paths::{MemoryScope, content_version};
use crate::tools::memory_repo::{MemoryRepo, git_memory_enabled, has_git_reference, lock_path};
use crate::tools::memory_store;
use crate::tools::memory_tool::MemoryContext;
use crate::tools::memory_write::{
    prepare_write, write_needs_reference_message, written_committed_message,
    written_committed_unknown_sha_message, written_message, written_not_committed_message,
};
use crate::turn_loop::types::ExecutableToolResult;

fn error_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        spill: None,
        parts: Vec::new(),
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

fn scope_id(scope: MemoryScope, context: &MemoryContext) -> String {
    match scope {
        MemoryScope::Global => String::new(),
        MemoryScope::Project => context.project_id.clone(),

        MemoryScope::Session => String::new(),
    }
}

pub fn write_action(
    context: &MemoryContext,
    args: &Value,
    session_id: &str,
) -> ExecutableToolResult {
    write_action_with_git_memory(context, args, session_id, git_memory_enabled())
}

pub fn write_action_with_git_memory(
    context: &MemoryContext,
    args: &Value,
    session_id: &str,
    git_memory: bool,
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

    if git_memory && !has_git_reference(&context.workspace_root, &context.base, &request.content) {
        return error_result(write_needs_reference_message(&request));
    }

    let (if_version, existing) =
        match memory_store::resolve_read_path(&context.base, &request.rel_path) {
            Ok((_, full_path)) => match std::fs::read_to_string(&full_path) {
                Ok(current) => (content_version(&current), true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ("new".to_string(), false)
                }
                Err(error) => return error_result(error.to_string()),
            },
            Err(error) => return error_result(error),
        };

    let lock = lock_path(&context.base).and_then(|path| MemoryLock::acquire(&path));
    let commit_allowed = lock.is_some();

    if let Err(error) = memory_store::write(
        &context.base,
        &request.rel_path,
        &request.content,
        &if_version,
    ) {
        return error_result(error);
    }
    if !git_memory {
        return ok_result(written_message(&request));
    }
    if !commit_allowed {
        return ok_result(written_not_committed_message(&request));
    }
    let repo = MemoryRepo::new(context.base.clone());
    let verb = if existing { "update" } else { "remember" };
    if !repo.commit(&format!("{verb}: {}", request.title)) {
        return ok_result(written_not_committed_message(&request));
    }
    let sha = repo
        .log_commits(1)
        .first()
        .map(|commit| commit.sha.clone())
        .unwrap_or_default();
    if sha.is_empty() {
        return ok_result(written_committed_unknown_sha_message(&request));
    }
    ok_result(written_committed_message(&request, &sha))
}

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

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn workspace_commit(dir: &Path) -> String {
        let repo = MemoryRepo::new(dir.to_path_buf());
        fs::write(dir.join("seed.txt"), "seed").unwrap();
        assert!(repo.commit("seed"));
        repo.log_commits(1)[0].sha.clone()
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

    #[test]
    fn git_memory_refuses_content_without_a_resolvable_commit() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let result = write_action_with_git_memory(
            &context,
            &serde_json::json!({ "path": "note", "content": "# Note\n\nno sha" }),
            "",
            true,
        );
        assert!(result.is_error, "{}", result.content);
        assert!(!context.base.exists());
    }

    #[test]
    fn git_memory_commits_the_write_and_reports_the_commit() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let sha = workspace_commit(dir.path());
        let result = write_action_with_git_memory(
            &context,
            &serde_json::json!({
                "path": "note",
                "content": format!("# Note\n\nsee {sha}")
            }),
            "",
            true,
        );
        assert!(!result.is_error, "{}", result.content);

        let commits = MemoryRepo::new(context.base.clone()).log_commits(1);
        assert_eq!(commits[0].message, "remember: Note");
        assert!(
            result.content.contains(&commits[0].sha),
            "{}",
            result.content
        );
    }

    #[test]
    fn git_memory_off_writes_without_creating_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let result = write_action_with_git_memory(
            &context,
            &serde_json::json!({ "path": "note", "content": "# Note" }),
            "",
            false,
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(!context.base.join(".git").exists());
    }
}
