use serde_json::Value;

use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::memory_repo::MemoryRepo;
use crate::tools::memory_tool::MemoryContext;
use crate::turn_loop::types::ExecutableToolResult;

pub fn log_action(context: &MemoryContext) -> ExecutableToolResult {
    let commits = MemoryRepo::new(context.base.clone()).log_commits(20);
    let mut lines = vec![
        LocalizedText::with_params(
            "engine.tools.memory.logHeader",
            i18n_params!["count" => commits.len()],
        )
        .render(),
    ];
    for commit in &commits {
        let short_sha: String = commit.sha.chars().take(7).collect();
        lines.push(
            LocalizedText::with_params(
                "engine.tools.memory.logLine",
                i18n_params![
                    "sha" => short_sha,
                    "date" => commit.date,
                    "message" => commit.message
                ],
            )
            .render(),
        );
    }
    ok_result(lines.join("\n"))
}

pub fn revert_action(context: &MemoryContext, args: &Value) -> ExecutableToolResult {
    let Some(sha) = args
        .get("sha")
        .and_then(Value::as_str)
        .filter(|sha| !sha.is_empty())
    else {
        return error_result(
            LocalizedText::with_params(
                "engine.tools.memory.revertFailed",
                i18n_params!["sha" => ""],
            )
            .render(),
        );
    };
    if !MemoryRepo::new(context.base.clone()).revert(sha) {
        return error_result(
            LocalizedText::with_params(
                "engine.tools.memory.revertFailed",
                i18n_params!["sha" => sha],
            )
            .render(),
        );
    }
    ok_result(
        LocalizedText::with_params("engine.tools.memory.reverted", i18n_params!["sha" => sha])
            .render(),
    )
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

fn error_result(text: String) -> ExecutableToolResult {
    ExecutableToolResult {
        is_error: true,
        ..ok_result(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn context(base: &std::path::Path) -> MemoryContext {
        MemoryContext::new(base.to_path_buf(), std::path::Path::new("/workspace"))
    }

    #[test]
    fn log_lists_the_last_commits_newest_first() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join("global")).unwrap();
        std::fs::write(dir.path().join("global/note.md"), "# Note\n\nfirst\n").unwrap();
        assert!(repo.commit("remember: Note"));
        std::fs::write(dir.path().join("global/note.md"), "# Note\n\nsecond\n").unwrap();
        assert!(repo.commit("update: Note"));

        let result = log_action(&context(dir.path()));
        assert!(!result.is_error);
        assert!(result.content.contains("remember: Note"));
        assert!(result.content.contains("update: Note"));
        let update = result.content.find("update: Note").unwrap();
        let remember = result.content.find("remember: Note").unwrap();
        assert!(update < remember, "newest commit must come first");
    }

    #[test]
    fn a_missing_sha_fails_with_the_same_error_as_a_bad_revert() {
        let dir = tempfile::tempdir().unwrap();
        let result = revert_action(&context(dir.path()), &serde_json::json!({}));
        assert!(result.is_error);
        assert!(!result.content.is_empty());
    }

    #[test]
    fn revert_restores_the_previous_revision() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join("global")).unwrap();
        let note = dir.path().join("global/note.md");
        std::fs::write(&note, "# Note\n\nfirst\n").unwrap();
        assert!(repo.commit("remember: Note"));
        std::fs::write(&note, "# Note\n\nsecond\n").unwrap();
        assert!(repo.commit("update: Note"));
        let sha = repo.log_commits(1)[0].sha.clone();

        let result = revert_action(&context(dir.path()), &serde_json::json!({ "sha": sha }));
        assert!(!result.is_error);
        assert_eq!(
            std::fs::read_to_string(&note)
                .unwrap()
                .replace("\r\n", "\n"),
            "# Note\n\nfirst\n"
        );
    }
}
