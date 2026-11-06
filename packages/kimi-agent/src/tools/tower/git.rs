use std::path::{Component, Path};
use tokio::process::Command;

pub async fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    git_impl(cwd, args, None).await
}

pub async fn git_with_input(cwd: &Path, args: &[&str], input: &str) -> Result<String, String> {
    git_impl(cwd, args, Some(input)).await
}

async fn git_impl(cwd: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

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
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("failed to spawn git: {e}"))?;
    if let Some(input) = input {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open git stdin".to_string())?;
        if let Err(error) = stdin.write_all(input.as_bytes()).await {
            let _ = child.kill().await;
            return Err(format!("failed to write git stdin: {error}"));
        }
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("failed to wait for git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            "unknown git error".to_string()
        } else {
            stderr
        };
        return Err(format!("git {} failed: {msg}", args.join(" ")));
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

pub async fn try_git(cwd: &Path, args: &[&str]) -> Option<String> {
    git(cwd, args).await.ok()
}

pub async fn is_inside_repo(cwd: &Path) -> bool {
    try_git(cwd, &["rev-parse", "--is-inside-work-tree"])
        .await
        .as_deref()
        == Some("true")
}

pub async fn has_any_commit(cwd: &Path) -> bool {
    try_git(cwd, &["rev-list", "-n", "1", "--all"])
        .await
        .is_some()
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

pub async fn branch_exists(cwd: &Path, branch: &str) -> bool {
    let ref_name = format!("refs/heads/{branch}");
    try_git(cwd, &["show-ref", "--verify", "--quiet", &ref_name])
        .await
        .is_some()
}

/// Whether `maybe_ancestor` is an ancestor of `ref_name` (v2 `isAncestor`):
/// `git merge-base --is-ancestor A B` exits 0 when it is, 1 when it is not.
///
/// A third outcome exists — any other non-zero exit (unknown ref, shallow
/// clone missing the object, unreadable repo) plus a failure to spawn git — and
/// it is NOT "not an ancestor". The completion gate picks its diff base from
/// this answer, so folding an error into `false` would silently re-base the
/// check onto the tower base. Errors propagate instead, and the caller refuses.
pub async fn is_ancestor(cwd: &Path, maybe_ancestor: &str, ref_name: &str) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["merge-base", "--is-ancestor", maybe_ancestor, ref_name])
        .current_dir(cwd)
        .output()
        .await
        .map_err(|error| {
            format!("git merge-base --is-ancestor {maybe_ancestor} {ref_name} failed: {error}")
        })?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(format!(
                "git merge-base --is-ancestor {maybe_ancestor} {ref_name} could not decide (status {:?}): {detail}",
                output.status.code()
            ))
        }
    }
}

pub async fn worktree_add(cwd: &Path, path: &Path, branch: &str, base: &str) -> Result<(), String> {
    let path_str = path.to_str().ok_or("invalid path")?;
    if branch_exists(cwd, branch).await {
        git(cwd, &["worktree", "add", path_str, branch]).await?;
    } else {
        git(cwd, &["worktree", "add", path_str, "-b", branch, base]).await?;
    }
    Ok(())
}

pub async fn worktree_remove(cwd: &Path, path: &Path) -> Result<(), String> {
    let path_str = path.to_str().ok_or("invalid path")?;
    // Idempotent teardown (v2 #3648): a worktree git no longer knows about is
    // reported as already removed instead of failing the whole teardown.
    if !path.exists() {
        let prunable = try_git(cwd, &["worktree", "list", "--porcelain"]).await;
        let known = prunable.as_deref().is_some_and(|out| {
            out.split('\n').any(|line| {
                line.strip_prefix("worktree ")
                    .is_some_and(|wt| wt.trim() == path_str)
            })
        });
        if !known {
            return Ok(());
        }
    }
    git(cwd, &["worktree", "remove", "--force", path_str]).await?;
    Ok(())
}

pub async fn is_registered_worktree(repo_root: &Path, path: &Path) -> bool {
    let Some(git_dir) = try_git(path, &["rev-parse", "--git-dir"]).await else {
        return false;
    };
    let Some(common_dir) = try_git(repo_root, &["rev-parse", "--git-common-dir"]).await else {
        return false;
    };
    let Ok(repo_real) = tokio::fs::canonicalize(repo_root).await else {
        return false;
    };
    let Ok(path_real) = tokio::fs::canonicalize(path).await else {
        return false;
    };
    let common = repo_real.join(common_dir.trim());
    let Ok(common_real) = tokio::fs::canonicalize(common).await else {
        return false;
    };
    let admin_root = common_real.join("worktrees");
    let resolved = path_real.join(git_dir.trim());
    let Ok(resolved) = tokio::fs::canonicalize(resolved).await else {
        return false;
    };
    let Ok(relative) = resolved.strip_prefix(&admin_root) else {
        return false;
    };
    !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

pub async fn is_worktree_dirty(path: &Path) -> bool {
    let status = try_git(path, &["status", "--porcelain"]).await;
    status.is_some_and(|s| !s.trim().is_empty())
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
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", "MERGE_HEAD"])
        .current_dir(cwd)
        .output()
        .await
        .map_err(|error| format!("failed to inspect MERGE_HEAD: {error}"))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(format!(
            "could not inspect MERGE_HEAD (status {code:?}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
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

    /// The three outcomes of `merge-base --is-ancestor` are three different
    /// answers. Exit 1 is a real "no"; anything else is a git failure the
    /// completion gate must not read as "no" — it would silently judge the
    /// mission's diff against the tower base instead of its spawn base.
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
        // Advance the branch so the base is a strict ancestor.
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

    /// Idempotent teardown (v2 #3648): removing a worktree path git no longer
    /// knows about succeeds instead of failing the whole teardown.
    #[tokio::test]
    async fn worktree_remove_succeeds_when_the_path_is_already_gone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await.unwrap();

        let missing = root.join("worktrees/never-created");
        // The path does not exist and git knows no such worktree: the remove
        // is a no-op success.
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
        // The default branch may be master or main depending on git config;
        // resolve it instead of assuming.
        let head = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap();

        let wt = root.join("worktrees/wt-a");
        worktree_add(root, &wt, "wt-branch", &head).await.unwrap();
        // A live, clean worktree removes fine.
        worktree_remove(root, &wt).await.unwrap();
        assert!(!wt.exists());
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
