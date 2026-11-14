use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::Notify;

use crate::_base::log::log::ILogService;
use crate::os::interface::host_process::IHostProcessService;
use crate::wire::record::WireAsyncError;

use super::cache::{
    IndexReadResult, build_index, cache_path, read_any_index, read_index, remove_legacy_index,
    write_index,
};
use super::gitReader::{
    GitReadResult, GitUnavailableReason, RepoHead, RepoHeadResult, read_git_history,
    read_repo_head, read_repo_tree,
};
use super::types::{GIT_MEMORY_WINDOW, GitMemoryIndex};

const TRANSIENT_FAILURE_COOLDOWN_MS: u64 = 10_000;
const DETERMINISTIC_HEAD_FAILURE_TTL_MS: u64 = 60_000;

#[derive(Clone)]
pub struct GitMemoryIndexLoaderDeps {
    pub process: Arc<dyn IHostProcessService>,
    pub store_dir: PathBuf,
    pub cwd: String,
    pub log: Arc<dyn ILogService>,
    pub now: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub on_index_changed: Arc<dyn Fn() + Send + Sync>,
    pub on_head_unavailable: Arc<dyn Fn(GitUnavailableReason) + Send + Sync>,
}

#[derive(Clone)]
struct HeadFailure {
    reason: GitUnavailableReason,
    at: u64,
}

#[derive(Default)]
struct LoaderState {
    index: Option<GitMemoryIndex>,
    index_loaded: bool,
    index_failure_at: Option<u64>,
    loading: bool,
    refresh: bool,
    head_value: Option<RepoHead>,
    head_loaded: bool,
    head_failure: Option<HeadFailure>,
    head_loading: bool,
    root_value: String,
    prefix_value: String,
    cleaned_legacy_root: String,
}

pub struct GitMemoryIndexLoader {
    deps: GitMemoryIndexLoaderDeps,
    state: Mutex<LoaderState>,
    index_notify: Notify,
    head_notify: Notify,
}

impl GitMemoryIndexLoader {
    pub fn new(deps: GitMemoryIndexLoaderDeps) -> Arc<Self> {
        Arc::new(Self {
            deps,
            state: Mutex::new(LoaderState::default()),
            index_notify: Notify::new(),
            head_notify: Notify::new(),
        })
    }

    pub fn repo_root(&self) -> String {
        self.state.lock().root_value.clone()
    }

    pub fn path_prefix(&self) -> String {
        self.state.lock().prefix_value.clone()
    }

    pub async fn ensure_index(self: &Arc<Self>) -> Option<GitMemoryIndex> {
        loop {
            let mut state = self.state.lock();
            if state.index_loaded {
                return state.index.clone();
            }
            if state.index_failure_at.is_some_and(|at| {
                (self.deps.now)().saturating_sub(at) < TRANSIENT_FAILURE_COOLDOWN_MS
            }) {
                return None;
            }
            if state.loading {
                let notified = self.index_notify.notified();
                drop(state);
                notified.await;
                return self.state.lock().index.clone();
            }
            state.loading = true;
            drop(state);

            let result = self.load_index().await;
            let mut state = self.state.lock();
            state.loading = false;
            let mut notify_index_changed = false;
            match result {
                Ok(Some(index)) => {
                    state.index = Some(index);
                    notify_index_changed = true;
                }
                Ok(None) => {
                    if state.head_value.is_some() {
                        state.index_failure_at = Some((self.deps.now)());
                    }
                }
                Err(error) => {
                    self.deps.log.warn(
                        "git memory index load failed",
                        Some(json!({ "error": error.to_string() })),
                    );
                    if state.head_value.is_some() {
                        state.index_failure_at = Some((self.deps.now)());
                    }
                }
            }
            drop(state);
            if notify_index_changed {
                let changed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (self.deps.on_index_changed)();
                }));
                let mut state = self.state.lock();
                match changed {
                    Ok(()) => {
                        state.index_loaded = true;
                        state.index_failure_at = None;
                    }
                    Err(error) => {
                        self.deps.log.warn(
                            "git memory index load failed",
                            Some(json!({ "error": panic_message(error) })),
                        );
                        if state.head_value.is_some() {
                            state.index_failure_at = Some((self.deps.now)());
                        }
                    }
                }
            }
            let index = self.state.lock().index.clone();
            self.index_notify.notify_waiters();
            return index;
        }
    }

    pub async fn ensure_head(self: &Arc<Self>) -> Option<RepoHead> {
        loop {
            let mut state = self.state.lock();
            if state.head_loaded {
                return state.head_value.clone();
            }
            if let Some(failure) = &state.head_failure {
                let cooldown = if failure.reason == GitUnavailableReason::Timeout {
                    TRANSIENT_FAILURE_COOLDOWN_MS
                } else {
                    DETERMINISTIC_HEAD_FAILURE_TTL_MS
                };
                if (self.deps.now)().saturating_sub(failure.at) < cooldown {
                    return None;
                }
            }
            if state.head_loading {
                let notified = self.head_notify.notified();
                drop(state);
                notified.await;
                continue;
            }
            state.head_loading = true;
            drop(state);

            let result = self.load_head().await;
            let mut state = self.state.lock();
            state.head_loading = false;
            match result {
                Ok(RepoHeadResult::Ok(head)) => {
                    state.head_value = Some(head);
                    state.head_loaded = true;
                    state.head_failure = None;
                }
                Ok(RepoHeadResult::Unavailable { reason, detail }) => {
                    state.head_failure = Some(HeadFailure {
                        reason,
                        at: (self.deps.now)(),
                    });
                    self.deps.log.warn(
                        "git memory head lookup failed",
                        Some(json!({
                            "cwd": self.deps.cwd,
                            "reason": unavailable_reason(reason),
                            "detail": detail,
                        })),
                    );
                    drop(state);
                    (self.deps.on_head_unavailable)(reason);
                    state = self.state.lock();
                }
                Err(error) => {
                    let detail = error.to_string();
                    state.head_failure = Some(HeadFailure {
                        reason: GitUnavailableReason::Timeout,
                        at: (self.deps.now)(),
                    });
                    self.deps.log.warn(
                        "git memory head lookup failed",
                        Some(json!({
                            "cwd": self.deps.cwd,
                            "reason": "timeout",
                            "detail": detail,
                        })),
                    );
                    drop(state);
                    (self.deps.on_head_unavailable)(GitUnavailableReason::Timeout);
                    state = self.state.lock();
                }
            }
            let head = state.head_value.clone();
            drop(state);
            self.head_notify.notify_waiters();
            return head;
        }
    }

    pub async fn load_index(self: &Arc<Self>) -> Result<Option<GitMemoryIndex>, WireAsyncError> {
        let Some(head) = self.ensure_head().await else {
            return Ok(None);
        };
        self.clean_legacy_index(&head.repo_root).await;
        let path = cache_path(&self.deps.store_dir, &head.repo_root);
        match read_index(&path, &head.head_sha, GIT_MEMORY_WINDOW).await {
            IndexReadResult::Hit(index) => Ok(Some(index)),
            IndexReadResult::Miss => {
                let stale = read_any_index(&path).await;
                if let Some(stale) = stale {
                    self.schedule_refresh(head, path);
                    return Ok(Some(stale));
                }
                self.rebuild(head, path).await
            }
            IndexReadResult::Corrupt => {
                self.deps.log.warn(
                    "git memory cache was corrupted; rebuilding it",
                    None,
                );
                self.rebuild(head, path).await
            }
        }
    }

    pub async fn load_head(&self) -> Result<RepoHeadResult, WireAsyncError> {
        let result = read_repo_head(&self.deps.process, &self.deps.cwd, None).await;
        if let RepoHeadResult::Ok(head) = &result {
            let mut state = self.state.lock();
            state.root_value = head.repo_root.clone();
            state.prefix_value = head.prefix.clone();
        }
        Ok(result)
    }

    async fn clean_legacy_index(&self, repo_root: &str) {
        if self.state.lock().cleaned_legacy_root == repo_root {
            return;
        }
        self.state.lock().cleaned_legacy_root = repo_root.to_string();
        if remove_legacy_index(std::path::Path::new(repo_root)).await {
            self.deps.log.debug(
                "git memory legacy cache removed",
                Some(json!({ "repoRoot": repo_root })),
            );
        }
    }

    fn schedule_refresh(self: &Arc<Self>, head: RepoHead, path: PathBuf) {
        let mut state = self.state.lock();
        if state.refresh {
            return;
        }
        state.refresh = true;
        drop(state);
        let this = self.clone();
        tokio::spawn(async move {
            match this.rebuild(head, path).await {
                Ok(Some(index)) => {
                    this.state.lock().index = Some(index);
                    if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (this.deps.on_index_changed)();
                    })) {
                        this.deps.log.warn(
                            "git memory refresh failed",
                            Some(json!({ "error": panic_message(error) })),
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    this.deps.log.warn(
                        "git memory refresh failed",
                        Some(json!({ "error": error.to_string() })),
                    );
                }
            }
            this.state.lock().refresh = false;
        });
    }

    async fn rebuild(
        &self,
        head: RepoHead,
        path: PathBuf,
    ) -> Result<Option<GitMemoryIndex>, WireAsyncError> {
        let history = read_git_history(
            &self.deps.process,
            &head.repo_root,
            GIT_MEMORY_WINDOW,
            head.head_sha,
            None,
        )
        .await;
        let GitReadResult::Ok {
            head_sha,
            commits,
        } = history
        else {
            return Ok(None);
        };
        let dirs = read_repo_tree(&self.deps.process, &head.repo_root, None)
            .await
            .unwrap_or_default();
        let computed_at = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let index = build_index(
            head.repo_root,
            head_sha,
            GIT_MEMORY_WINDOW,
            commits,
            computed_at,
            dirs,
        );
        if !write_index(&path, &index).await {
            self.deps.log.warn(
                "git memory cache could not be written; continuing without it",
                None,
            );
        }
        Ok(Some(index))
    }
}

fn unavailable_reason(reason: GitUnavailableReason) -> &'static str {
    match reason {
        GitUnavailableReason::NotARepo => "not-a-repo",
        GitUnavailableReason::SpawnFailed => "spawn-failed",
        GitUnavailableReason::Timeout => "timeout",
        GitUnavailableReason::Permission => "permission",
    }
}

fn panic_message(error: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = error.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = error.downcast_ref::<&'static str>() {
        return (*message).to_string();
    }
    "unknown error".to_string()
}
