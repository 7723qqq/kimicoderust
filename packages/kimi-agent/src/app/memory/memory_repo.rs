use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::wire::record::WireAsyncError;

use super::memory_paths::memory_dir;

pub const MEMORY_AUTHOR_NAME: &str = "kimi-code memory";
pub const MEMORY_AUTHOR_EMAIL: &str = "memory@local";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryCommit {
    pub sha: String,
    pub date: String,
    pub message: String,
}

#[async_trait]
pub trait IMemoryRepo: Send + Sync {
    async fn commit(&self, message: &str) -> Result<bool, WireAsyncError>;
    async fn log_commits(&self, limit: usize) -> Result<Vec<MemoryCommit>, WireAsyncError>;
    async fn recent_commit_shas(&self, limit: usize) -> Result<HashMap<String, String>, WireAsyncError>;
    async fn revert(&self, sha: &str) -> Result<bool, WireAsyncError>;
}

pub struct MemoryRepo {
    dir: PathBuf,
    queue: Arc<tokio::sync::Mutex<()>>,
    initialized: AtomicBool,
}

struct GitOutput {
    ok: bool,
    stdout: String,
}

impl MemoryRepo {
    pub fn new(home_dir: PathBuf) -> Self {
        Self {
            dir: memory_dir(&home_dir),
            queue: Arc::new(tokio::sync::Mutex::new(())),
            initialized: AtomicBool::new(false),
        }
    }

    async fn run(&self, args: &[String]) -> GitOutput {
        let result = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        match result {
            Ok(output) => GitOutput {
                ok: output.status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            },
            Err(error) => {
                tracing::warn!(%error, dir = %self.dir.display(), "memory repo command failed");
                GitOutput {
                    ok: false,
                    stdout: String::new(),
                }
            }
        }
    }

    async fn ensure_repo(&self) -> bool {
        if self.initialized.load(Ordering::Acquire) {
            return true;
        }
        if let Err(error) = tokio::fs::create_dir_all(&self.dir).await {
            tracing::warn!(%error, dir = %self.dir.display(), "memory repo directory could not be created");
            return false;
        }
        if !self.run(&["init".to_string()]).await.ok {
            tracing::warn!(dir = %self.dir.display(), "memory repo could not be initialised");
            return false;
        }
        self.initialized.store(true, Ordering::Release);
        true
    }

    fn author_args() -> Vec<String> {
        vec![
            "-c".to_string(),
            format!("user.name={MEMORY_AUTHOR_NAME}"),
            "-c".to_string(),
            format!("user.email={MEMORY_AUTHOR_EMAIL}"),
        ]
    }
}

#[async_trait]
impl IMemoryRepo for MemoryRepo {
    async fn commit(&self, message: &str) -> Result<bool, WireAsyncError> {
        let _guard = self.queue.lock().await;
        if !self.ensure_repo().await {
            return Ok(false);
        }
        if !self.run(&["add".to_string(), "-A".to_string()]).await.ok {
            tracing::warn!(dir = %self.dir.display(), "memory repo staging failed");
            return Ok(false);
        }
        let mut args = Self::author_args();
        args.extend(["commit".to_string(), "-m".to_string(), message.to_string()]);
        if !self.run(&args).await.ok {
            tracing::warn!(dir = %self.dir.display(), message, "memory repo commit failed");
            return Ok(false);
        }
        Ok(true)
    }

    async fn log_commits(&self, limit: usize) -> Result<Vec<MemoryCommit>, WireAsyncError> {
        let result = self.run(&["log".to_string(), format!("-{limit}"), "--date=short".to_string(), "--format=%H%x09%ad%x09%s".to_string()]).await;
        if !result.ok {
            return Ok(Vec::new());
        }
        let mut commits = Vec::new();
        for line in result.stdout.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            let parts = line.split('\t').collect::<Vec<_>>();
            let sha = parts.first().copied().unwrap_or("").trim();
            if sha.is_empty() {
                continue;
            }
            commits.push(MemoryCommit {
                sha: sha.to_string(),
                date: parts.get(1).copied().unwrap_or("").trim().to_string(),
                message: parts.get(2..).unwrap_or(&[]).join("\t").trim().to_string(),
            });
        }
        Ok(commits)
    }

    async fn recent_commit_shas(&self, limit: usize) -> Result<HashMap<String, String>, WireAsyncError> {
        let result = self.run(&[
            "log".to_string(),
            "-n".to_string(),
            limit.to_string(),
            "--format=%x00%H".to_string(),
            "--name-only".to_string(),
            "--no-renames".to_string(),
        ]).await;
        let mut shas = HashMap::new();
        if !result.ok {
            return Ok(shas);
        }
        let mut current: Option<String> = None;
        for raw_line in result.stdout.split('\n') {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(sha) = line.strip_prefix('\0') {
                current = Some(sha.to_string());
                continue;
            }
            let Some(current) = &current else {
                continue;
            };
            shas.entry(line.to_string()).or_insert_with(|| current.clone());
        }
        Ok(shas)
    }

    async fn revert(&self, sha: &str) -> Result<bool, WireAsyncError> {
        let _guard = self.queue.lock().await;
        if !self.ensure_repo().await {
            return Ok(false);
        }
        let mut args = Self::author_args();
        args.extend(["revert".to_string(), "--no-edit".to_string(), sha.to_string()]);
        if !self.run(&args).await.ok {
            tracing::warn!(dir = %self.dir.display(), sha, "memory repo revert failed");
            return Ok(false);
        }
        Ok(true)
    }
}
