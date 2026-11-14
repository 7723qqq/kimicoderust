use serde_json::Value;

use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;
use crate::app::git::work_tree::GitWorkTree;
use crate::environment::environment::EnvironmentPath;
use crate::tool::path_access::{PathClass, is_within_directory};
use crate::turn_loop::types::{FileOperation, ToolFileAccess, ToolResourceAccess};

pub fn file_accesses(context: &ResolvedToolExecutionHookContext) -> Vec<ToolFileAccess> {
    context
        .execution
        .accesses
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|access| match access {
            ToolResourceAccess::File(file) => Some(file.clone()),
            _ => None,
        })
        .collect()
}

pub fn write_file_accesses(context: &ResolvedToolExecutionHookContext) -> Vec<ToolFileAccess> {
    file_accesses(context)
        .into_iter()
        .filter(|access| matches!(access.operation, FileOperation::Write | FileOperation::ReadWrite))
        .collect()
}

pub fn has_git_path_component(target_path: &str, cwd: &str, path_class: PathClass) -> bool {
    relative_path_parts(target_path, cwd, path_class)
        .iter()
        .any(|part| part.eq_ignore_ascii_case(".git"))
}

pub fn is_git_control_path(target_path: &str, marker: &GitWorkTree, path_class: PathClass) -> bool {
    is_within_directory(target_path, &marker.dot_git_path, path_class)
        || is_within_directory(target_path, &marker.control_dir_path, path_class)
}

pub fn default_path_class() -> PathClass {
    if cfg!(target_os = "windows") {
        PathClass::Win32
    } else {
        PathClass::Posix
    }
}

fn relative_path_parts(target_path: &str, cwd: &str, path_class: PathClass) -> Vec<String> {
    path_mod(path_class)
        .relative(cwd, target_path)
        .split(|character| character == '/' || character == '\\')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn path_mod(path_class: PathClass) -> EnvironmentPath {
    EnvironmentPath::new(match path_class {
        PathClass::Win32 => "win32",
        PathClass::Posix => "posix",
    })
}

pub fn legacy_file_accesses(tool_lower: &str, args: &Value) -> Vec<String> {
    if !matches!(
        tool_lower,
        "read"
            | "write"
            | "edit"
            | "grep"
            | "glob"
            | "lsp"
            | "readmediafile"
            | "read_media_file"
            | "memory"
    ) {
        return Vec::new();
    }
    match args.get("path") {
        Some(Value::String(path)) => vec![path.clone()],
        Some(Value::Array(paths)) => paths
            .iter()
            .filter_map(|path| path.as_str())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

pub fn is_legacy_git_control_path(path_str: &str) -> bool {
    let normalized = path_str.replace('\\', "/").to_ascii_lowercase();
    normalized == ".git"
        || normalized.contains("/.git/")
        || normalized.ends_with("/.git")
        || normalized.starts_with(".git/")
}

#[cfg(test)]
mod tests {
    use super::is_legacy_git_control_path;
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::{Value, json};

    #[test]
    fn test_bash_command_text_is_not_a_file_access() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let verdict = engine.evaluate(
            "Bash",
            &json!({ "command": "cat /workspace/project/.git/config" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate("Bash", &json!({ "command": "cat /home/u/project/.env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate(
            "Bash",
            &json!({ "command": "grep -n x /workspace/project/credentials" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate(
            "Bash",
            &json!({ "command": "git commit -F .git/CMSG9.txt" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate("Read", &json!({ "path": "/workspace/project/.git/config" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "GitControlPathAccessAsk");
    }

    #[test]
    fn test_query_and_url_subjects_are_not_file_accesses() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let verdict = engine.evaluate("WebSearch", &json!({ "query": "how to read .env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");

        let verdict = engine.evaluate("FetchURL", &json!({ "url": "https://example.com/.env" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
    }

    #[test]
    fn test_path_shaped_tools_declare_file_accesses() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let sensitive = "/workspace/project/.env";
        for tool in [
            "Read",
            "Write",
            "Edit",
            "Grep",
            "Glob",
            "lsp",
            "ReadMediaFile",
            "Memory",
        ] {
            let verdict = engine.evaluate(tool, &json!({ "path": sensitive }));
            assert_eq!(
                verdict.decision,
                VerdictDecision::Ask,
                "{tool} should declare a file access"
            );
            assert_eq!(verdict.policy_name, "SensitiveFileAccessAsk");
            assert_eq!(verdict.session_approval_rule, None);
        }
    }

    #[test]
    fn test_is_git_control_path() {
        assert!(is_legacy_git_control_path(".git"));
        assert!(is_legacy_git_control_path(".GIT"));
        assert!(is_legacy_git_control_path(".git/config"));
        assert!(is_legacy_git_control_path(".git/HEAD"));
        assert!(is_legacy_git_control_path(".git/index"));
        assert!(is_legacy_git_control_path(".git/refs/heads/main"));
        assert!(is_legacy_git_control_path(".git/hooks/pre-commit"));
        assert!(is_legacy_git_control_path("repo/.git"));
        assert!(is_legacy_git_control_path("repo/.git/config"));
        assert!(is_legacy_git_control_path("packages/app/.git/HEAD"));
        assert!(is_legacy_git_control_path("a/b/c/.git/objects"));
        assert!(is_legacy_git_control_path("repo\\.git"));
        assert!(is_legacy_git_control_path("repo\\.git\\config"));
        assert!(is_legacy_git_control_path("C:\\repo\\.git\\HEAD"));

        assert!(!is_legacy_git_control_path(".gitignore"));
        assert!(!is_legacy_git_control_path(".gitattributes"));
        assert!(!is_legacy_git_control_path(".gitmodules"));
        assert!(!is_legacy_git_control_path(".github/workflows/ci.yml"));
        assert!(!is_legacy_git_control_path(".github/CODEOWNERS"));
        assert!(!is_legacy_git_control_path("git.rs"));
        assert!(!is_legacy_git_control_path("git_controller.ts"));
        assert!(!is_legacy_git_control_path("src/git/mod.rs"));
    }
}
