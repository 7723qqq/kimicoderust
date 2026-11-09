use serde_json::Value;

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_lock::MemoryLock;
use crate::tools::memory_paths::{extract_title, sanitize_file_name};
use crate::tools::memory_read::{normalize_path, not_found};
use crate::tools::memory_repo::{MemoryRepo, git_memory_enabled, lock_path};
use crate::tools::memory_store;
use crate::tools::memory_tool::MemoryContext;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteRefusal {
    MissingPath,
    NotFound(String),
    InvalidFilename(String),
}

impl DeleteRefusal {
    pub fn message(&self) -> String {
        match self {
            Self::MissingPath => {
                LocalizedText::new("engine.tools.memory.deletePathRequired").render()
            }
            Self::NotFound(path) => not_found(path),
            Self::InvalidFilename(path) => LocalizedText::with_params(
                "engine.tools.memory.invalidFilename",
                i18n_params!["path" => path],
            )
            .render(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRequest {
    pub rel_path: String,
    pub file_name: String,
}

pub fn prepare_delete(path: Option<&str>, exists: bool) -> Result<DeleteRequest, DeleteRefusal> {
    let Some(path) = path else {
        return Err(DeleteRefusal::MissingPath);
    };
    let rel_path = normalize_path(path);
    if !exists {
        return Err(DeleteRefusal::NotFound(rel_path));
    }
    let last = rel_path.rsplit('/').next().unwrap_or(&rel_path);
    let Some(file_name) = sanitize_file_name(last) else {
        return Err(DeleteRefusal::InvalidFilename(path.to_string()));
    };
    Ok(DeleteRequest {
        rel_path,
        file_name,
    })
}

pub fn deleted_message(rel_path: &str) -> String {
    LocalizedText::with_params(
        "engine.tools.memory.deleted",
        i18n_params!["path" => rel_path],
    )
    .render()
}

pub fn deleted_not_committed_message(rel_path: &str) -> String {
    LocalizedText::with_params(
        "engine.tools.memory.deletedNotCommitted",
        i18n_params!["path" => rel_path],
    )
    .render()
}

pub fn delete_action(context: &MemoryContext, args: &Value) -> ExecutableToolResult {
    delete_action_with_git_memory(context, args, git_memory_enabled())
}

pub fn delete_action_with_git_memory(
    context: &MemoryContext,
    args: &Value,
    git_memory: bool,
) -> ExecutableToolResult {
    let raw_path = args.get("path").and_then(Value::as_str);
    let exists = match raw_path {
        Some(path) => {
            let rel_path = normalize_path(path);
            match memory_store::resolve_read_path(&context.base, &rel_path) {
                Ok((_, full_path)) => full_path.exists(),

                Err(_) => true,
            }
        }
        None => false,
    };
    let request = match prepare_delete(raw_path, exists) {
        Ok(request) => request,
        Err(refusal) => return error_result(refusal.message()),
    };
    let (body, version) = match memory_store::read(&context.base, &request.rel_path) {
        Ok(read) => read,
        Err(error) => return error_result(error),
    };
    let title = extract_title(&body, &request.file_name);

    let lock = lock_path(&context.base).and_then(|path| MemoryLock::acquire(&path));
    let commit_allowed = lock.is_some();
    if let Err(error) = memory_store::delete(&context.base, &request.rel_path, &version) {
        return error_result(error);
    }
    if !git_memory || !commit_allowed {
        return ok_result(if commit_allowed {
            deleted_message(&request.rel_path)
        } else {
            deleted_not_committed_message(&request.rel_path)
        });
    }
    if !MemoryRepo::new(context.base.clone()).commit(&format!("forget: {title}")) {
        return ok_result(deleted_not_committed_message(&request.rel_path));
    }
    ok_result(deleted_message(&request.rel_path))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;

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

    #[test]
    fn git_memory_delete_commits_a_forget_entry() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let file = context.base.join("global/note.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "# Note\n\nbody").unwrap();
        assert!(MemoryRepo::new(context.base.clone()).commit("remember: Note"));

        let result = delete_action_with_git_memory(
            &context,
            &serde_json::json!({ "path": "global/note" }),
            true,
        );
        assert!(!result.is_error, "{}", result.content);
        assert!(!file.exists());
        let commits = MemoryRepo::new(context.base.clone()).log_commits(1);
        assert_eq!(commits[0].message, "forget: Note");
    }

    #[test]
    fn a_real_file_is_deleted_and_named_in_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let file = context.base.join("global/note.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "body").unwrap();

        let result = delete_action(&context, &serde_json::json!({ "path": "global/note" }));
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("global/note.md"),
            "{}",
            result.content
        );
        assert!(!file.exists());
    }

    #[test]
    fn a_missing_file_is_refused_without_creating_anything() {
        let dir = tempfile::tempdir().unwrap();
        let context = context(dir.path());
        let result = delete_action(&context, &serde_json::json!({ "path": "global/absent" }));
        assert!(result.is_error);
        assert!(
            result.content.contains("global/absent.md"),
            "{}",
            result.content
        );
        assert!(!context.base.join("global/absent.md").exists());
    }

    #[test]
    fn a_missing_path_is_refused_first() {
        assert_eq!(prepare_delete(None, true), Err(DeleteRefusal::MissingPath));
    }

    #[test]
    fn a_missing_file_is_reported_with_the_normalized_path() {
        assert_eq!(
            prepare_delete(Some("global/gone"), false),
            Err(DeleteRefusal::NotFound("global/gone.md".to_string()))
        );
    }

    #[test]
    fn the_suffix_is_added_and_the_last_segment_is_validated() {
        let request = prepare_delete(Some("projects/abc/note"), true).unwrap();
        assert_eq!(request.rel_path, "projects/abc/note.md");
        assert_eq!(request.file_name, "note.md");
    }

    #[test]
    fn only_the_last_segment_is_sanitized_here() {
        let request = prepare_delete(Some("global/../escape"), true).unwrap();
        assert_eq!(request.file_name, "escape.md");
        assert!(prepare_delete(Some("global/.."), true).is_err());
    }

    #[test]
    fn the_deleted_result_names_the_path() {
        assert!(
            deleted_message("global/a.md").contains("global/a.md"),
            "{}",
            deleted_message("global/a.md")
        );
    }

    #[test]
    fn refusals_render_catalog_sentences() {
        assert!(!DeleteRefusal::MissingPath.message().is_empty());
        assert!(
            DeleteRefusal::NotFound("global/a.md".to_string())
                .message()
                .contains("global/a.md")
        );
        assert!(
            DeleteRefusal::InvalidFilename("x/y".to_string())
                .message()
                .contains("x/y")
        );
    }
}
