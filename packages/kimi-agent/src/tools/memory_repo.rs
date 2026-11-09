use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use regex::Regex;

const MEMORY_AUTHOR_NAME: &str = "kimi-code memory";
const MEMORY_AUTHOR_EMAIL: &str = "memory@local";

const GIT_MEMORY_ENV: &str = "KIMI_CODE_EXPERIMENTAL_GIT_MEMORY";

fn run_git_command(
    mut command: Command,
    label: &str,
) -> Option<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match command.output() {
        Ok(output) => Some((output.status, output.stdout, output.stderr)),
        Err(error) => {
            tracing::warn!(command = label, %error, "memory git command could not start");
            None
        }
    }
}

pub fn git_memory_enabled() -> bool {
    crate::env::env_switch_default_off(GIT_MEMORY_ENV)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCommit {
    pub sha: String,
    pub date: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct MemoryRepo {
    dir: PathBuf,
}

impl MemoryRepo {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn run(&self, args: &[&str]) -> Option<String> {
        let label = format!("git {}", args.join(" "));
        let mut command = Command::new("git");
        command.args(args).current_dir(&self.dir);
        let (status, stdout, stderr) = run_git_command(command, &label)?;
        if !status.success() {
            tracing::warn!(
                command = %label,
                code = ?status.code(),
                stderr = %String::from_utf8_lossy(&stderr).trim(),
                "memory git command failed"
            );
            return None;
        }
        Some(String::from_utf8_lossy(&stdout).into_owned())
    }

    pub fn ensure_repo(&self) -> bool {
        if self.dir.join(".git").is_dir() {
            return true;
        }
        if std::fs::create_dir_all(&self.dir).is_err() {
            return false;
        }
        self.run(&["init"]).is_some()
    }

    pub fn commit(&self, message: &str) -> bool {
        if !self.ensure_repo() {
            return false;
        }
        if self.run(&["add", "-A"]).is_none() {
            return false;
        }
        self.run(&[
            "-c",
            &format!("user.name={MEMORY_AUTHOR_NAME}"),
            "-c",
            &format!("user.email={MEMORY_AUTHOR_EMAIL}"),
            "commit",
            "-m",
            message,
        ])
        .is_some()
    }

    pub fn log_commits(&self, limit: usize) -> Vec<MemoryCommit> {
        let spec = format!("-{limit}");
        let Some(stdout) = self.run(&["log", &spec, "--date=short", "--format=%H%x09%ad%x09%s"])
        else {
            return Vec::new();
        };
        stdout
            .lines()
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                let sha = parts.next()?.trim().to_string();
                let date = parts.next()?.trim().to_string();
                let message = parts.next().unwrap_or("").trim().to_string();
                Some(MemoryCommit { sha, date, message })
            })
            .collect()
    }

    pub fn revert(&self, sha: &str) -> bool {
        if !self.ensure_repo() {
            return false;
        }
        self.run(&[
            "-c",
            &format!("user.name={MEMORY_AUTHOR_NAME}"),
            "-c",
            &format!("user.email={MEMORY_AUTHOR_EMAIL}"),
            "revert",
            "--no-edit",
            sha,
        ])
        .is_some()
    }
}

pub fn has_git_reference(workspace_root: &Path, memory_dir: &Path, content: &str) -> bool {
    static REF_RE: OnceLock<Regex> = OnceLock::new();

    let re =
        REF_RE.get_or_init(|| Regex::new(r"(?-u)\b[0-9a-f]{7,40}\b").expect("static ref regex"));
    let mut seen = std::collections::HashSet::new();
    let candidates: Vec<&str> = re
        .find_iter(content)
        .map(|m| m.as_str())
        .filter(|candidate| seen.insert(*candidate))
        .take(8)
        .collect();
    candidates
        .iter()
        .any(|sha| commit_exists(workspace_root, sha) || commit_exists(memory_dir, sha))
}

fn commit_exists(repo: &Path, sha: &str) -> bool {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .current_dir(repo);
    run_git_command(command, "git cat-file -e")
        .map(|(status, _, _)| status.success())
        .unwrap_or(false)
}

pub fn lock_path(memory_dir: &Path) -> Option<PathBuf> {
    let home = memory_dir.parent()?;
    Some(home.join("locks").join("memory-write.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    #[test]
    fn commit_log_and_revert_round_trip() {
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

        let commits = repo.log_commits(20);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].message, "update: Note");
        assert_eq!(commits[1].message, "remember: Note");
        assert_eq!(commits[0].sha.len(), 40);
        assert_eq!(commits[0].date.len(), 10);

        assert!(repo.revert(&commits[0].sha));
        assert_eq!(
            std::fs::read_to_string(&note)
                .unwrap()
                .replace("\r\n", "\n"),
            "# Note\n\nfirst\n"
        );
    }

    #[test]
    fn an_empty_repository_has_no_commits_and_revert_fails() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::new(dir.path().to_path_buf());
        assert!(repo.log_commits(20).is_empty());
        assert!(!repo.revert("0000000000000000000000000000000000000000"));
    }

    #[test]
    fn has_git_reference_accepts_a_full_sha_from_the_memory_repo() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join("global")).unwrap();
        std::fs::write(dir.path().join("global/note.md"), "# Note\n").unwrap();
        assert!(repo.commit("remember: Note"));
        let sha = repo.log_commits(1)[0].sha.clone();

        let workspace = tempfile::tempdir().unwrap();
        assert!(has_git_reference(
            workspace.path(),
            dir.path(),
            &format!("see {sha} for details")
        ));
        assert!(!has_git_reference(
            workspace.path(),
            dir.path(),
            "no reference here"
        ));
    }

    #[test]
    fn lock_path_lives_next_to_the_memory_directory() {
        assert_eq!(
            lock_path(Path::new("/home/u/.kimi-code/memory")).unwrap(),
            Path::new("/home/u/.kimi-code/locks/memory-write.lock")
        );
    }
}
