use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::fs;
use tokio::time::sleep;

use super::{INBOX_DIR, STATE_FILE, TowerStore};
use crate::tools::tower::git::{git, git_with_input, try_git};

pub(crate) struct ProtocolLockGuard {
    path: PathBuf,
    token: String,
}

impl Drop for ProtocolLockGuard {
    fn drop(&mut self) {
        let Ok(holder) = std::fs::read_to_string(&self.path) else {
            return;
        };
        if holder.contains(&format!("token={}", self.token)) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy)]
enum ProtocolLockKind {
    State,
    InboxPublication,
}

impl ProtocolLockKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::InboxPublication => "inbox publication",
        }
    }

    fn explanation(self, same_process: bool) -> &'static str {
        match (self, same_process) {
            (Self::State, true) => {
                "another tower tool call in this process is writing state.json; wait for it to finish"
            }
            (Self::InboxPublication, true) => {
                "another tower tool call in this process is publishing an inbox message; wait for it to finish"
            }
            (Self::State, false) => {
                "another tower process may be writing state.json; a confirmed dead holder is reclaimed automatically, so inspect the holder if this persists"
            }
            (Self::InboxPublication, false) => {
                "another tower process may be publishing an inbox message; a confirmed dead holder is reclaimed automatically, so inspect the holder if this persists"
            }
        }
    }
}

struct StateLockReaperGuard {
    ref_name: String,
    token: String,
}

impl TowerStore {
    pub(crate) async fn with_state_lock<T, F, Fut>(&self, operation: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        self.with_protocol_lock(
            self.abs(&format!("{STATE_FILE}.lock")),
            ProtocolLockKind::State,
            operation,
        )
        .await
    }

    pub(crate) async fn with_inbox_publication_lock<T, F, Fut>(
        &self,
        operation: F,
    ) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        self.with_protocol_lock(
            self.abs(&format!("{INBOX_DIR}.lock")),
            ProtocolLockKind::InboxPublication,
            operation,
        )
        .await
    }

    pub(crate) async fn with_completion_lock<T, F, Fut>(&self, operation: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        self.with_state_lock(|| self.with_inbox_publication_lock(operation))
            .await
    }

    async fn with_protocol_lock<T, F, Fut>(
        &self,
        lock_path: PathBuf,
        kind: ProtocolLockKind,
        operation: F,
    ) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let deadline = Instant::now() + Duration::from_millis(self.state_lock_timeout_ms);
        let token = format!("{:016x}", fastrand::u64(..));
        let mut last_stale_check = None;
        let guard = loop {
            if self.try_acquire_state_lock(&lock_path, &token).await? {
                break ProtocolLockGuard {
                    path: lock_path.clone(),
                    token: token.clone(),
                };
            }
            let now = Instant::now();
            if last_stale_check
                .is_none_or(|last: Instant| now.duration_since(last) >= Duration::from_millis(250))
            {
                last_stale_check = Some(now);
                if self.reap_stale_state_lock(&lock_path).await? {
                    continue;
                }
            }
            if now >= deadline {
                let holder = fs::read_to_string(&lock_path).await.unwrap_or_default();
                let same_process = lock_pid(&holder) == Some(std::process::id());
                return Err(format!(
                    "timed out after {}ms waiting for the tower {} lock \"{}\" — held by {} — {}",
                    self.state_lock_timeout_ms,
                    kind.as_str(),
                    lock_path.display(),
                    holder.trim(),
                    kind.explanation(same_process),
                ));
            }
            sleep(Duration::from_millis(self.state_lock_poll_ms)).await;
        };
        let result = operation().await;
        drop(guard);
        result
    }

    async fn try_acquire_state_lock(&self, path: &Path, token: &str) -> Result<bool, String> {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt;
                let body = format!(
                    "pid={} since={} token={token}",
                    std::process::id(),
                    chrono::Utc::now().to_rfc3339(),
                );
                if let Err(error) = file.write_all(body.as_bytes()).await {
                    let _ = fs::remove_file(path).await;
                    return Err(error.to_string());
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err("tower is not initialized in this repository — run TowerInit first".into())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub(crate) async fn state_lock_reaper_ref(&self, path: &Path) -> Result<String, String> {
        let canonical = match fs::canonicalize(path.parent().unwrap_or(&self.repo_root)).await {
            Ok(parent) => parent.join(path.file_name().unwrap_or_default()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let root = fs::canonicalize(&self.repo_root)
                    .await
                    .map_err(|error| error.to_string())?;
                root.join(path.strip_prefix(&self.repo_root).unwrap_or(path))
            }
            Err(error) => return Err(error.to_string()),
        };
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        Ok(format!("refs/tower/state-lock-reaper-{digest:x}"))
    }

    async fn read_state_lock_reaper(&self, ref_name: &str) -> Option<String> {
        try_git(
            &self.repo_root,
            &["rev-parse", "--verify", "--quiet", ref_name],
        )
        .await
    }

    async fn acquire_state_lock_reaper(
        &self,
        lock_path: &Path,
    ) -> Result<Option<StateLockReaperGuard>, String> {
        let ref_name = self.state_lock_reaper_ref(lock_path).await?;
        let current = self.read_state_lock_reaper(&ref_name).await;
        if let Some(ref current_token) = current {
            let holder = git(&self.repo_root, &["cat-file", "-p", current_token]).await?;
            if lock_pid(&holder).is_some_and(pid_alive) {
                return Ok(None);
            }
        }
        let token = git_with_input(
            &self.repo_root,
            &["hash-object", "-w", "--stdin"],
            &format!(
                "pid={} since={} token={:016x}",
                std::process::id(),
                chrono::Utc::now().to_rfc3339(),
                fastrand::u64(..)
            ),
        )
        .await?;
        let expected = current.as_deref().unwrap_or("");
        match git(
            &self.repo_root,
            &["update-ref", &ref_name, &token, expected],
        )
        .await
        {
            Ok(_) => Ok(Some(StateLockReaperGuard { ref_name, token })),
            Err(error) => {
                let after = self.read_state_lock_reaper(&ref_name).await;
                if after.as_deref() == Some(token.as_str()) {
                    return Ok(Some(StateLockReaperGuard { ref_name, token }));
                }
                if after != current
                    || error.contains("reference already exists")
                    || error.contains("but expected")
                {
                    return Ok(None);
                }
                Err(error)
            }
        }
    }

    async fn release_state_lock_reaper(&self, guard: StateLockReaperGuard) -> Result<(), String> {
        match git(
            &self.repo_root,
            &["update-ref", "-d", &guard.ref_name, &guard.token],
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                let current = self.read_state_lock_reaper(&guard.ref_name).await;
                if current.as_deref() != Some(guard.token.as_str()) {
                    return Ok(());
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn reap_stale_state_lock(&self, path: &Path) -> Result<bool, String> {
        let Some(guard) = self.acquire_state_lock_reaper(path).await? else {
            return Ok(false);
        };
        let result = self.reap_stale_state_lock_guarded(path).await;
        let released = self.release_state_lock_reaper(guard).await;
        match (result, released) {
            (Ok(reaped), Ok(())) => Ok(reaped),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    async fn reap_stale_state_lock_guarded(&self, path: &Path) -> Result<bool, String> {
        let Ok(content) = fs::read_to_string(path).await else {
            return Ok(false);
        };
        let Some(pid) = lock_pid(&content) else {
            return Ok(false);
        };
        if pid_alive(pid) {
            return Ok(false);
        }
        let Ok(confirmed) = fs::read_to_string(path).await else {
            return Ok(false);
        };
        if confirmed != content || pid_alive(pid) {
            return Ok(false);
        }
        match fs::remove_file(path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn lock_pid(content: &str) -> Option<u32> {
    content
        .split_whitespace()
        .find_map(|part| part.strip_prefix("pid="))?
        .parse()
        .ok()
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let alive = unsafe { libc::kill(pid as i32, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return false;
    }
    match std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "stat="])
        .output()
    {
        Ok(out) => !String::from_utf8_lossy(&out.stdout)
            .trim_start()
            .starts_with('Z'),
        Err(_) => true,
    }
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const ERROR_ACCESS_DENIED: u32 = 5;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle != 0 {
            CloseHandle(handle);
            true
        } else {
            GetLastError() == ERROR_ACCESS_DENIED
        }
    }
}
