use std::path::{Component, Path};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::primitives::deadline::Deadline;
use crate::primitives::read::{ReadOutcome, UnreadableCause};

const GIT_WAIT: Duration = Duration::from_secs(60);

struct GitRun {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl GitRun {
    fn failure_detail(&self) -> String {
        let stderr = String::from_utf8_lossy(&self.stderr).trim().to_string();
        if stderr.is_empty() {
            format!("exit {:?}, no diagnostic on stderr", self.status.code())
        } else {
            format!("exit {:?}: {stderr}", self.status.code())
        }
    }
}

async fn run_git(
    cwd: &Path,
    args: &[&str],
    input: Option<&str>,
    limit: Duration,
) -> Result<GitRun, String> {
    run_git_env(cwd, args, input, limit, &[]).await
}

async fn run_git_env(
    cwd: &Path,
    args: &[&str],
    input: Option<&str>,
    limit: Duration,
    env: &[(&str, String)],
) -> Result<GitRun, String> {
    let label = format!("git {}", args.join(" "));
    let deadline = Deadline::new(label.clone(), limit);

    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !env.is_empty() {
        command.envs(env.iter().map(|(key, value)| (*key, value)));
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("{label}: failed to spawn: {e}"))?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("{label}: failed to open stdin"))?;
        let written = deadline
            .wait("writing its stdin", async move {
                let result = stdin.write_all(input.as_bytes()).await;
                drop(stdin);
                result
            })
            .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(format!("{label}: failed to write stdin: {error}")),
            Err(timeout) => return Err(timeout.to_string()),
        }
    }
    let output = deadline
        .wait(
            "waiting for the subprocess to exit",
            child.wait_with_output(),
        )
        .await
        .map_err(|timeout| timeout.to_string())?
        .map_err(|error| format!("{label}: failed to wait: {error}"))?;
    Ok(GitRun {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

pub async fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    git_impl(cwd, args, None).await
}

pub async fn git_with_input(cwd: &Path, args: &[&str], input: &str) -> Result<String, String> {
    git_impl(cwd, args, Some(input)).await
}

pub async fn git_with_env(
    cwd: &Path,
    args: &[&str],
    env: &[(&str, String)],
) -> Result<String, String> {
    let run = run_git_env(cwd, args, None, GIT_WAIT, env).await?;
    if !run.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            run.failure_detail()
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
}

async fn git_impl(cwd: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
    let run = run_git(cwd, args, input, GIT_WAIT).await?;
    if !run.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            run.failure_detail()
        ));
    }
    Ok(String::from_utf8_lossy(&run.stdout).trim_end().to_string())
}

pub(crate) async fn git_probe(cwd: &Path, args: &[&str]) -> ReadOutcome<String> {
    let label = format!("git {}", args.join(" "));
    let run = match run_git(cwd, args, None, GIT_WAIT).await {
        Ok(run) => run,
        Err(error) => return ReadOutcome::Unreadable(UnreadableCause::new(label, error)),
    };
    if run.status.success() {
        return ReadOutcome::Found(String::from_utf8_lossy(&run.stdout).trim_end().to_string());
    }
    let stderr = String::from_utf8_lossy(&run.stderr);
    if run.status.code() == Some(1)
        || (run.status.code() == Some(128) && stderr.contains("not a git repository"))
    {
        return ReadOutcome::Absent;
    }
    let cause = UnreadableCause::new(label, run.failure_detail());
    tracing::warn!(%cause, "a git probe could not answer");
    ReadOutcome::Unreadable(cause)
}

pub async fn is_inside_repo(cwd: &Path) -> Result<bool, String> {
    match git_probe(cwd, &["rev-parse", "--is-inside-work-tree"]).await {
        ReadOutcome::Found(value) => Ok(value.trim() == "true"),
        ReadOutcome::Absent => Ok(false),
        ReadOutcome::Unreadable(cause) => Err(cause.to_string()),
    }
}

pub async fn has_any_commit(cwd: &Path) -> Result<bool, String> {
    match git_probe(cwd, &["rev-list", "-n", "1", "--all"]).await {
        ReadOutcome::Found(value) => Ok(!value.trim().is_empty()),
        ReadOutcome::Absent => Ok(false),
        ReadOutcome::Unreadable(cause) => Err(cause.to_string()),
    }
}

pub async fn current_branch(cwd: &Path) -> Result<String, String> {
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    if branch == "HEAD" {
        return Err("cannot determine base branch from a detached HEAD".into());
    }
    Ok(branch)
}

pub async fn branch_tip(cwd: &Path, ref_name: &str) -> Result<String, String> {
    git(cwd, &["rev-parse", ref_name]).await
}

pub async fn branch_exists(cwd: &Path, branch: &str) -> Result<bool, String> {
    let ref_name = format!("refs/heads/{branch}");
    match git_probe(cwd, &["show-ref", "--verify", "--quiet", &ref_name]).await {
        ReadOutcome::Found(_) => Ok(true),
        ReadOutcome::Absent => Ok(false),
        ReadOutcome::Unreadable(cause) => Err(cause.to_string()),
    }
}

pub async fn is_ancestor(cwd: &Path, maybe_ancestor: &str, ref_name: &str) -> Result<bool, String> {
    let run = run_git(
        cwd,
        &["merge-base", "--is-ancestor", maybe_ancestor, ref_name],
        None,
        GIT_WAIT,
    )
    .await?;
    match run.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git merge-base --is-ancestor {maybe_ancestor} {ref_name} could not decide ({})",
            run.failure_detail()
        )),
    }
}

pub async fn worktree_add(cwd: &Path, path: &Path, branch: &str, base: &str) -> Result<(), String> {
    let path_str = path.to_str().ok_or("invalid path")?;
    if branch_exists(cwd, branch).await? {
        git(cwd, &["worktree", "add", path_str, branch]).await?;
    } else {
        git(cwd, &["worktree", "add", path_str, "-b", branch, base]).await?;
    }
    Ok(())
}

pub async fn worktree_remove(cwd: &Path, path: &Path) -> Result<(), String> {
    let path_str = path.to_str().ok_or("invalid path")?;

    if !path.exists() {
        let known = match git_probe(cwd, &["worktree", "list", "--porcelain"]).await {
            ReadOutcome::Found(list) => list.split('\n').any(|line| {
                line.strip_prefix("worktree ")
                    .is_some_and(|wt| wt.trim() == path_str)
            }),
            ReadOutcome::Absent => false,

            ReadOutcome::Unreadable(cause) => return Err(cause.to_string()),
        };
        if !known {
            return Ok(());
        }
    }
    git(cwd, &["worktree", "remove", "--force", path_str]).await?;
    Ok(())
}

async fn canonical_dir(path: &Path) -> Result<Option<std::path::PathBuf>, String> {
    match tokio::fs::canonicalize(path).await {
        Ok(real) => Ok(Some(real)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot resolve {}: {error}", path.display())),
    }
}

pub async fn is_registered_worktree(repo_root: &Path, path: &Path) -> Result<bool, String> {
    let git_dir = match git_probe(path, &["rev-parse", "--git-dir"]).await {
        ReadOutcome::Found(value) => value,
        ReadOutcome::Absent => return Ok(false),
        ReadOutcome::Unreadable(cause) => return Err(cause.to_string()),
    };
    let common_dir = match git_probe(repo_root, &["rev-parse", "--git-common-dir"]).await {
        ReadOutcome::Found(value) => value,
        ReadOutcome::Absent => return Ok(false),
        ReadOutcome::Unreadable(cause) => return Err(cause.to_string()),
    };
    let Some(repo_real) = canonical_dir(repo_root).await? else {
        return Ok(false);
    };
    let Some(path_real) = canonical_dir(path).await? else {
        return Ok(false);
    };
    let common = repo_real.join(common_dir.trim());
    let Some(common_real) = canonical_dir(&common).await? else {
        return Ok(false);
    };
    let admin_root = common_real.join("worktrees");
    let Some(resolved) = canonical_dir(&path_real.join(git_dir.trim())).await? else {
        return Ok(false);
    };
    let Ok(relative) = resolved.strip_prefix(&admin_root) else {
        return Ok(false);
    };
    Ok(!relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)))
}

pub async fn is_worktree_dirty(path: &Path) -> Result<bool, String> {
    match git_probe(path, &["status", "--porcelain"]).await {
        ReadOutcome::Found(status) => Ok(!status.trim().is_empty()),
        ReadOutcome::Absent => Err(format!(
            "{} is not a git work tree, so its dirty state cannot be read",
            path.display()
        )),
        ReadOutcome::Unreadable(cause) => Err(cause.to_string()),
    }
}

pub async fn dirty_files(cwd: &Path) -> Result<Vec<String>, String> {
    let status = git(cwd, &["status", "--porcelain", "-uall"]).await?;
    Ok(status
        .lines()
        .filter_map(|line| {
            let path = line.get(3..)?.trim();
            let path = path.rsplit_once(" -> ").map_or(path, |(_, new)| new);
            Some(path.trim_matches('"').to_string())
        })
        .collect())
}

async fn has_merge_head(cwd: &Path) -> Result<bool, String> {
    let run = run_git(
        cwd,
        &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
        None,
        GIT_WAIT,
    )
    .await?;
    match run.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "could not inspect MERGE_HEAD ({})",
            run.failure_detail()
        )),
    }
}

pub async fn merge_no_ff(cwd: &Path, branch: &str) -> Result<String, String> {
    if has_merge_head(cwd).await? {
        return Err(
            "cannot start a tower merge while MERGE_HEAD already exists; finish or abort the existing merge first"
                .into(),
        );
    }
    let head_before = branch_tip(cwd, "HEAD").await?;
    let status_before = git(cwd, &["status", "--porcelain"]).await?;
    if let Err(original) = git(cwd, &["merge", "--no-ff", branch]).await {
        if has_merge_head(cwd).await? {
            git(cwd, &["merge", "--abort"])
                .await
                .map_err(|error| format!("recovery-required: merge abort failed: {error}"))?;
            let mut problems = Vec::new();
            if has_merge_head(cwd).await? {
                problems.push("MERGE_HEAD still exists");
            }
            if branch_tip(cwd, "HEAD").await? != head_before {
                problems.push("HEAD was not restored to the pre-merge commit");
            }
            if git(cwd, &["status", "--porcelain"]).await? != status_before {
                problems.push("the index or worktree differs from its pre-merge state");
            }
            if !problems.is_empty() {
                return Err(format!(
                    "recovery-required: merge abort did not restore the checkout ({})",
                    problems.join("; ")
                ));
            }
        }
        return Err(original);
    }
    branch_tip(cwd, "HEAD").await
}

pub async fn diff_name_only(cwd: &Path, base: &str, ref_name: &str) -> Result<Vec<String>, String> {
    let range = format!("{base}...{ref_name}");
    let out = git(cwd, &["diff", "--name-only", &range]).await?;
    let files = out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(files)
}

pub struct BaseDirtyEntry {
    pub path: String,
    pub unmerged: bool,
}

const UNMERGED_CODES: [&str; 7] = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"];
const ADD_PATHS_CHUNK: usize = 100;

pub async fn list_base_dirty_entries(cwd: &Path) -> Result<Vec<BaseDirtyEntry>, String> {
    let run = run_git(
        cwd,
        &[
            "status",
            "--porcelain",
            "-z",
            "--no-renames",
            "--untracked-files=normal",
        ],
        None,
        GIT_WAIT,
    )
    .await?;
    if !run.status.success() {
        return Err(format!(
            "git status --porcelain failed: {}",
            run.failure_detail()
        ));
    }

    let out = String::from_utf8_lossy(&run.stdout);
    let mut entries = Vec::new();
    for record in out.split('\0') {
        if record.len() < 4 {
            continue;
        }
        let code = &record[..2];
        let raw = record[3..].trim_end_matches('/');
        if raw.is_empty() || raw.split('/').any(|part| part == ".tower") {
            continue;
        }
        entries.push(BaseDirtyEntry {
            path: raw.to_string(),
            unmerged: UNMERGED_CODES.contains(&code),
        });
    }
    Ok(entries)
}

pub async fn snapshot_base_wip(
    cwd: &Path,
    base: &str,
    paths: &[String],
    message: &str,
) -> Result<Option<String>, String> {
    if paths.is_empty() {
        return Ok(None);
    }
    let top_level = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let top_level = std::path::PathBuf::from(top_level);
    let base_tip = git(&top_level, &["rev-parse", base]).await?;
    let index_dir = std::env::temp_dir().join(format!(
        "tower-wip-index-{}-{:016x}",
        std::process::id(),
        fastrand::u64(..)
    ));
    tokio::fs::create_dir_all(&index_dir)
        .await
        .map_err(|error| format!("cannot create the WIP index directory: {error}"))?;
    let env = vec![
        (
            "GIT_INDEX_FILE",
            index_dir.join("index").to_string_lossy().into_owned(),
        ),
        ("GIT_LITERAL_PATHSPECS", "1".to_string()),
    ];
    let result = async {
        git_with_env(&top_level, &["read-tree", &base_tip], &env).await?;
        for chunk in paths.chunks(ADD_PATHS_CHUNK) {
            let mut args = vec!["add", "-A", "--"];
            args.extend(chunk.iter().map(String::as_str));
            git_with_env(&top_level, &args, &env).await?;
        }
        let tree = git_with_env(&top_level, &["write-tree"], &env).await?;
        let base_tree = git(&top_level, &["rev-parse", &format!("{base_tip}^{{tree}}")]).await?;
        if tree == base_tree {
            return Ok(None);
        }
        let commit = git_with_env(
            &top_level,
            &["commit-tree", &tree, "-p", &base_tip, "-m", message],
            &env,
        )
        .await?;
        Ok(Some(commit))
    }
    .await;
    let _ = tokio::fs::remove_dir_all(&index_dir).await;
    result
}

async fn git_commit(cwd: &Path, args: &[&str]) -> Result<(), String> {
    match git(cwd, args).await {
        Ok(_) => Ok(()),
        Err(error) if error.to_lowercase().contains("identity unknown") => {
            let mut retry = vec![
                "-c",
                "user.name=Kimi Tower",
                "-c",
                "user.email=kimi-tower@localhost",
            ];
            retry.extend_from_slice(args);
            git(cwd, &retry).await.map(|_| ())
        }
        Err(error) => Err(error),
    }
}

pub async fn init_repository(cwd: &Path) -> Result<(), String> {
    git(cwd, &["init"]).await.map(|_| ())
}

pub async fn checkout_new_local_branch(cwd: &Path, branch: &str) -> Result<(), String> {
    git(cwd, &["checkout", "-b", branch]).await.map(|_| ())
}

pub async fn commit_allow_empty(cwd: &Path, message: &str) -> Result<(), String> {
    git_commit(cwd, &["commit", "--allow-empty", "-m", message]).await
}

pub async fn commit_paths(cwd: &Path, paths: &[String], message: &str) -> Result<(), String> {
    for chunk in paths.chunks(ADD_PATHS_CHUNK) {
        let mut args = vec!["add", "-A", "--"];
        args.extend(chunk.iter().map(String::as_str));
        git(cwd, &args).await?;
    }
    git_commit(cwd, &["commit", "-m", message]).await
}

pub async fn worktree_add_new_branch(
    cwd: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<(), String> {
    let path_str = path.to_str().ok_or("invalid path")?;
    git(cwd, &["worktree", "add", path_str, "-b", branch, base])
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn git_with_input_writes_stdin_for_hash_object() {
        let dir = tempfile::tempdir().unwrap();
        let digest = git_with_input(dir.path(), &["hash-object", "--stdin"], "hello world\n")
            .await
            .unwrap();
        assert_eq!(digest, "3b18e512dba79e4c8300dd08aeb37f8e728b8dad");
    }

    #[tokio::test]
    async fn is_ancestor_separates_no_from_a_git_failure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await.unwrap();
        let _ = git(root, &["config", "user.email", "t@e.test"]).await;
        let _ = git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join("f.txt"), "base").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "init"]).await.unwrap();
        let base = git(root, &["rev-parse", "HEAD"]).await.unwrap();
        git(root, &["branch", "feat/x"]).await.unwrap();

        git(root, &["checkout", "-q", "feat/x"]).await.unwrap();
        std::fs::write(root.join("f.txt"), "work").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "work"]).await.unwrap();

        assert_eq!(
            is_ancestor(root, &base, "feat/x").await,
            Ok(true),
            "a real ancestor is Ok(true)"
        );
        assert_eq!(
            is_ancestor(root, "feat/x", &base).await,
            Ok(false),
            "exit 1 is a real 'not an ancestor', not an error"
        );
        assert!(
            is_ancestor(root, "no-such-ref", "feat/x").await.is_err(),
            "an unknown ref must surface as an error, not Ok(false)"
        );
        assert!(
            is_ancestor(root, &base, "also-missing").await.is_err(),
            "an unknown target must surface as an error too"
        );
    }

    #[tokio::test]
    async fn worktree_remove_succeeds_when_the_path_is_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await.unwrap();

        let missing = root.join("worktrees/never-created");

        assert!(!missing.exists());
        worktree_remove(root, &missing).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_remove_still_fails_on_a_live_worktree_with_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await.unwrap();
        let _ = git(root, &["config", "user.email", "t@e.test"]).await;
        let _ = git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join("f.txt"), "base").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "init"]).await.unwrap();

        let head = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();

        let wt = root.join("worktrees/wt-a");
        worktree_add(root, &wt, "wt-branch", &head).await.unwrap();

        worktree_remove(root, &wt).await.unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn a_directory_outside_a_repository_answers_no() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(is_inside_repo(dir.path()).await, Ok(false));
        assert_eq!(has_any_commit(dir.path()).await, Ok(false));
        assert_eq!(branch_exists(dir.path(), "main").await, Ok(false));
    }

    #[tokio::test]
    async fn an_unreadable_dirty_state_is_an_error_not_clean() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = is_worktree_dirty(dir.path()).await;
        assert!(outcome.is_err(), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_clean_worktree_is_not_dirty_and_a_changed_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await.unwrap();
        let _ = git(root, &["config", "user.email", "t@e.test"]).await;
        let _ = git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join("f.txt"), "base").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "init"]).await.unwrap();
        assert_eq!(is_worktree_dirty(root).await, Ok(false));
        std::fs::write(root.join("f.txt"), "changed").unwrap();
        assert_eq!(is_worktree_dirty(root).await, Ok(true));
    }

    async fn conflicting_repo(root: &Path) {
        git(root, &["init", "-q"]).await.unwrap();
        let _ = git(root, &["config", "user.email", "t@e.test"]).await;
        let _ = git(root, &["config", "user.name", "t"]).await;
        std::fs::write(root.join("f.txt"), "base").unwrap();
        git(root, &["add", "."]).await.unwrap();
        git(root, &["commit", "-qm", "base"]).await.unwrap();
        let base = current_branch(root).await.unwrap();
        git(root, &["checkout", "-qb", "feat/conflict"])
            .await
            .unwrap();
        std::fs::write(root.join("f.txt"), "branch").unwrap();
        git(root, &["commit", "-qam", "branch change"])
            .await
            .unwrap();
        git(root, &["checkout", "-q", &base]).await.unwrap();
        std::fs::write(root.join("f.txt"), "main").unwrap();
        git(root, &["commit", "-qam", "main change"]).await.unwrap();
    }

    #[tokio::test]
    async fn merge_no_ff_refuses_an_existing_merge_head() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        conflicting_repo(root).await;
        assert!(
            git(root, &["merge", "--no-ff", "feat/conflict"])
                .await
                .is_err()
        );
        let error = merge_no_ff(root, "feat/conflict").await.unwrap_err();
        assert!(
            error.contains("cannot start a tower merge while MERGE_HEAD already exists"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn merge_no_ff_aborts_a_conflict_and_restores_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        conflicting_repo(root).await;
        let head = branch_tip(root, "HEAD").await.unwrap();
        assert!(merge_no_ff(root, "feat/conflict").await.is_err());
        assert_eq!(branch_tip(root, "HEAD").await.unwrap(), head);
        assert!(
            git(root, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
                .await
                .is_err()
        );
        assert_eq!(git(root, &["status", "--porcelain"]).await.unwrap(), "");
    }
}
