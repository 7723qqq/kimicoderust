//! v2 `session/agentLifecycle/profile/gitContext.ts`: the `explore` profile's
//! `promptPrefix`.
//!
//! Collected once per spawn (not per session): the subagent sees the
//! repository state at the moment it starts, wrapped in a `<git-context>`
//! block. A non-repository working directory yields the explicit
//! `status="unavailable"` marker; a collection that fails for another reason
//! yields no prefix at all, because a partial report would mislead the model.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tracing::{debug, warn};

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DIRTY_FILES: usize = 20;
const MAX_COMMIT_LINE_LENGTH: usize = 200;

/// Repository hosts whose remote URL is safe to share with the model. A remote
/// on any other host is dropped: the URL may embed credentials or an internal
/// hostname.
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "gitlab.com",
    "gitee.com",
    "bitbucket.org",
    "codeberg.org",
    "git.sr.ht",
];

enum GitFailure {
    Timeout,
    SpawnError,
    CommandFailed {
        exit_code: Option<i32>,
        stderr: String,
    },
}

/// Collect the repository state for `cwd` as a `<git-context>` block.
///
/// Returns an empty string when the directory is a repository but has nothing
/// beyond the working directory to report, and the `status="unavailable"`
/// marker when it is not a repository at all (v2
/// `collectGitContext`).
pub async fn collect_git_context(cwd: &Path) -> String {
    let rev_parse_args = ["rev-parse", "--is-inside-work-tree"];
    match run_git(cwd, &rev_parse_args).await {
        Ok(_) => {}
        Err(GitFailure::CommandFailed { stderr, .. }) if is_not_a_repo(&stderr) => {
            return "<git-context status=\"unavailable\" reason=\"not-a-repo\"/>".to_string();
        }
        Err(failure) => {
            log_git_failure(cwd, &rev_parse_args, &failure);
            return String::new();
        }
    }

    let remote_args = ["remote", "get-url", "origin"];
    let branch_args = ["symbolic-ref", "--short", "HEAD"];
    let status_args = ["status", "--porcelain"];
    let log_args = ["log", "-3", "--format=%h %s"];
    let (remote, branch, status, git_log) = tokio::join!(
        run_git(cwd, &remote_args),
        run_git(cwd, &branch_args),
        run_git(cwd, &status_args),
        run_git(cwd, &log_args),
    );

    for (args, failure) in [
        (&remote_args[..], &remote),
        (&branch_args[..], &branch),
        (&status_args[..], &status),
        (&log_args[..], &git_log),
    ] {
        if let Err(failure) = failure {
            log_git_failure(cwd, args, failure);
        }
    }

    let remote_url = stdout_of(&remote);
    let branch_name = stdout_of(&branch);
    let dirty_raw = stdout_of(&status);
    let log_raw = stdout_of(&git_log);

    let mut sections: Vec<String> = vec![format!("Working directory: {}", cwd.display())];

    if let Some(remote_url) = remote_url.as_deref() {
        if let Some(safe_url) = sanitize_remote_url(remote_url) {
            sections.push(format!("Remote: {safe_url}"));
            if let Some(project) = parse_project_name(&safe_url) {
                sections.push(format!("Project: {project}"));
            }
        }
    }

    if let Some(branch_name) = branch_name.as_deref() {
        sections.push(format!("Branch: {branch_name}"));
    }

    if let Some(dirty_raw) = dirty_raw.as_deref() {
        let dirty_lines: Vec<&str> = dirty_raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if !dirty_lines.is_empty() {
            let total = dirty_lines.len();
            let mut body = dirty_lines
                .iter()
                .take(MAX_DIRTY_FILES)
                .map(|line| format!("  {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            if total > MAX_DIRTY_FILES {
                body.push_str(&format!("\n  ... and {} more", total - MAX_DIRTY_FILES));
            }
            sections.push(format!("Dirty files ({total}):\n{body}"));
        }
    }

    if let Some(log_raw) = log_raw.as_deref() {
        let log_lines: Vec<&str> = log_raw
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if !log_lines.is_empty() {
            let body = log_lines
                .iter()
                .map(|line| {
                    let line: String = line.chars().take(MAX_COMMIT_LINE_LENGTH).collect();
                    format!("  {line}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("Recent commits:\n{body}"));
        }
    }

    if sections.len() <= 1 {
        return String::new();
    }
    format!("<git-context>\n{}\n</git-context>", sections.join("\n"))
}

/// v2 `sanitizeRemoteUrl`: keep an allowed host's ssh form verbatim, and
/// canonicalize an allowed host's URL to `https://host[:port]/path`, dropping
/// userinfo and query. Anything else is dropped.
pub fn sanitize_remote_url(remote_url: &str) -> Option<String> {
    for host in ALLOWED_HOSTS {
        if remote_url.starts_with(&format!("git@{host}:")) {
            return Some(remote_url.to_string());
        }
    }

    let parsed = url::Url::parse(remote_url).ok()?;
    let host = parsed.host_str()?;
    if !ALLOWED_HOSTS.contains(&host) {
        return None;
    }
    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Some(format!("https://{host}{port}{}", parsed.path()))
}

/// v2 `parseProjectName`: `owner/repo`, derived from either the scp-style
/// path or the URL pathname, with leading/trailing slashes and a `.git`
/// suffix trimmed.
pub fn parse_project_name(remote_url: &str) -> Option<String> {
    let raw_path = match scp_path(remote_url) {
        Some(path) => path.to_string(),
        None => url::Url::parse(remote_url).ok()?.path().to_string(),
    };
    let project = raw_path
        .trim_start_matches('/')
        .trim_end_matches('/')
        .trim_end_matches(".git");
    (!project.is_empty()).then(|| project.to_string())
}

/// The path half of `user@host:path` (v2's `/^[^/]+@[^/:]+:(.+)$/`).
fn scp_path(remote_url: &str) -> Option<&str> {
    let at = remote_url.find('@')?;
    if remote_url[..at].contains('/') {
        return None;
    }
    let rest = &remote_url[at + 1..];
    let colon = rest.find(':')?;
    let host = &rest[..colon];
    if host.is_empty() || host.contains('/') {
        return None;
    }
    Some(&rest[colon + 1..])
}

fn stdout_of(result: &Result<String, GitFailure>) -> Option<String> {
    result.as_ref().ok().cloned()
}

fn is_not_a_repo(stderr: &str) -> bool {
    stderr.contains("not a git repository")
}

fn log_git_failure(cwd: &Path, args: &[&str], failure: &GitFailure) {
    let command = format!("git {}", args.join(" "));
    match failure {
        GitFailure::Timeout => debug!(
            cwd = %cwd.display(),
            command,
            "git context command timed out"
        ),
        GitFailure::SpawnError => warn!(
            cwd = %cwd.display(),
            command,
            "git context command failed to spawn"
        ),
        GitFailure::CommandFailed { exit_code, stderr } => debug!(
            cwd = %cwd.display(),
            command,
            exit_code = exit_code.unwrap_or_default(),
            stderr,
            "git context command failed"
        ),
    }
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, GitFailure> {
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // A timed-out collection must not leave a git process behind.
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| GitFailure::SpawnError)?;

    match tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()).await {
        Err(_) => Err(GitFailure::Timeout),
        Ok(Err(_)) => Err(GitFailure::SpawnError),
        Ok(Ok(output)) => {
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
            } else {
                Err(GitFailure::CommandFailed {
                    exit_code: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn ssh_remotes_on_allowed_hosts_are_kept_verbatim() {
        assert_eq!(
            sanitize_remote_url("git@github.com:owner/repo.git").as_deref(),
            Some("git@github.com:owner/repo.git")
        );
        assert_eq!(
            sanitize_remote_url("git@git.sr.ht:~user/repo").as_deref(),
            Some("git@git.sr.ht:~user/repo")
        );
    }

    #[test]
    fn allowed_hosts_are_canonicalized_and_others_are_dropped() {
        assert_eq!(
            sanitize_remote_url("https://github.com/owner/repo.git").as_deref(),
            Some("https://github.com/owner/repo.git")
        );
        // Userinfo and query are dropped.
        assert_eq!(
            sanitize_remote_url("https://token@gitlab.com/group/repo?foo=bar").as_deref(),
            Some("https://gitlab.com/group/repo")
        );
        // A port survives.
        assert_eq!(
            sanitize_remote_url("ssh://git@gitee.com:2222/owner/repo").as_deref(),
            Some("https://gitee.com:2222/owner/repo")
        );
        // An internal host is never disclosed.
        assert_eq!(
            sanitize_remote_url("https://git.internal.corp/owner/repo"),
            None
        );
        assert_eq!(sanitize_remote_url("not a url"), None);
    }

    #[test]
    fn project_names_are_derived_from_both_remote_forms() {
        assert_eq!(
            parse_project_name("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_project_name("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_project_name("https://gitlab.com/group/sub/repo.git/").as_deref(),
            Some("group/sub/repo")
        );
        assert_eq!(parse_project_name("git@github.com:"), None);
        assert_eq!(parse_project_name("not a url"), None);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed");
    }

    #[tokio::test]
    async fn a_non_repository_yields_the_unavailable_marker() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            collect_git_context(temp.path()).await,
            "<git-context status=\"unavailable\" reason=\"not-a-repo\"/>"
        );
    }

    #[tokio::test]
    async fn a_repository_reports_remote_branch_dirty_files_and_commits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        git(
            root,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "first commit"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\n").unwrap();

        let context = collect_git_context(root).await;
        assert!(context.starts_with("<git-context>\n"), "{context}");
        assert!(context.ends_with("\n</git-context>"), "{context}");
        assert!(
            context.contains(&format!("Working directory: {}", root.display())),
            "{context}"
        );
        assert!(
            context.contains("Remote: git@github.com:owner/repo.git"),
            "{context}"
        );
        assert!(context.contains("Project: owner/repo"), "{context}");
        assert!(context.contains("Branch: "), "{context}");
        assert!(context.contains("Dirty files (1):\n  M a.txt"), "{context}");
        assert!(context.contains("Recent commits:\n  "), "{context}");
        assert!(context.contains("first commit"), "{context}");
    }

    #[tokio::test]
    async fn a_clean_repository_without_a_remote_still_reports_state() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-q", "-m", "first commit"]);

        let context = collect_git_context(root).await;
        assert!(context.contains("Working directory: "), "{context}");
        assert!(!context.contains("Remote: "), "{context}");
        assert!(context.contains("Recent commits:"), "{context}");
    }
}
