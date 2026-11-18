use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;

use crate::primitives::read::ReadOutcome;

use super::concurrency::{map_with_concurrency, TOWER_FILE_IO_CONCURRENCY, TOWER_GIT_CONCURRENCY};
use super::frontmatter::{parse_frontmatter, read_frontmatter_prefix, render_frontmatter};
use super::git::{
    branch_exists, branch_tip, checkout_new_local_branch, commit_allow_empty, commit_paths,
    current_branch, diff_name_only, git, git_probe, git_with_input, has_any_commit, init_repository,
    is_ancestor, is_inside_repo, is_registered_worktree, is_worktree_dirty,
    list_base_dirty_entries, snapshot_base_wip, worktree_add, worktree_add_new_branch,
    worktree_remove,
};
use super::missionGate::{
    evaluate_mission_gate, TowerMissionBranchObservation, TowerMissionGateResult,
    TowerMissionGitObservations, TowerReviewBinding, TowerWorkerRuntime, TowerWorkerRuntimeStatus,
};
use super::paths::{
    date_dash, finding_file_name, has_non_ascii_characters, inbox_file_name,
    is_reserved_tower_agent_name, mission_file_name, review_file_name, slugify, target_slug,
    ACTIVITY_LOG, BROADCAST_NAME, FINDINGS_DIR, INBOX_DIR, LOG_DIR, MISSIONS_DIR, MISSIONS_INDEX,
    REVIEWS_DIR, STATE_FILE, TOWER_NAME, TOWER_ROOT, WORKTREES_DIR,
};
pub use super::types::{
    TowerBranchConflict, TowerCompleteResult, TowerFindingInput, TowerInitResult,
    TowerMergeResult, TowerMergeStatus, TowerMissionPatch, TowerPlanInput, TowerReanchorDigest,
    TowerReanchorMissionDigest, TowerReanchorRelation, TowerRebaseMissionResult,
    TowerRebaseMissionStatus, TowerReviewInput, TowerReviewResult, TowerSendInput, TowerSendResult,
    TowerTaskDrop,
};
use super::types::{
    TowerAgentKind, TowerInboxItem, TowerMission, TowerMissionKind, TowerMissionStatus,
    TowerMissionTask, TowerRoster, TowerRosterEntry, TowerState,
};

pub const MAX_REVIEW_ROUNDS: u32 = 5;

const PREPARED_OPERATION_TIMEOUT_MS: u64 = 60_000;
const PREPARED_OPERATION_MAX_BACKOFF_MS: u64 = 100;
const PREPARATION_CONFLICT: &str = "tower preparation conflict";

static REPO_STATE_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

fn state_lock_for(root: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let registry = REPO_STATE_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(|error| error.into_inner());
    map.entry(root.to_path_buf()).or_default().clone()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TowerAddWorktreeResult {
    pub rel: String,
    pub spawn_base: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TowerStoreOptions {
    pub state_lock_timeout_ms: u64,
    pub state_lock_poll_ms: u64,
}

impl Default for TowerStoreOptions {
    fn default() -> Self {
        Self {
            state_lock_timeout_ms: 10_000,
            state_lock_poll_ms: 10,
        }
    }
}

#[derive(Clone)]
pub enum TowerActiveAgentIds {
    Set(HashSet<String>),
    Provider(Arc<dyn Fn() -> HashSet<String> + Send + Sync>),
}

impl TowerActiveAgentIds {
    fn resolve(&self) -> HashSet<String> {
        match self {
            Self::Set(ids) => ids.clone(),
            Self::Provider(provider) => provider(),
        }
    }
}

#[derive(Clone, Default)]
pub struct TowerMissionRuntimeOptions {
    pub active_agent_ids: Option<TowerActiveAgentIds>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TowerTeardownOptions {
    pub force: bool,
    pub exclude: Vec<String>,
    pub dry_run: bool,
    pub live_agent_ids: HashSet<String>,
}

#[derive(Clone)]
struct TowerStateSnapshot {
    raw: String,
    state: TowerState,
}

#[derive(Default)]
struct MissionUpdateOptions {
    silent: bool,
    validate_only: bool,
    completion_prepared: bool,
    completion_tip: Option<String>,
    completion_key_artifacts: Option<Vec<String>>,
    force_save: bool,
    inbox_items: Option<Vec<TowerInboxItem>>,
}

struct ReviewLockedOptions {
    existing: Vec<super::types::TowerReviewInfo>,
    next_review_seq: u64,
    reviewed_commit: String,
    inbox_items: Vec<TowerInboxItem>,
    activity_log_refs: HashMap<String, bool>,
    validate_only: bool,
}

struct ReviewPreparation {
    snapshot: TowerStateSnapshot,
    review_files: Vec<String>,
    inbox_files: Vec<String>,
    options: ReviewLockedOptions,
}

struct ProtocolLockGuard {
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
    StateIo,
    InboxPublication,
}

impl ProtocolLockKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::State => "state",
            Self::StateIo => "state I/O",
            Self::InboxPublication => "inbox publication",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::State => "writing state.json",
            Self::StateIo => "reading or writing state.json",
            Self::InboxPublication => "publishing an inbox message",
        }
    }
}

struct StateLockReaperGuard {
    ref_name: String,
    token: String,
}

#[derive(Clone)]
pub struct TowerStore {
    pub repo_root: PathBuf,
    state_lock_timeout_ms: u64,
    state_lock_poll_ms: u64,
}

fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn new_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn is_open_mission(mission: &TowerMission) -> bool {
    mission.status.is_open()
}

fn mission_number(id: &str) -> i64 {
    id.strip_prefix('M')
        .and_then(|rest| rest.parse::<i64>().ok())
        .unwrap_or(0)
}

fn scope_stem(raw: &str) -> String {
    let mut value = raw;
    if let Some(stripped) = value.strip_suffix("/**") {
        value = stripped;
    } else if let Some(stripped) = value.strip_suffix("/*") {
        value = stripped;
    }
    value = value.trim_end_matches('/');
    value = value.trim_end_matches('*');
    value.to_string()
}

fn scopes_overlap(a: &[String], b: &[String]) -> bool {
    for raw_a in a {
        let stem_a = scope_stem(raw_a);
        if stem_a.is_empty() {
            continue;
        }
        for raw_b in b {
            let stem_b = scope_stem(raw_b);
            if stem_b.is_empty() {
                continue;
            }
            if stem_a == stem_b
                || stem_a.starts_with(&format!("{stem_b}/"))
                || stem_b.starts_with(&format!("{stem_a}/"))
            {
                return true;
            }
        }
    }
    false
}

fn dep_path_exists(from_id: &str, to_id: &str, by_id: &HashMap<String, TowerMission>) -> bool {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(from_id.to_string());
    while let Some(current) = queue.pop_front() {
        if current == to_id {
            return true;
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        let Some(mission) = by_id.get(&current) else {
            continue;
        };
        if !is_open_mission(mission) {
            continue;
        }
        for dep in &mission.deps {
            if !seen.contains(dep) {
                queue.push_back(dep.clone());
            }
        }
    }
    false
}

fn auto_dep_note(other: &TowerMission) -> String {
    format!(
        "auto dependency on {}: scope overlaps {} (\"{}\") — merges are serialized so shared files land in dependency order instead of colliding",
        other.id,
        other.id,
        other.scope.join(", ")
    )
}

fn add_auto_deps(
    mission: &mut TowerMission,
    candidates: &[TowerMission],
    by_id: &mut HashMap<String, TowerMission>,
) {
    if mission.kind == TowerMissionKind::Survey {
        return;
    }
    for other in candidates {
        if other.id == mission.id || other.kind == TowerMissionKind::Survey {
            continue;
        }
        if !scopes_overlap(&mission.scope, &other.scope) {
            continue;
        }
        if dep_path_exists(&mission.id, &other.id, by_id)
            || dep_path_exists(&other.id, &mission.id, by_id)
        {
            continue;
        }
        mission.deps.push(other.id.clone());
        mission.notes.push(auto_dep_note(other));
        by_id.insert(mission.id.clone(), mission.clone());
    }
}

pub fn resolve_mission_by_branch(state: &TowerState, branch: &str) -> Option<TowerMission> {
    let mut resolved: Option<TowerMission> = None;
    for mission in &state.missions {
        if mission.branch != branch || !is_open_mission(mission) {
            continue;
        }
        let replace = resolved
            .as_ref()
            .is_none_or(|current| mission_number(&mission.id) > mission_number(&current.id));
        if replace {
            resolved = Some(mission.clone());
        }
    }
    resolved
}

fn unowned_branch_message(branch: &str) -> String {
    format!(
        "branch \"{branch}\" exists in git but is not owned by any tower mission (it appeared after planning) — refusing to build the worker on unrelated history; delete or rename that branch if it is stale, or re-plan the mission under a new title"
    )
}

pub async fn assert_local_base_branch(repo_root: &Path, base: &str) -> Result<(), String> {
    if !branch_exists(repo_root, base).await? {
        return Err(format!(
            "base branch \"{base}\" does not exist as a local branch — merges land on a local branch, so remote-tracking refs and tags are not accepted; create a local branch first"
        ));
    }
    Ok(())
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

impl TowerStore {
    pub fn new<P: Into<PathBuf>>(repo_root: P) -> Self {
        Self::with_options(repo_root, TowerStoreOptions::default())
    }

    pub fn with_options<P: Into<PathBuf>>(repo_root: P, options: TowerStoreOptions) -> Self {
        Self {
            repo_root: repo_root.into(),
            state_lock_timeout_ms: options.state_lock_timeout_ms,
            state_lock_poll_ms: options.state_lock_poll_ms,
        }
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.repo_root.join(rel)
    }

    pub fn state_lock(&self) -> Arc<tokio::sync::Mutex<()>> {
        state_lock_for(&self.repo_root)
    }

    pub async fn lock_state_process(
        &self,
        operation: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, String> {
        let lock = self.state_lock();
        let deadline = crate::primitives::Deadline::new(
            format!("tower {operation}"),
            Duration::from_millis(self.state_lock_timeout_ms),
        );
        deadline
            .wait(
                "waiting for the in-process tower state lock",
                lock.lock_owned(),
            )
            .await
            .map_err(|timeout| {
                format!(
                    "{timeout} — another tower tool call in this process still holds it (a stuck git operation?); retry once that call finishes"
                )
            })
    }

    async fn run_prepared<TPrepared, TResult, Prepare, PrepareFuture, Commit, CommitFuture>(
        &self,
        prepare: Prepare,
        commit: Commit,
    ) -> Result<TResult, String>
    where
        Prepare: Fn() -> PrepareFuture + 'static,
        PrepareFuture: Future<Output = Result<TPrepared, String>> + 'static,
        Commit: Fn(TPrepared) -> CommitFuture + 'static,
        CommitFuture: Future<Output = Result<TResult, String>> + 'static,
        TPrepared: 'static,
        TResult: 'static,
    {
        let deadline = Instant::now() + Duration::from_millis(PREPARED_OPERATION_TIMEOUT_MS);
        let mut backoff_ms = self.state_lock_poll_ms;
        loop {
            let prepared = prepare().await?;
            match commit(prepared).await {
                Ok(result) => return Ok(result),
                Err(error) if error == PREPARATION_CONFLICT => {
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "tower protocol inputs kept changing for {PREPARED_OPERATION_TIMEOUT_MS}ms while preparing the operation; no stale snapshot was written, so retry the tool call"
                        ));
                    }
                    if backoff_ms > 0 {
                        sleep(Duration::from_millis(backoff_ms)).await;
                    }
                    backoff_ms = backoff_ms
                        .saturating_mul(2)
                        .min(PREPARED_OPERATION_MAX_BACKOFF_MS);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn with_state_lock<T, F, Fut>(&self, operation: F) -> Result<T, String>
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

    async fn with_state_io_lock<T, F, Fut>(&self, operation: F) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        self.with_protocol_lock(
            self.abs(&format!("{STATE_FILE}.io.lock")),
            ProtocolLockKind::StateIo,
            operation,
        )
        .await
    }

    async fn with_inbox_publication_lock<T, F, Fut>(&self, operation: F) -> Result<T, String>
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

    async fn with_completion_lock<T, F, Fut>(&self, operation: F) -> Result<T, String>
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
        let token = new_token();
        let mut last_stale_check: Option<Instant> = None;
        let guard = loop {
            if self.try_acquire_state_lock(&lock_path, &token).await? {
                break ProtocolLockGuard {
                    path: lock_path.clone(),
                    token: token.clone(),
                };
            }
            let now = Instant::now();
            if last_stale_check
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(250))
            {
                last_stale_check = Some(now);
                if self.reap_stale_state_lock(&lock_path).await? {
                    continue;
                }
            }
            if now >= deadline {
                let content = fs::read_to_string(&lock_path).await.unwrap_or_default();
                let content = content.trim();
                let holder = if content.is_empty() {
                    String::new()
                } else {
                    format!(" — held by {content}")
                };
                let same_process = lock_pid(content) == Some(std::process::id());
                let explanation = if same_process {
                    format!(
                        "another tower tool call in this process is {}; wait for it to finish",
                        kind.action()
                    )
                } else {
                    format!(
                        "another tower process may be {}; a confirmed dead holder is reclaimed automatically, so inspect the holder if this persists",
                        kind.action()
                    )
                };
                return Err(format!(
                    "timed out after {}ms waiting for the tower {} lock \"{}\"{holder} — {explanation}",
                    self.state_lock_timeout_ms,
                    kind.as_str(),
                    lock_path.display()
                ));
            }
            sleep(Duration::from_millis(self.state_lock_poll_ms)).await;
        };
        let result = operation().await;
        drop(guard);
        result
    }

    async fn try_acquire_state_lock(&self, path: &Path, token: &str) -> Result<bool, String> {
        match OpenOptions::new().write(true).create_new(true).open(path).await {
            Ok(mut file) => {
                let body = format!(
                    "pid={} since={} token={token}",
                    std::process::id(),
                    iso_now()
                );
                if let Err(error) = file.write_all(body.as_bytes()).await {
                    let _ = fs::remove_file(path).await;
                    return Err(error.to_string());
                }
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(self.state_missing_error().await)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    async fn reap_stale_state_lock(&self, path: &Path) -> Result<bool, String> {
        let Some(guard) = self.acquire_state_lock_reaper(path).await? else {
            return Ok(false);
        };
        let result = self.reap_stale_state_lock_guarded(path).await;
        let released = self.release_state_lock_reaper(&guard).await;
        match (result, released) {
            (Ok(reaped), Ok(())) => Ok(reaped),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        }
    }

    async fn state_lock_reaper_ref(&self, path: &Path) -> Result<String, String> {
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

    async fn read_state_lock_reaper(&self, ref_name: &str) -> Result<Option<String>, String> {
        match git_probe(
            &self.repo_root,
            &["rev-parse", "--verify", "--quiet", ref_name],
        )
        .await
        {
            ReadOutcome::Found(token) => Ok(Some(token)),
            ReadOutcome::Absent => Ok(None),
            ReadOutcome::Unreadable(cause) => Err(cause.to_string()),
        }
    }

    async fn acquire_state_lock_reaper(
        &self,
        lock_path: &Path,
    ) -> Result<Option<StateLockReaperGuard>, String> {
        let ref_name = self.state_lock_reaper_ref(lock_path).await?;
        let current = self.read_state_lock_reaper(ref_name.as_str()).await?;
        if let Some(current_token) = current.as_ref() {
            let holder = git(&self.repo_root, &["cat-file", "-p", current_token.as_str()]).await?;
            let Some(pid) = lock_pid(&holder) else {
                return Ok(None);
            };
            if pid == 0 || !self.state_lock_holder_is_dead(pid).await {
                return Ok(None);
            }
        }
        let token = git_with_input(
            &self.repo_root,
            &["hash-object", "-w", "--stdin"],
            &format!(
                "pid={} since={} token={}",
                std::process::id(),
                iso_now(),
                new_token()
            ),
        )
        .await?;
        let expected = current.as_deref().unwrap_or("");
        match git(
            &self.repo_root,
            &["update-ref", ref_name.as_str(), token.as_str(), expected],
        )
        .await
        {
            Ok(_) => Ok(Some(StateLockReaperGuard { ref_name, token })),
            Err(error) => {
                let after = self.read_state_lock_reaper(&ref_name).await?;
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

    async fn release_state_lock_reaper(&self, guard: &StateLockReaperGuard) -> Result<(), String> {
        match git(
            &self.repo_root,
            &[
                "update-ref",
                "-d",
                guard.ref_name.as_str(),
                guard.token.as_str(),
            ],
        )
        .await
        {
            Ok(_) => Ok(()),
            Err(error) => {
                let current = self.read_state_lock_reaper(&guard.ref_name).await?;
                if current.is_none() || current.as_deref() != Some(guard.token.as_str()) {
                    return Ok(());
                }
                Err(error)
            }
        }
    }

    async fn reap_stale_state_lock_guarded(&self, path: &Path) -> Result<bool, String> {
        let Ok(content) = fs::read_to_string(path).await else {
            return Ok(false);
        };
        let Some(pid) = lock_pid(&content) else {
            return Ok(false);
        };
        if pid == 0 || !self.state_lock_holder_is_dead(pid).await {
            return Ok(false);
        }
        let Ok(confirmed) = fs::read_to_string(path).await else {
            return Ok(false);
        };
        if confirmed != content || !self.state_lock_holder_is_dead(pid).await {
            return Ok(false);
        }
        match fs::remove_file(path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn state_lock_holder_is_dead(&self, pid: u32) -> bool {
        if pid == std::process::id() {
            return false;
        }
        !pid_alive(pid)
    }

    async fn is_initialized_unlocked(&self) -> bool {
        fs::read_to_string(self.abs(STATE_FILE)).await.is_ok()
    }

    pub async fn is_initialized(&self) -> Result<bool, String> {
        if fs::metadata(self.abs(STATE_FILE)).await.is_err() {
            return Ok(false);
        }
        self.with_state_io_lock(|| async { Ok(self.is_initialized_unlocked().await) })
            .await
    }

    pub async fn is_state_lost(&self) -> Result<bool, String> {
        if self.is_initialized().await? {
            return Ok(false);
        }
        Ok(fs::metadata(self.abs(TOWER_ROOT)).await.is_ok())
    }

    async fn state_missing_error(&self) -> String {
        if !self.is_initialized_unlocked().await
            && fs::metadata(self.abs(TOWER_ROOT)).await.is_ok()
        {
            return "tower state was lost: .tower/comms/state.json is missing but the .tower/ directory still exists — the tower was initialized here and its comms state was deleted (e.g. by git clean), so all recorded mission, roster, and review history is gone. This operation is refused instead of silently running against an empty history; TowerStatus reports the loss, and TowerSend/TowerComplete recover a minimal state (stamped recoveredAt) automatically".to_string();
        }
        "tower is not initialized in this repository — run TowerInit first".to_string()
    }

    pub async fn ensure_repository(&self, base: Option<&str>) -> Result<(), String> {
        if is_inside_repo(&self.repo_root).await? {
            return Ok(());
        }
        init_repository(&self.repo_root).await?;
        let unborn = git(&self.repo_root, &["symbolic-ref", "--short", "HEAD"])
            .await
            .unwrap_or_else(|_| "main".to_string());
        let resolved_base = base.map(str::to_string).unwrap_or_else(|| unborn.clone());
        if resolved_base != unborn {
            checkout_new_local_branch(&self.repo_root, &resolved_base).await?;
        }
        let dirty = list_base_dirty_entries(&self.repo_root).await?;
        if dirty.is_empty() {
            return commit_allow_empty(&self.repo_root, "tower: init").await;
        }
        let paths: Vec<String> = dirty.into_iter().map(|entry| entry.path).collect();
        commit_paths(
            &self.repo_root,
            &paths,
            &format!("tower: snapshot of uncommitted base checkout changes (base {resolved_base})"),
        )
        .await
    }

    pub async fn init(
        &self,
        session_id: Option<String>,
        base: Option<String>,
    ) -> Result<TowerInitResult, String> {
        self.ensure_repository(base.as_deref()).await?;
        if !has_any_commit(&self.repo_root).await? {
            return Err(
                "the repository has no commits yet — create an initial commit first".into(),
            );
        }
        let state_path = self.abs(STATE_FILE);
        if let Some(parent) = state_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        self.with_state_lock(move || async move {
            if self.is_initialized().await? {
                let mut state = self.load().await?;
                let mut rebased_from = None;
                let mut rebase_digest = None;
                if let Some(requested_base) = base.as_deref() {
                    if requested_base != state.base {
                        let reanchor = self.reanchor_locked(&mut state, requested_base).await?;
                        rebased_from = Some(reanchor.from.clone());
                        rebase_digest = Some(reanchor);
                    }
                }
                let retired_agents = self.adopt_foreign_roster(&mut state, session_id).await?;
                let checkout = self.checked_out_branch().await;
                let open_missions = state
                    .missions
                    .iter()
                    .filter(|mission| is_open_mission(mission))
                    .map(|mission| mission.id.clone())
                    .collect();
                return Ok(TowerInitResult {
                    base: state.base,
                    created: false,
                    retired_agents,
                    checkout,
                    rebased_from,
                    rebase_digest,
                    open_missions,
                });
            }

            let checkout = self.checked_out_branch().await;
            let resolved_base = match base.clone() {
                Some(requested_base) => {
                    assert_local_base_branch(&self.repo_root, &requested_base).await?;
                    requested_base
                }
                None => {
                    if checkout == "HEAD" {
                        return Err("cannot determine the base branch from a detached HEAD — pass the base branch explicitly".into());
                    }
                    checkout.clone()
                }
            };

            for dir in &[
                INBOX_DIR,
                FINDINGS_DIR,
                REVIEWS_DIR,
                MISSIONS_DIR,
                LOG_DIR,
                WORKTREES_DIR,
            ] {
                fs::create_dir_all(self.abs(dir))
                    .await
                    .map_err(|error| error.to_string())?;
            }
            self.ensure_git_exclude().await?;

            let state = TowerState {
                version: 1,
                base: resolved_base.clone(),
                mode: "branch".into(),
                created_at: iso_now(),
                recovered_at: None,
                session_id,
                inbox_ack_ids: None,
                roster: TowerRoster::default(),
                missions: Vec::new(),
            };
            self.save(&state).await?;
            fs::write(self.abs(ACTIVITY_LOG), b"")
                .await
                .map_err(|error| error.to_string())?;
            self.render_missions_index(&state).await?;
            self.append_log(
                TOWER_NAME,
                "init",
                &[
                    ("mode", Some(state.mode.as_str())),
                    ("base", Some(resolved_base.as_str())),
                ],
                Some(MISSIONS_INDEX),
            )
            .await?;
            Ok(TowerInitResult {
                base: resolved_base,
                created: true,
                retired_agents: Vec::new(),
                checkout,
                rebased_from: None,
                rebase_digest: None,
                open_missions: Vec::new(),
            })
        })
        .await
    }

    async fn reanchor_locked(
        &self,
        state: &mut TowerState,
        base: &str,
    ) -> Result<TowerReanchorDigest, String> {
        assert_local_base_branch(&self.repo_root, base).await?;
        let base_tip = branch_tip(&self.repo_root, base).await?;
        let open: Vec<TowerMission> = state
            .missions
            .iter()
            .filter(|mission| is_open_mission(mission))
            .cloned()
            .collect();
        let mut missions = Vec::new();
        for mission in &open {
            if !branch_exists(&self.repo_root, &mission.branch).await? {
                missions.push(TowerReanchorMissionDigest {
                    id: mission.id.clone(),
                    branch: mission.branch.clone(),
                    relation: TowerReanchorRelation::NoBranch,
                    branch_tip: None,
                });
                continue;
            }
            let mission_tip = branch_tip(&self.repo_root, mission.branch.as_str()).await?;
            let relation = if is_ancestor(&self.repo_root, &base_tip, &mission_tip).await? {
                TowerReanchorRelation::UpToDate
            } else {
                TowerReanchorRelation::Stale
            };
            missions.push(TowerReanchorMissionDigest {
                id: mission.id.clone(),
                branch: mission.branch.clone(),
                relation,
                branch_tip: Some(mission_tip),
            });
        }
        let digest = TowerReanchorDigest {
            from: state.base.clone(),
            to: base.to_string(),
            base_tip,
            missions,
        };
        state.base = base.to_string();
        self.save(state).await?;
        let open_missions = if open.is_empty() {
            None
        } else {
            Some(open.len().to_string())
        };
        let relations = if digest.missions.is_empty() {
            None
        } else {
            Some(
                digest
                    .missions
                    .iter()
                    .map(|mission| format!("{}:{}", mission.id, mission.relation.as_str()))
                    .collect::<Vec<_>>()
                    .join(","),
            )
        };
        self.append_log(
            TOWER_NAME,
            "rebase",
            &[
                ("from", Some(digest.from.as_str())),
                ("to", Some(base)),
                ("open_missions", open_missions.as_deref()),
                ("relations", relations.as_deref()),
            ],
            None,
        )
        .await?;
        Ok(digest)
    }

    pub async fn rebase(&self, base: &str) -> Result<Option<TowerReanchorDigest>, String> {
        self.with_state_lock(|| async {
            let mut state = self.load().await?;
            if state.base == base {
                return Ok(None);
            }
            Ok(Some(self.reanchor_locked(&mut state, base).await?))
        })
        .await
    }

    async fn checked_out_branch(&self) -> String {
        git(&self.repo_root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap_or_else(|_| "HEAD".to_string())
    }

    async fn adopt_foreign_roster(
        &self,
        state: &mut TowerState,
        session_id: Option<String>,
    ) -> Result<Vec<String>, String> {
        let Some(session_id) = session_id else {
            return Ok(Vec::new());
        };
        if state.session_id.as_deref() == Some(session_id.as_str()) {
            return Ok(Vec::new());
        }
        let previous = state.session_id.clone();
        let stale: Vec<String> = state
            .roster
            .agents
            .iter()
            .filter(|agent| agent.session_id.as_deref() != Some(session_id.as_str()))
            .map(|agent| agent.name.clone())
            .collect();
        state
            .roster
            .agents
            .retain(|agent| agent.session_id.as_deref() == Some(session_id.as_str()));
        state.session_id = Some(session_id.clone());
        self.save(state).await?;
        let retired = if stale.is_empty() {
            None
        } else {
            Some(stale.join(","))
        };
        self.append_log(
            TOWER_NAME,
            "adopt",
            &[
                ("session", Some(session_id.as_str())),
                (
                    "previous",
                    Some(previous.as_deref().unwrap_or("unknown")),
                ),
                ("retired", retired.as_deref()),
            ],
            None,
        )
        .await?;
        Ok(stale)
    }

    pub async fn adopt(&self, session_id: &str) -> Result<Vec<String>, String> {
        if fs::metadata(self.abs(STATE_FILE)).await.is_err() {
            return Ok(Vec::new());
        }
        let read = self
            .with_state_io_lock(|| async {
                fs::read_to_string(self.abs(STATE_FILE))
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            })
            .await;
        match read {
            Ok(()) => {}
            Err(error) => {
                if fs::metadata(self.abs(STATE_FILE)).await.is_err() {
                    return Ok(Vec::new());
                }
                return Err(error);
            }
        }
        self.with_state_lock(|| async {
            let mut state = self.load().await?;
            self.adopt_foreign_roster(&mut state, Some(session_id.to_string()))
                .await
        })
        .await
    }

    pub async fn release(&self, session_id: &str) -> Result<(), String> {
        if !self.is_initialized().await? {
            return Ok(());
        }
        self.with_state_lock(|| async {
            let mut state = self.load().await?;
            if state.session_id.as_deref() != Some(session_id) {
                return Ok(());
            }
            state.session_id = None;
            self.save(&state).await?;
            self.append_log(
                TOWER_NAME,
                "release",
                &[("session", Some(session_id))],
                None,
            )
            .await
        })
        .await
    }

    async fn ensure_git_exclude(&self) -> Result<(), String> {
        let git_dir = match read_git_dir(&self.repo_root).await {
            Some(dir) => PathBuf::from(dir),
            None => self.repo_root.join(".git"),
        };
        let exclude_path = git_dir.join("info").join("exclude");
        if let Some(parent) = exclude_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|error| error.to_string())?;
        }
        let existing = fs::read_to_string(&exclude_path).await.unwrap_or_default();
        if existing
            .split('\n')
            .any(|line| line.trim_end_matches('\r').trim() == ".tower/")
        {
            return Ok(());
        }
        let prefix = if existing.is_empty() || existing.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude_path)
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(format!("{prefix}.tower/\n").as_bytes())
            .await
            .map_err(|error| error.to_string())
    }
}

async fn read_git_dir(cwd: &Path) -> Option<String> {
    let raw = fs::read_to_string(cwd.join(".git")).await.ok()?;
    let line = raw.lines().find_map(|line| line.strip_prefix("gitdir:"))?;
    Some(line.trim().to_string())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TowerUpdateMissionOptions {
    pub silent: bool,
}

fn assert_scopes_disjoint(missions: &[TowerMission]) -> Result<(), String> {
    let mut scopes: Vec<(String, String, String)> = Vec::new();
    for mission in missions {
        if mission.kind == TowerMissionKind::Survey {
            continue;
        }
        for raw in &mission.scope {
            let stem = scope_stem(raw);
            if stem.is_empty() {
                return Err(format!(
                    "mission {} scope \"{raw}\" covers the whole repo — narrow it down",
                    mission.id
                ));
            }
            scopes.push((mission.id.clone(), raw.clone(), stem));
        }
    }
    let by_id: HashMap<String, TowerMission> = missions
        .iter()
        .map(|mission| (mission.id.clone(), mission.clone()))
        .collect();
    for i in 0..scopes.len() {
        for j in (i + 1)..scopes.len() {
            let a = &scopes[i];
            let b = &scopes[j];
            if a.0 == b.0 {
                continue;
            }
            if a.2 == b.2
                || a.2.starts_with(&format!("{}/", b.2))
                || b.2.starts_with(&format!("{}/", a.2))
            {
                if dep_path_exists(&a.0, &b.0, &by_id) || dep_path_exists(&b.0, &a.0, &by_id) {
                    continue;
                }
                return Err(format!(
                    "mission scopes overlap: {} (\"{}\") vs {} (\"{}\") without a dependency between them — split the shared files into exactly one mission, or wire a dependency so the merges serialize (TowerPlan and scope widening auto-add one when scopes overlap; if one of them is stale finished work, abandon it first (TowerMission status=abandoned))",
                    a.0, a.1, b.0, b.1
                ));
            }
        }
    }
    Ok(())
}

impl TowerStore {
    async fn load_snapshot(&self) -> Result<TowerStateSnapshot, String> {
        self.with_state_io_lock(|| async {
            let raw = match fs::read_to_string(self.abs(STATE_FILE)).await {
                Ok(raw) => raw,
                Err(_) => {
                    return Err(self.state_missing_error().await);
                }
            };
            let state: TowerState =
                serde_json::from_str(&raw).map_err(|error| error.to_string())?;
            Ok(TowerStateSnapshot { raw, state })
        })
        .await
    }

    pub async fn load(&self) -> Result<TowerState, String> {
        Ok(self.load_snapshot().await?.state)
    }

    async fn assert_preparation_current(
        &self,
        snapshot: &TowerStateSnapshot,
        inbox_files: Option<&[String]>,
        review_files: Option<&[String]>,
    ) -> Result<TowerState, String> {
        let current = self.load_snapshot().await?;
        if current.raw != snapshot.raw {
            return Err(PREPARATION_CONFLICT.into());
        }
        if let Some(expected) = inbox_files {
            self.assert_directory_current(INBOX_DIR, expected, |file| file.ends_with(".md"))
                .await?;
        }
        if let Some(expected) = review_files {
            self.assert_directory_current(REVIEWS_DIR, expected, |file| {
                file.starts_with("review-") && file.ends_with(".md")
            })
            .await?;
        }
        Ok(current.state)
    }

    async fn assert_directory_current(
        &self,
        dir: &str,
        expected: &[String],
        include: fn(&str) -> bool,
    ) -> Result<(), String> {
        let mut files = match fs::read_dir(self.abs(dir)).await {
            Ok(mut entries) => {
                let mut files = Vec::new();
                while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())?
                {
                    files.push(entry.file_name().to_string_lossy().to_string());
                }
                files
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.to_string()),
        };
        files.retain(|file| include(file));
        files.sort();
        if files != expected {
            return Err(PREPARATION_CONFLICT.into());
        }
        Ok(())
    }

    pub async fn load_or_recover(&self) -> Result<TowerState, String> {
        if self.is_initialized().await? {
            return self.load().await;
        }
        if !self.is_state_lost().await? {
            return Err("tower is not initialized in this repository — run TowerInit first".into());
        }
        self.recover_minimal_state().await
    }

    async fn recover_minimal_state(&self) -> Result<TowerState, String> {
        for dir in &[
            INBOX_DIR,
            FINDINGS_DIR,
            REVIEWS_DIR,
            MISSIONS_DIR,
            LOG_DIR,
            WORKTREES_DIR,
        ] {
            fs::create_dir_all(self.abs(dir))
                .await
                .map_err(|error| error.to_string())?;
        }
        self.with_state_lock(|| async {
            if self.is_initialized().await? {
                return self.load().await;
            }
            let checkout = self.checked_out_branch().await;
            if checkout == "HEAD" {
                return Err("tower state was lost and the checkout is on a detached HEAD — the recovered minimal state takes the current checkout branch as its base, which a detached HEAD cannot provide; check out the previous base branch and retry, or run TowerInit with an explicit base".into());
            }
            let now = iso_now();
            let state = TowerState {
                version: 1,
                base: checkout.clone(),
                mode: "branch".into(),
                created_at: now.clone(),
                recovered_at: Some(now.clone()),
                session_id: None,
                inbox_ack_ids: None,
                roster: TowerRoster::default(),
                missions: Vec::new(),
            };
            self.save(&state).await?;
            self.render_missions_index(&state).await?;
            self.append_log(
                TOWER_NAME,
                "state.recovered",
                &[
                    ("base", Some(checkout.as_str())),
                    ("recovered_at", Some(now.as_str())),
                ],
                Some(MISSIONS_INDEX),
            )
            .await?;
            Ok(state)
        })
        .await
    }

    pub async fn resolve_messaging_caller(
        &self,
        state: &TowerState,
        agent_id: &str,
    ) -> Result<(String, TowerState, bool), String> {
        if agent_id == "main" || state.recovered_at.is_none() {
            return Ok((self.resolve_caller_name(state, agent_id)?, state.clone(), false));
        }
        if let Some(entry) = self.resolve_agent(state, agent_id) {
            return Ok((entry.name, state.clone(), false));
        }
        let spawned_at = state.recovered_at.clone().unwrap_or_default();
        self.register_agent(TowerRosterEntry {
            name: agent_id.to_string(),
            agent_id: agent_id.to_string(),
            session_id: None,
            kind: TowerAgentKind::Worker,
            mission_id: None,
            review_target: None,
            review_mission_id: None,
            worktree: None,
            branch: None,
            spawned_at,
            last_inbox_read_at: None,
            inbox_ack_ids: None,
            died_at: None,
            death_status: None,
            death_reason: None,
            status: None,
        })
        .await?;
        self.append_log(
            TOWER_NAME,
            "roster.placeholder",
            &[
                ("agent", Some(agent_id)),
                (
                    "recovered_at",
                    state.recovered_at.as_deref(),
                ),
            ],
            None,
        )
        .await?;
        Ok((agent_id.to_string(), self.load().await?, true))
    }

    async fn save(&self, state: &TowerState) -> Result<(), String> {
        self.with_state_io_lock(|| async {
            let file = self.abs(STATE_FILE);
            let tmp = format!(
                "{}.tmp-{}-{}",
                file.to_string_lossy(),
                std::process::id(),
                new_token()
            );
            let json = serde_json::to_string_pretty(state).map_err(|error| error.to_string())?;
            let result = async {
                fs::write(&tmp, format!("{json}\n").as_bytes())
                    .await
                    .map_err(|error| error.to_string())?;
                fs::rename(&tmp, &file)
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            if result.is_err() {
                let _ = fs::remove_file(&tmp).await;
            }
            result
        })
        .await
    }

    pub async fn append_log(
        &self,
        actor: &str,
        action: &str,
        details: &[(&str, Option<&str>)],
        ref_path: Option<&str>,
    ) -> Result<(), String> {
        let mut parts = vec![iso_now(), actor.to_string(), action.to_string()];
        for (key, value) in details {
            if let Some(value) = value {
                parts.push(format!("{key}={value}"));
            }
        }
        if let Some(ref_path) = ref_path {
            parts.push(format!("ref={ref_path}"));
        }
        let line = format!("{}\n", parts.join(" "));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.abs(ACTIVITY_LOG))
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn recent_log(&self, lines: usize) -> Vec<String> {
        let content = match fs::read_to_string(self.abs(ACTIVITY_LOG)).await {
            Ok(content) => content,
            Err(_) => return Vec::new(),
        };
        let all: Vec<String> = content
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect();
        let start = all.len().saturating_sub(lines);
        all[start..].to_vec()
    }

    pub fn resolve_agent(&self, state: &TowerState, agent_id: &str) -> Option<TowerRosterEntry> {
        let mut resolved = None;
        for agent in &state.roster.agents {
            if agent.agent_id == agent_id {
                resolved = Some(agent.clone());
            }
        }
        resolved
    }

    pub fn resolve_caller_name(&self, state: &TowerState, agent_id: &str) -> Result<String, String> {
        if agent_id == "main" {
            return Ok(TOWER_NAME.to_string());
        }
        match self.resolve_agent(state, agent_id) {
            Some(entry) => Ok(entry.name),
            None => Err(format!(
                "agent \"{agent_id}\" is not a tower participant — only spawned workers/reviewers and the tower can use tower tools"
            )),
        }
    }

    pub fn find_agent(&self, state: &TowerState, name: &str) -> Option<TowerRosterEntry> {
        state
            .roster
            .agents
            .iter()
            .find(|agent| agent.name == name)
            .cloned()
    }

    pub fn is_stopped_by_user(entry: &TowerRosterEntry) -> bool {
        const UPSTREAM_USER_CANCELLATION_REASON: &str = "Aborted by the user";
        entry.died_at.is_some()
            && entry.death_reason.as_deref().is_some_and(|reason| {
                let reason = reason.trim();
                reason.eq_ignore_ascii_case("user cancelled")
                    || reason.eq_ignore_ascii_case(UPSTREAM_USER_CANCELLATION_REASON)
            })
    }

    pub fn find_by_name(&self, state: &TowerState, name: &str) -> Option<TowerRosterEntry> {
        self.find_agent(state, name)
    }

    pub async fn register_agent(&self, entry: TowerRosterEntry) -> Result<(), String> {
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            self.register_agent_locked(entry, &mut state)?;
            self.save(&state).await
        })
        .await
    }

    pub async fn register_spawned_agent(
        &self,
        entry: TowerRosterEntry,
        mission: Option<(String, String)>,
    ) -> Result<(), String> {
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            self.register_agent_locked(entry, &mut state)?;
            let Some((mission_id, owner)) = mission else {
                return self.save(&state).await;
            };
            self.update_mission_locked(
                TOWER_NAME,
                &mission_id,
                &TowerMissionPatch {
                    status: Some(TowerMissionStatus::Active),
                    owner: Some(owner),
                    ..Default::default()
                },
                &mut state,
                &MissionUpdateOptions {
                    silent: true,
                    force_save: true,
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
        })
        .await
    }

    fn register_agent_locked(
        &self,
        mut entry: TowerRosterEntry,
        state: &mut TowerState,
    ) -> Result<(), String> {
        if entry.name.trim().is_empty() || entry.name.trim() != entry.name {
            return Err(format!(
                "tower agent name \"{}\" must not be blank or carry surrounding whitespace",
                entry.name
            ));
        }
        if is_reserved_tower_agent_name(&entry.name) {
            return Err(format!(
                "tower agent name \"{}\" is reserved by the tower protocol — pick a different name",
                entry.name
            ));
        }
        let mut index = state.roster.agents.len();
        while index > 0 {
            index -= 1;
            if state.roster.agents[index].agent_id == entry.agent_id {
                state.roster.agents.remove(index);
            }
        }
        if self.find_agent(state, &entry.name).is_some() {
            return Err(format!(
                "tower agent name \"{}\" is already registered",
                entry.name
            ));
        }
        if entry.last_inbox_read_at.is_none() {
            entry.last_inbox_read_at = Some(entry.spawned_at.clone());
        }
        state.roster.agents.push(entry);
        Ok(())
    }

    pub async fn mark_agent_died(
        &self,
        agent_id: &str,
        status: &str,
        reason: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Option<TowerRosterEntry>, String> {
        let agent_id = agent_id.to_string();
        let status = status.to_string();
        let reason = reason.map(str::to_string);
        let session_id = session_id.map(str::to_string);
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            if let Some(session_id) = session_id.as_deref()
                && let Some(current) = state.session_id.as_deref()
                && current != session_id
            {
                return Ok(None);
            }
            let Some(index) = state
                .roster
                .agents
                .iter()
                .rposition(|agent| agent.agent_id == agent_id)
            else {
                return Ok(None);
            };
            let existing = state.roster.agents[index].clone();
            if existing.died_at.is_some() {
                return Ok(Some(existing));
            }
            let mut entry = existing.clone();
            entry.died_at = Some(iso_now());
            entry.death_status = Some(status.clone());
            entry.death_reason = reason.clone();
            entry.status = Some("dead".to_string());
            state.roster.agents[index] = entry.clone();
            self.save(&state).await?;
            let mission = state
                .missions
                .iter()
                .find(|mission| Some(&mission.id) == entry.mission_id.as_ref());
            let mission_ref =
                mission.map(|mission| format!("{MISSIONS_DIR}/{}", mission_file_name(&mission.id, &mission.slug)));
            let reason_log = reason.as_deref().map(|reason| {
                reason
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(200)
                    .collect::<String>()
            });
            let session_log = session_id.clone();
            let pid_log = std::process::id().to_string();
            self.append_log(
                TOWER_NAME,
                "died",
                &[
                    ("name", Some(entry.name.as_str())),
                    ("agent", Some(agent_id.as_str())),
                    ("kind", Some(entry.kind.as_str())),
                    ("status", Some(status.as_str())),
                    ("reason", reason_log.as_deref()),
                    ("mission", entry.mission_id.as_deref()),
                    ("target", entry.review_target.as_deref()),
                    ("session", session_log.as_deref()),
                    ("pid", Some(pid_log.as_str())),
                ],
                mission_ref.as_deref(),
            )
            .await?;
            Ok(Some(entry))
        })
        .await
    }

    pub async fn clear_agent_died(
        &self,
        agent_id: &str,
        session_id: Option<&str>,
    ) -> Result<bool, String> {
        let agent_id = agent_id.to_string();
        let session_id = session_id.map(str::to_string);
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            if let Some(session_id) = session_id.as_deref()
                && let Some(current) = state.session_id.as_deref()
                && current != session_id
            {
                return Ok(false);
            }
            let Some(index) = state
                .roster
                .agents
                .iter()
                .rposition(|agent| agent.agent_id == agent_id)
            else {
                return Ok(false);
            };
            let existing = state.roster.agents[index].clone();
            if existing.died_at.is_none() {
                return Ok(false);
            }
            let mut entry = existing.clone();
            entry.died_at = None;
            entry.death_status = None;
            entry.death_reason = None;
            entry.status = None;
            state.roster.agents[index] = entry.clone();
            self.save(&state).await?;
            let session_log = session_id.clone();
            let pid_log = std::process::id().to_string();
            self.append_log(
                TOWER_NAME,
                "revived",
                &[
                    ("name", Some(entry.name.as_str())),
                    ("agent", Some(agent_id.as_str())),
                    ("kind", Some(entry.kind.as_str())),
                    ("session", session_log.as_deref()),
                    ("pid", Some(pid_log.as_str())),
                ],
                None,
            )
            .await?;
            Ok(true)
        })
        .await
    }

    pub async fn plan(&self, input: &[TowerPlanInput]) -> Result<Vec<TowerMission>, String> {
        if input.is_empty() {
            return Err("TowerPlan needs at least one mission".into());
        }
        for item in input {
            if has_non_ascii_characters(&item.title) {
                let offending = item
                    .title
                    .chars()
                    .find(|character| !(' '..='~').contains(character))
                    .unwrap_or(' ');
                return Err(format!(
                    "mission title \"{}\" contains non-ASCII characters (first: \"{offending}\") — titles must be printable ASCII English: the title becomes the branch/worktree slug, and non-ASCII text slugs to a generic word like \"item\" that collides across missions; rewrite the title in English with a unique identifier word (e.g. a business code like B010100) and plan again",
                    item.title
                ));
            }
        }
        let input = input.to_vec();
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            let start_index = state.missions.len();
            let mut missions: Vec<TowerMission> = input
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let number = start_index + index + 1;
                    let slug = slugify(&item.title, 40);
                    TowerMission {
                        id: format!("M{number}"),
                        title: item.title.clone(),
                        slug: slug.clone(),
                        kind: item.kind.unwrap_or_default(),
                        scope: item.scope.clone(),
                        branch: format!("feat/{slug}"),
                        worktree: format!("wt-{number}"),
                        spawn_base: None,
                        deps: item.deps.clone().unwrap_or_default(),
                        last_rebase: None,
                        completion_tip: None,
                        key_artifacts: None,
                        status: TowerMissionStatus::Planned,
                        owner: None,
                        context: item
                            .context
                            .as_deref()
                            .map(str::trim)
                            .filter(|context| !context.is_empty())
                            .map(str::to_string),
                        tasks: item
                            .tasks
                            .clone()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|text| TowerMissionTask {
                                text,
                                done: false,
                                dropped: false,
                            })
                            .collect(),
                        notes: Vec::new(),
                        blockers: Vec::new(),
                    }
                })
                .collect();

            let known_ids: HashSet<String> = state
                .missions
                .iter()
                .map(|mission| mission.id.clone())
                .chain(missions.iter().map(|mission| mission.id.clone()))
                .collect();
            for mission in &missions {
                for dep in &mission.deps {
                    if !known_ids.contains(dep) {
                        return Err(format!(
                            "mission {} depends on unknown mission \"{dep}\"",
                            mission.id
                        ));
                    }
                }
            }
            let mut taken_branches: HashMap<String, TowerMission> = state
                .missions
                .iter()
                .map(|mission| (mission.branch.clone(), mission.clone()))
                .collect();
            for mission in &missions {
                if let Some(existing) = taken_branches.get(&mission.branch) {
                    return Err(format!(
                        "mission {} branch \"{}\" is already used by {} ({}) \"{}\" — change the title so its slug differs; branch-to-mission resolution must stay unambiguous",
                        mission.id,
                        mission.branch,
                        existing.id,
                        existing.status.as_str(),
                        existing.title
                    ));
                }
                if branch_exists(&self.repo_root, &mission.branch).await? {
                    return Err(format!(
                        "mission {} branch \"{}\" already exists in git but is not owned by any tower mission — the worker would start on that branch's unrelated history; change the title so its slug differs, or delete/rename the stale branch if it is a leftover",
                        mission.id, mission.branch
                    ));
                }
                taken_branches.insert(mission.branch.clone(), mission.clone());
            }

            let open_existing: Vec<TowerMission> = state
                .missions
                .iter()
                .filter(|mission| is_open_mission(mission))
                .cloned()
                .collect();
            let mut by_id: HashMap<String, TowerMission> = open_existing
                .iter()
                .cloned()
                .chain(missions.iter().cloned())
                .map(|mission| (mission.id.clone(), mission))
                .collect();
            for index in 0..missions.len() {
                let id = missions[index].id.clone();
                let older: Vec<TowerMission> = open_existing
                    .iter()
                    .cloned()
                    .chain(
                        missions
                            .iter()
                            .filter(|mission| mission_number(&mission.id) < mission_number(&id))
                            .cloned(),
                    )
                    .collect();
                let mut mission = missions[index].clone();
                add_auto_deps(&mut mission, &older, &mut by_id);
                missions[index] = mission;
            }
            let mut all: Vec<TowerMission> = open_existing;
            all.extend(missions.iter().cloned());
            assert_scopes_disjoint(&all)?;

            state.missions.extend(missions.iter().cloned());
            self.save(&state).await?;
            self.render_missions_index(&state).await?;
            for mission in &missions {
                self.render_mission_file(mission).await?;
            }
            let ids = missions
                .iter()
                .map(|mission| mission.id.as_str())
                .collect::<Vec<_>>()
                .join(",");
            self.append_log(
                TOWER_NAME,
                "plan",
                &[("missions", Some(ids.as_str()))],
                Some(MISSIONS_INDEX),
            )
            .await?;
            Ok(missions)
        })
        .await
    }

    pub async fn update_mission(
        &self,
        caller_name: &str,
        id: &str,
        patch: TowerMissionPatch,
        options: TowerUpdateMissionOptions,
    ) -> Result<TowerMission, String> {
        if patch.status != Some(TowerMissionStatus::Completed) {
            let caller_name = caller_name.to_string();
            let id = id.to_string();
            return self
                .with_state_lock(move || async move {
                    let mut state = self.load().await?;
                    self.update_mission_locked(
                        &caller_name,
                        &id,
                        &patch,
                        &mut state,
                        &MissionUpdateOptions {
                            silent: options.silent,
                            ..Default::default()
                        },
                    )
                    .await
                })
                .await;
        }

        let prepare_store = self.clone();
        let prepare_caller = caller_name.to_string();
        let prepare_id = id.to_string();
        let prepare_patch = patch.clone();
        let commit_store = self.clone();
        let commit_caller = caller_name.to_string();
        let commit_id = id.to_string();
        let commit_patch = patch.clone();
        self.run_prepared(
            move || {
                let store = prepare_store.clone();
                let caller = prepare_caller.clone();
                let id = prepare_id.clone();
                let patch = prepare_patch.clone();
                async move {
                    let snapshot = store.load_snapshot().await?;
                    let inbox = store.list_inbox_snapshot().await?;
                    let mut validation_state = snapshot.state.clone();
                    store
                        .update_mission_locked(
                            &caller,
                            &id,
                            &patch,
                            &mut validation_state,
                            &MissionUpdateOptions {
                                silent: options.silent,
                                validate_only: true,
                                inbox_items: Some(inbox.0),
                                ..Default::default()
                            },
                        )
                        .await?;
                    Ok((snapshot, inbox.1))
                }
            },
            move |(snapshot, inbox_files)| {
                let store = commit_store.clone();
                let caller = commit_caller.clone();
                let id = commit_id.clone();
                let patch = commit_patch.clone();
                async move {
                    let lock_store = store.clone();
                    lock_store
                        .with_completion_lock(move || async move {
                            let mut state = store
                                .assert_preparation_current(
                                    &snapshot,
                                    Some(inbox_files.as_slice()),
                                    None,
                                )
                                .await?;
                            store
                                .update_mission_locked(
                                    &caller,
                                    &id,
                                    &patch,
                                    &mut state,
                                    &MissionUpdateOptions {
                                        silent: options.silent,
                                        completion_prepared: true,
                                        ..Default::default()
                                    },
                                )
                                .await
                        })
                        .await
                }
            },
        )
        .await
    }

    async fn update_mission_locked(
        &self,
        caller_name: &str,
        id: &str,
        patch: &TowerMissionPatch,
        state: &mut TowerState,
        options: &MissionUpdateOptions,
    ) -> Result<TowerMission, String> {
        let mission_index = state
            .missions
            .iter()
            .position(|mission| mission.id == id)
            .ok_or_else(|| format!("unknown mission \"{id}\""))?;
        let previous_status = state.missions[mission_index].status;
        if caller_name != TOWER_NAME {
            let caller = self.find_agent(state, caller_name);
            let owns = caller
                .as_ref()
                .is_some_and(|agent| agent.kind == TowerAgentKind::Worker)
                && caller
                    .as_ref()
                    .and_then(|agent| agent.mission_id.as_deref())
                    == Some(id);
            if !owns {
                return Err(format!(
                    "agent \"{caller_name}\" does not own mission {id} — workers update only their own mission file"
                ));
            }
        }

        let mission = &state.missions[mission_index];
        let is_noop = patch.status == Some(mission.status)
            && patch.note.is_none()
            && patch.blocker.is_none()
            && patch.clear_blockers.is_none()
            && patch.task_done.is_none()
            && patch.task_drop.is_none()
            && patch.owner.is_none()
            && patch.scope.is_none()
            && patch.spawn_base.is_none()
            && !(mission.kind == TowerMissionKind::Artifact
                && patch.status == Some(TowerMissionStatus::Completed));
        if is_noop {
            if options.force_save {
                self.save(state).await?;
            }
            return Ok(mission.clone());
        }

        if patch.status == Some(TowerMissionStatus::Merged) {
            return Err(format!(
                "mission {id} cannot transition to merged through a generic mission patch — only TowerMerge records merged after its merge gate succeeds"
            ));
        }
        if !is_open_mission(mission)
            && (patch.status.is_some_and(|status| status != mission.status)
                || patch.blocker.is_some())
        {
            return Err(format!(
                "mission {id} is {} — merged and abandoned are terminal, so a generic mission patch cannot reopen it",
                mission.status.as_str()
            ));
        }

        if let Some(spawn_base) = patch.spawn_base.as_deref() {
            if caller_name != TOWER_NAME {
                return Err(format!(
                    "agent \"{caller_name}\" cannot record a mission spawn base — only the tower does"
                ));
            }
            state.missions[mission_index].spawn_base = Some(spawn_base.to_string());
        }

        if let Some(owner) = patch.owner.as_deref() {
            if caller_name != TOWER_NAME {
                return Err(format!(
                    "agent \"{caller_name}\" cannot assign mission ownership — only the tower sets owner"
                ));
            }
            state.missions[mission_index].owner = Some(owner.to_string());
        }

        if let Some(scope) = patch.scope.as_ref() {
            if caller_name != TOWER_NAME {
                return Err(format!(
                    "agent \"{caller_name}\" cannot change mission scope — only the tower widens a scope, and every change is logged"
                ));
            }
            let mut patched = state.missions[mission_index].clone();
            patched.scope = scope.clone();
            let others: Vec<TowerMission> = state
                .missions
                .iter()
                .filter(|mission| mission.id != id && is_open_mission(mission))
                .cloned()
                .collect();
            let mut by_id: HashMap<String, TowerMission> = state
                .missions
                .iter()
                .cloned()
                .map(|mission| (mission.id.clone(), mission))
                .chain(std::iter::once((id.to_string(), patched.clone())))
                .collect();
            add_auto_deps(&mut patched, &others, &mut by_id);
            let mut all = others;
            all.push(patched);
            assert_scopes_disjoint(&all)?;
            state.missions[mission_index].scope = scope.clone();
        }

        if let Some(status) = patch.status {
            if status == TowerMissionStatus::Abandoned && caller_name != TOWER_NAME {
                return Err(format!(
                    "agent \"{caller_name}\" cannot abandon mission {id} — abandoning releases the mission scope, so only the tower does it"
                ));
            }
            state.missions[mission_index].status = status;
        }
        if let Some(note) = patch.note.as_deref() {
            state.missions[mission_index].notes.push(note.to_string());
        }
        if let Some(blocker) = patch.blocker.as_deref() {
            state.missions[mission_index]
                .blockers
                .push(blocker.to_string());
            state.missions[mission_index].status = TowerMissionStatus::Blocked;
        }
        if patch.clear_blockers == Some(true) {
            state.missions[mission_index].blockers.clear();
        }
        if let Some(task_done) = patch.task_done.as_deref() {
            let task = state.missions[mission_index]
                .tasks
                .iter_mut()
                .find(|task| !task.done && !task.dropped && task.text.contains(task_done));
            match task {
                Some(task) => task.done = true,
                None => {
                    return Err(format!(
                        "mission {id} has no open task matching \"{task_done}\""
                    ));
                }
            }
        }
        let mut task_drop_log = None;
        if let Some(drop) = patch.task_drop.as_ref() {
            let reason = drop.reason.as_deref().unwrap_or("").trim();
            if reason.is_empty() {
                return Err(format!(
                    "dropping a task from mission {id} requires a reason — the drop is the escape hatch for legitimately descoped work, and the reason is recorded in the mission notes and the activity log for audit"
                ));
            }
            let task = state.missions[mission_index]
                .tasks
                .iter_mut()
                .find(|task| !task.done && !task.dropped && task.text.contains(&drop.text));
            let Some(task) = task else {
                return Err(format!(
                    "mission {id} has no open task matching \"{}\"",
                    drop.text
                ));
            };
            task.dropped = true;
            let log = format!("dropped task \"{}\": {reason}", task.text);
            task_drop_log = Some(log.clone());
            state.missions[mission_index].notes.push(log);
        }

        if patch.status == Some(TowerMissionStatus::Completed)
            && patch.blocker.is_none()
            && !options.completion_prepared
        {
            let mission_snapshot = state.missions[mission_index].clone();
            self.assert_completable(state, &mission_snapshot).await?;
            if caller_name != TOWER_NAME {
                self.assert_inbox_read(
                    state,
                    caller_name,
                    &mission_snapshot,
                    options.inbox_items.as_deref(),
                )
                .await?;
            }
        }
        if options.validate_only {
            return Ok(state.missions[mission_index].clone());
        }
        if state.missions[mission_index].kind == TowerMissionKind::Artifact
            && patch.status == Some(TowerMissionStatus::Completed)
            && patch.blocker.is_none()
        {
            if let Some(completion_tip) = options.completion_tip.as_deref() {
                state.missions[mission_index].completion_tip = Some(completion_tip.to_string());
                state.missions[mission_index].key_artifacts =
                    Some(options.completion_key_artifacts.clone().unwrap_or_default());
            } else if previous_status != TowerMissionStatus::Completed {
                state.missions[mission_index].completion_tip = None;
                state.missions[mission_index].key_artifacts = None;
            }
        }

        let mission = state.missions[mission_index].clone();
        self.save(state).await?;
        self.render_missions_index(state).await?;
        self.render_mission_file(&mission).await?;
        let task_tick_only = patch.task_done.is_some()
            && patch.status.is_none()
            && patch.note.is_none()
            && patch.blocker.is_none()
            && patch.clear_blockers.is_none()
            && patch.task_drop.is_none()
            && patch.owner.is_none()
            && patch.scope.is_none()
            && patch.spawn_base.is_none();
        if !task_tick_only && !options.silent {
            let status = patch.status.map(|status| status.as_str().to_string());
            let note = if patch.note.is_some() { Some("added") } else { None };
            let blocker = if patch.blocker.is_some() {
                Some("added")
            } else {
                None
            };
            let owner = patch.owner.as_deref();
            let scope = patch
                .scope
                .as_ref()
                .map(|scope| scope.join(","));
            let spawn_base = patch.spawn_base.as_deref();
            self.append_log(
                caller_name,
                "mission.update",
                &[
                    ("id", Some(id)),
                    ("status", status.as_deref()),
                    ("note", note),
                    ("blocker", blocker),
                    ("task_drop", task_drop_log.as_deref()),
                    ("owner", owner),
                    ("scope", scope.as_deref()),
                    ("spawn_base", spawn_base),
                ],
                None,
            )
            .await?;
        }
        Ok(mission)
    }

    async fn assert_completable(
        &self,
        state: &TowerState,
        mission: &TowerMission,
    ) -> Result<(), String> {
        let open: Vec<&TowerMissionTask> = mission
            .tasks
            .iter()
            .filter(|task| !task.done && !task.dropped)
            .collect();
        if !open.is_empty() {
            return Err(format!(
                "mission {} cannot transition to completed — {} open task(s): {}; tick finished tasks with task_done, or drop legitimately descoped ones with task_drop (a reason is mandatory and lands in the mission notes and the activity log)",
                mission.id,
                open.len(),
                open.iter()
                    .map(|task| format!("\"{}\"", task.text))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !mission.blockers.is_empty() {
            return Err(format!(
                "mission {} cannot transition to completed — {} blocker(s) remain: {}; clear or resolve them before completing",
                mission.id,
                mission.blockers.len(),
                mission.blockers.join(" | ")
            ));
        }
        if mission.kind == TowerMissionKind::Survey {
            if !branch_exists(&self.repo_root, &mission.branch).await? {
                return Ok(());
            }
            let base = self.diff_base(state, mission).await?;
            let changed = diff_name_only(&self.repo_root, &base, &mission.branch).await?;
            if !changed.is_empty() {
                return Err(format!(
                    "mission {} cannot transition to completed — it is a read-only survey but branch \"{}\" has {} changed file(s) vs \"{base}\": {}; move any changes worth keeping to a build mission and leave the survey branch zero-diff",
                    mission.id,
                    mission.branch,
                    changed.len(),
                    changed.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                ));
            }
            return Ok(());
        }
        if !branch_exists(&self.repo_root, &mission.branch).await? {
            return Err(if mission.kind == TowerMissionKind::Artifact {
                format!(
                    "mission {} cannot transition to completed — its branch \"{}\" does not exist, so no work has landed; an artifact mission must produce committed deliverables on its branch: spawn a worker to do the work, or have the tower abandon the mission (status=abandoned) if it is no longer needed",
                    mission.id, mission.branch
                )
            } else {
                format!(
                    "mission {} cannot transition to completed — its branch \"{}\" does not exist, so no work has landed; a build mission must produce a diff on its branch: spawn a worker to do the work, or have the tower abandon the mission (status=abandoned) if it is no longer needed",
                    mission.id, mission.branch
                )
            });
        }
        let base = self.diff_base(state, mission).await?;
        let changed = diff_name_only(&self.repo_root, &base, &mission.branch).await?;
        if changed.is_empty() {
            return Err(if mission.kind == TowerMissionKind::Artifact {
                format!(
                    "mission {} cannot transition to completed — branch \"{}\" has no changes vs \"{base}\"; an artifact mission must produce a committed diff on its branch: commit the deliverables there first, or if the work turned out unnecessary, have the tower abandon the mission (status=abandoned) instead",
                    mission.id, mission.branch
                )
            } else {
                format!(
                    "mission {} cannot transition to completed — branch \"{}\" has no changes vs \"{base}\"; a build mission must produce a diff on its branch: commit the work there first, or if the work turned out unnecessary, have the tower abandon the mission (status=abandoned) instead",
                    mission.id, mission.branch
                )
            });
        }
        Ok(())
    }

    async fn assert_inbox_read(
        &self,
        state: &TowerState,
        caller_name: &str,
        mission: &TowerMission,
        inbox_items: Option<&[TowerInboxItem]>,
    ) -> Result<(), String> {
        let items = match inbox_items {
            Some(items) => items.to_vec(),
            None => self.list_inbox_items().await?,
        };
        let unread = items
            .iter()
            .filter(|item| {
                self.inbox_item_visible_to(caller_name, item)
                    && self.is_inbox_item_unread(state, caller_name, item)
            })
            .count();
        if unread == 0 {
            return Ok(());
        }
        Err(format!(
            "mission {} cannot transition to completed — {} unread inbox message(s) for {caller_name}; call TowerInbox until no unread messages remain, incorporate anything new into the delivery, then retry status=completed",
            mission.id, unread
        ))
    }

    pub async fn diff_base(
        &self,
        state: &TowerState,
        mission: &TowerMission,
    ) -> Result<String, String> {
        if let Some(spawn_base) = mission.spawn_base.as_deref() {
            if is_ancestor(&self.repo_root, spawn_base, &mission.branch).await? {
                return Ok(spawn_base.to_string());
            }
        }
        Ok(state.base.clone())
    }

    fn inbox_item_visible_to(&self, caller_name: &str, item: &TowerInboxItem) -> bool {
        caller_name == TOWER_NAME || item.to == caller_name || item.to == BROADCAST_NAME
    }

    fn is_inbox_item_unread(
        &self,
        state: &TowerState,
        caller_name: &str,
        item: &TowerInboxItem,
    ) -> bool {
        let entry = if caller_name == TOWER_NAME {
            None
        } else {
            self.find_agent(state, caller_name)
        };
        let ack_ids = if caller_name == TOWER_NAME {
            state.inbox_ack_ids.as_deref()
        } else {
            entry
                .as_ref()
                .and_then(|agent| agent.inbox_ack_ids.as_deref())
        };
        if ack_ids.is_some_and(|ids| ids.iter().any(|id| id == &item.message_id)) {
            return false;
        }
        if caller_name == TOWER_NAME {
            return true;
        }
        let since = entry.and_then(|agent| {
            agent
                .last_inbox_read_at
                .clone()
                .or_else(|| Some(agent.spawned_at.clone()))
        });
        match since {
            None => true,
            Some(since) => item.sent_at.as_str() > since.as_str(),
        }
    }

    async fn list_unread_inbox_items(
        &self,
        state: &TowerState,
        caller_name: &str,
    ) -> Result<Vec<TowerInboxItem>, String> {
        Ok(self
            .list_inbox_items()
            .await?
            .into_iter()
            .filter(|item| {
                self.inbox_item_visible_to(caller_name, item)
                    && self.is_inbox_item_unread(state, caller_name, item)
            })
            .collect())
    }

    async fn list_inbox_items(&self) -> Result<Vec<TowerInboxItem>, String> {
        Ok(self.list_inbox_snapshot().await?.0)
    }

    async fn list_inbox_snapshot(
        &self,
    ) -> Result<(Vec<TowerInboxItem>, Vec<String>), String> {
        let mut files = match fs::read_dir(self.abs(INBOX_DIR)).await {
            Ok(mut entries) => {
                let mut files = Vec::new();
                while let Some(entry) = entries
                    .next_entry()
                    .await
                    .map_err(|error| error.to_string())?
                {
                    files.push(entry.file_name().to_string_lossy().to_string());
                }
                files
            }
            Err(_) => return Ok((Vec::new(), Vec::new())),
        };
        files.retain(|file| file.ends_with(".md"));
        let candidates = files.clone();
        let loaded = map_with_concurrency(
            candidates.clone(),
            TOWER_FILE_IO_CONCURRENCY,
            |_, file| async move {
                let rel = format!("{INBOX_DIR}/{file}");
                let text = fs::read_to_string(self.abs(&rel)).await.ok()?;
                let (fields, body) = parse_frontmatter(&text);
                if fields.get("type").map(String::as_str) != Some("inbox") {
                    return None;
                }
                Some(TowerInboxItem {
                    message_id: fields
                        .get("message_id")
                        .cloned()
                        .unwrap_or_else(|| rel.clone()),
                    file: rel,
                    from: fields
                        .get("from")
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                    to: fields.get("to").cloned().unwrap_or_default(),
                    subject: fields.get("subject").cloned().unwrap_or_default(),
                    sent_at: fields.get("sent_at").cloned().unwrap_or_default(),
                    scope: fields.get("scope").cloned(),
                    action: fields.get("action").cloned(),
                    consent_ref: fields.get("consent_ref").cloned(),
                    body,
                })
            },
        )
        .await;
        let mut items: Vec<TowerInboxItem> = loaded.into_iter().flatten().collect();
        items.sort_by(|a, b| {
            b.sent_at
                .cmp(&a.sent_at)
                .then_with(|| a.file.cmp(&b.file))
        });
        files.sort();
        Ok((items, files))
    }

    pub async fn read_inbox(
        &self,
        caller_name: &str,
        limit: usize,
    ) -> Result<Vec<TowerInboxItem>, String> {
        let mut items: Vec<TowerInboxItem> = self
            .list_inbox_items()
            .await?
            .into_iter()
            .filter(|item| self.inbox_item_visible_to(caller_name, item))
            .collect();
        items.truncate(limit.max(1));
        Ok(items)
    }

    pub async fn read_unread_inbox(
        &self,
        caller_name: &str,
        limit: usize,
    ) -> Result<Vec<TowerInboxItem>, String> {
        let state = self.load().await?;
        let mut items = self.list_unread_inbox_items(&state, caller_name).await?;
        items.truncate(limit.max(1));
        Ok(items)
    }

    pub async fn ack_inbox(
        &self,
        caller_name: &str,
        message_ids: &[String],
    ) -> Result<usize, String> {
        let caller_name = caller_name.to_string();
        let message_ids = message_ids.to_vec();
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            let index = state
                .roster
                .agents
                .iter()
                .position(|agent| agent.name == caller_name);
            let entry = index.and_then(|index| state.roster.agents.get(index).cloned());
            let current: Vec<String> = if caller_name == TOWER_NAME {
                state.inbox_ack_ids.clone().unwrap_or_default()
            } else {
                entry
                    .as_ref()
                    .and_then(|agent| agent.inbox_ack_ids.clone())
                    .unwrap_or_default()
            };
            let previous_size = current.len();
            let mut ack_ids: Vec<String> = current;
            for message_id in message_ids {
                if !ack_ids.contains(&message_id) {
                    ack_ids.push(message_id);
                }
            }
            let mut changed = false;
            if ack_ids.len() > previous_size {
                if caller_name == TOWER_NAME {
                    state.inbox_ack_ids = Some(ack_ids);
                    changed = true;
                } else if let Some(index) = index {
                    state.roster.agents[index].inbox_ack_ids = Some(ack_ids);
                    changed = true;
                }
            }
            if changed {
                self.save(&state).await?;
            }
            Ok(self
                .list_unread_inbox_items(&state, &caller_name)
                .await?
                .len())
        })
        .await
    }

    pub async fn send(
        &self,
        caller_name: &str,
        input: TowerSendInput,
    ) -> Result<String, String> {
        Ok(self.send_detailed(caller_name, input).await?.item.file)
    }

    pub async fn send_detailed(
        &self,
        caller_name: &str,
        input: TowerSendInput,
    ) -> Result<TowerSendResult, String> {
        let state = self.load().await?;
        let caller_name = caller_name.to_string();
        self.with_inbox_publication_lock(move || async move {
            self.send_detailed_snapshot(&caller_name, input, &state)
                .await
        })
        .await
    }

    async fn send_detailed_snapshot(
        &self,
        caller_name: &str,
        input: TowerSendInput,
        state: &TowerState,
    ) -> Result<TowerSendResult, String> {
        let to = input.to.trim().to_string();
        if to != TOWER_NAME
            && to != BROADCAST_NAME
            && self.find_agent(state, &to).is_none()
        {
            let mut known = vec![TOWER_NAME.to_string(), BROADCAST_NAME.to_string()];
            known.extend(state.roster.agents.iter().map(|agent| agent.name.clone()));
            return Err(format!(
                "unknown recipient \"{to}\" — address a roster agent, {TOWER_NAME}, or {BROADCAST_NAME} (known: {})",
                known.join(", ")
            ));
        }
        if to == caller_name {
            return Err("cannot send an inbox message to yourself".into());
        }

        let message_id = new_token();
        let sent_at = iso_now();
        let body = input.body.trim().to_string();
        let mut fields: Vec<(&str, String)> = vec![
            ("type", "inbox".into()),
            ("message_id", message_id.clone()),
            ("from", caller_name.to_string()),
            ("to", to.clone()),
            ("subject", input.subject.clone()),
            ("sent_at", sent_at.clone()),
        ];
        if let Some(scope) = input.scope.as_deref() {
            fields.push(("scope", scope.to_string()));
        }
        if let Some(action) = input.action.as_deref() {
            fields.push(("action", action.to_string()));
        }
        if let Some(consent_ref) = input.consent_ref.as_deref() {
            fields.push(("consent_ref", consent_ref.to_string()));
        }
        let rendered = render_frontmatter(
            &fields
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect::<Vec<_>>(),
        )?;
        let content = format!("{rendered}\n\n{body}\n");
        let base_name = inbox_file_name(caller_name, &to, &input.subject);
        let rel = self
            .write_unique(&format!("{INBOX_DIR}/{base_name}"), &content)
            .await?;
        let subject_slug = slugify(&input.subject, 60);
        let activity_log_error = match self
            .append_log(
                caller_name,
                "inbox.send",
                &[
                    ("to", Some(to.as_str())),
                    ("subject", Some(subject_slug.as_str())),
                ],
                Some(&rel),
            )
            .await
        {
            Ok(()) => None,
            Err(error) => Some(format!(
                "inbox message {rel} was stored, but appending its inbox.send activity log failed: {error}"
            )),
        };
        let caller = self.find_agent(state, caller_name);
        let recipient = if to == TOWER_NAME || to == BROADCAST_NAME {
            None
        } else {
            self.find_agent(state, &to)
        };
        let mission_id = caller
            .as_ref()
            .and_then(|agent| {
                agent
                    .mission_id
                    .clone()
                    .or_else(|| agent.review_mission_id.clone())
            })
            .or_else(|| {
                if caller_name == TOWER_NAME {
                    recipient.as_ref().and_then(|agent| {
                        agent
                            .mission_id
                            .clone()
                            .or_else(|| agent.review_mission_id.clone())
                    })
                } else {
                    None
                }
            });
        Ok(TowerSendResult {
            item: TowerInboxItem {
                message_id,
                file: rel,
                from: caller_name.to_string(),
                to,
                subject: input.subject,
                sent_at,
                scope: input.scope,
                action: input.action,
                consent_ref: input.consent_ref,
                body,
            },
            mission_id,
            activity_log_error,
        })
    }

    pub async fn complete(
        &self,
        caller_name: &str,
        report: &str,
        key_artifacts: &[String],
    ) -> Result<TowerCompleteResult, String> {
        let trimmed = report.trim();
        if trimmed.is_empty() {
            return Err("TowerComplete report must not be empty".into());
        }
        let normalized = normalize_artifact_paths(key_artifacts)?;
        let prepare_store = self.clone();
        let prepare_caller = caller_name.to_string();
        let prepare_report = trimmed.to_string();
        let prepare_artifacts = normalized.clone();
        let commit_store = self.clone();
        let commit_caller = caller_name.to_string();
        let commit_report = trimmed.to_string();
        let commit_artifacts = normalized;
        self.run_prepared(
            move || {
                let store = prepare_store.clone();
                let caller = prepare_caller.clone();
                let report = prepare_report.clone();
                let artifacts = prepare_artifacts.clone();
                async move {
                    let snapshot = store.load_snapshot().await?;
                    let inbox = store.list_inbox_snapshot().await?;
                    let mut validation_state = snapshot.state.clone();
                    store
                        .complete_locked(
                            &caller,
                            &report,
                            &artifacts,
                            &mut validation_state,
                            &MissionUpdateOptions {
                                validate_only: true,
                                inbox_items: Some(inbox.0.clone()),
                                ..Default::default()
                            },
                        )
                        .await?;
                    Ok((snapshot, inbox.1, inbox.0))
                }
            },
            move |(snapshot, inbox_files, inbox_items)| {
                let store = commit_store.clone();
                let caller = commit_caller.clone();
                let report = commit_report.clone();
                let artifacts = commit_artifacts.clone();
                async move {
                    let lock_store = store.clone();
                    lock_store
                        .with_completion_lock(move || async move {
                            let mut state = store
                                .assert_preparation_current(
                                    &snapshot,
                                    Some(inbox_files.as_slice()),
                                    None,
                                )
                                .await?;
                            store
                                .complete_locked(
                                    &caller,
                                    &report,
                                    &artifacts,
                                    &mut state,
                                    &MissionUpdateOptions {
                                        completion_prepared: true,
                                        inbox_items: Some(inbox_items),
                                        ..Default::default()
                                    },
                                )
                                .await
                        })
                        .await
                }
            },
        )
        .await
    }

    async fn complete_locked(
        &self,
        caller_name: &str,
        report: &str,
        key_artifacts: &[String],
        state: &mut TowerState,
        options: &MissionUpdateOptions,
    ) -> Result<TowerCompleteResult, String> {
        let caller = self.find_agent(state, caller_name);
        let owned_mission = caller
            .as_ref()
            .and_then(|agent| agent.mission_id.as_deref())
            .and_then(|id| state.missions.iter().find(|mission| mission.id == id))
            .cloned();
        if caller_name == TOWER_NAME
            || caller.as_ref().map(|agent| agent.kind) != Some(TowerAgentKind::Worker)
            || owned_mission.is_none()
        {
            if caller.as_ref().map(|agent| agent.kind) == Some(TowerAgentKind::Worker)
                && state.recovered_at.is_some()
            {
                let caller = caller.expect("worker caller");
                let recovered_at = state.recovered_at.clone().unwrap_or_default();
                return self
                    .complete_state_lost_locked(
                        &caller,
                        report,
                        key_artifacts,
                        state,
                        recovered_at,
                        options,
                    )
                    .await;
            }
            if let Some(agent) = caller.as_ref()
                && agent.kind == TowerAgentKind::Worker
                && let Some(mission_id) = agent.mission_id.as_deref()
            {
                return Err(format!("unknown mission \"{mission_id}\""));
            }
            return Err(format!(
                "agent \"{caller_name}\" cannot use TowerComplete — only the worker who owns a mission can complete it"
            ));
        }
        let before_mission = owned_mission.expect("owned mission");
        if before_mission.kind != TowerMissionKind::Artifact && !key_artifacts.is_empty() {
            return Err(
                "TowerComplete key_artifacts is only supported for artifact missions".into(),
            );
        }
        if before_mission.kind == TowerMissionKind::Artifact && key_artifacts.is_empty() {
            return Err(format!(
                "TowerComplete key_artifacts for artifact mission {} must name at least one committed key deliverable",
                before_mission.id
            ));
        }
        let was_completed = before_mission.status == TowerMissionStatus::Completed;
        let subject = match before_mission.kind {
            TowerMissionKind::Survey => "survey-summary",
            TowerMissionKind::Artifact => "acceptance-request",
            TowerMissionKind::Build => "review-request",
        };
        let mut completion_tip = None;
        let mut completion_base = None;
        let mut completion_changed: Vec<String> = Vec::new();
        if before_mission.kind == TowerMissionKind::Artifact {
            if !branch_exists(&self.repo_root, &before_mission.branch).await? {
                if options.completion_prepared {
                    return Err(format!(
                        "artifact mission {} cannot complete — its branch \"{}\" does not exist, so there are no committed deliverables to accept",
                        before_mission.id, before_mission.branch
                    ));
                }
            } else {
                let tip = branch_tip(&self.repo_root, &before_mission.branch).await?;
                let base = self.diff_base(state, &before_mission).await?;
                let changed =
                    diff_name_only(&self.repo_root, &base, &before_mission.branch).await?;
                if changed.is_empty() {
                    if options.completion_prepared {
                        return Err(format!(
                            "artifact mission {} cannot complete — branch \"{}\" has no changes vs \"{base}\"; commit the deliverables there first",
                            before_mission.id, before_mission.branch
                        ));
                    }
                } else {
                    let changed_set: HashSet<&str> =
                        changed.iter().map(String::as_str).collect();
                    let uncommitted: Vec<String> = key_artifacts
                        .iter()
                        .filter(|path| !changed_set.contains(path.as_str()))
                        .cloned()
                        .collect();
                    if !uncommitted.is_empty() {
                        return Err(format!(
                            "TowerComplete key_artifacts for artifact mission {} must name paths from the committed branch diff; not found: {}",
                            before_mission.id,
                            uncommitted.join(", ")
                        ));
                    }
                    let out_of_scope: Vec<String> = key_artifacts
                        .iter()
                        .filter(|path| {
                            !before_mission
                                .scope
                                .iter()
                                .any(|pattern| glob_matches(pattern, path))
                        })
                        .cloned()
                        .collect();
                    if !out_of_scope.is_empty() {
                        return Err(format!(
                            "TowerComplete key_artifacts for artifact mission {} must stay inside its scope ({}): {}",
                            before_mission.id,
                            before_mission.scope.join(", "),
                            out_of_scope.join(", ")
                        ));
                    }
                    let mut missing_at_tip: Vec<String> = Vec::new();
                    for path in key_artifacts {
                        let object = format!("{tip}:{path}");
                        let entry_type = git(
                            &self.repo_root,
                            &["cat-file", "-t", object.as_str()],
                        )
                        .await
                        .unwrap_or_default();
                        if entry_type.trim() != "blob" {
                            missing_at_tip.push(path.clone());
                        }
                    }
                    if !missing_at_tip.is_empty() {
                        let short = &tip[..tip.len().min(7)];
                        return Err(format!(
                            "TowerComplete key_artifacts for artifact mission {} must name files present at completion tip {short}; missing or not files: {}",
                            before_mission.id,
                            missing_at_tip.join(", ")
                        ));
                    }
                }
                completion_tip = Some(tip);
                completion_base = Some(base);
                completion_changed = changed;
            }
        }
        let completion_report = match (completion_tip.as_deref(), completion_base.as_deref()) {
            (Some(tip), Some(_)) if before_mission.kind == TowerMissionKind::Artifact => {
                artifact_acceptance_report(report, key_artifacts, &completion_changed, tip)
            }
            _ => report.to_string(),
        };
        let same_completion = before_mission.kind != TowerMissionKind::Artifact
            || (before_mission.completion_tip == completion_tip
                && same_artifact_paths(before_mission.key_artifacts.as_ref(), key_artifacts));
        if was_completed {
            if same_completion {
                if let Some(existing) = self
                    .find_completion_message(
                        caller_name,
                        &before_mission,
                        subject,
                        &completion_report,
                        options.inbox_items.as_deref(),
                    )
                    .await?
                {
                    let before_mission_id = before_mission.id.clone();
                    return Ok(TowerCompleteResult {
                        mission: Some(before_mission),
                        recovered_at: None,
                        message: Some(TowerSendResult {
                            item: existing,
                            mission_id: Some(before_mission_id),
                            activity_log_error: None,
                        }),
                        notification_created: false,
                        notification_error: None,
                        activity_log_error: None,
                    });
                }
            }
            if !options.completion_prepared {
                self.assert_completable(state, &before_mission).await?;
                self.assert_inbox_read(
                    state,
                    caller_name,
                    &before_mission,
                    options.inbox_items.as_deref(),
                )
                .await?;
            }
        }
        let mission = self
            .update_mission_locked(
                caller_name,
                &before_mission.id,
                &TowerMissionPatch {
                    status: Some(TowerMissionStatus::Completed),
                    ..Default::default()
                },
                state,
                &MissionUpdateOptions {
                    silent: options.silent,
                    validate_only: options.validate_only,
                    completion_prepared: options.completion_prepared,
                    completion_tip: completion_tip.clone(),
                    completion_key_artifacts: Some(key_artifacts.to_vec()),
                    force_save: false,
                    inbox_items: options.inbox_items.clone(),
                },
            )
            .await?;
        if options.validate_only {
            return Ok(TowerCompleteResult {
                mission: Some(mission),
                recovered_at: None,
                message: None,
                notification_created: false,
                notification_error: None,
                activity_log_error: None,
            });
        }
        let input = TowerSendInput {
            to: TOWER_NAME.into(),
            subject: subject.into(),
            body: completion_report.clone(),
            scope: Some(mission.id.clone()),
            action: Some("complete".into()),
            consent_ref: None,
        };
        match self.send_detailed_snapshot(caller_name, input, state).await {
            Ok(message) => Ok(TowerCompleteResult {
                mission: Some(mission),
                recovered_at: None,
                activity_log_error: message.activity_log_error.clone(),
                message: Some(message),
                notification_created: true,
                notification_error: None,
            }),
            Err(error) => {
                let existing = self
                    .find_completion_message(
                        caller_name,
                        &mission,
                        subject,
                        &completion_report,
                        None,
                    )
                    .await?;
                Ok(TowerCompleteResult {
                    mission: Some(mission.clone()),
                    recovered_at: None,
                    message: existing.map(|item| TowerSendResult {
                        item,
                        mission_id: Some(mission.id.clone()),
                        activity_log_error: None,
                    }),
                    notification_created: false,
                    notification_error: Some(format!(
                        "mission {} was persisted as completed, but storing its {subject} notification failed: {error} — retry with the same report to reuse an already stored notification instead of duplicating it",
                        mission.id
                    )),
                    activity_log_error: None,
                })
            }
        }
    }

    async fn complete_state_lost_locked(
        &self,
        caller: &TowerRosterEntry,
        report: &str,
        key_artifacts: &[String],
        state: &mut TowerState,
        recovered_at: String,
        options: &MissionUpdateOptions,
    ) -> Result<TowerCompleteResult, String> {
        let subject = "completion-report-state-lost";
        let completion_report = if key_artifacts.is_empty() {
            report.to_string()
        } else {
            format!(
                "Key artifacts:\n{}\n\n{report}",
                key_artifacts
                    .iter()
                    .map(|path| format!("- {path}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let items = match options.inbox_items.as_ref() {
            Some(items) => items.clone(),
            None => self.list_inbox_items().await?,
        };
        let existing = items.into_iter().find(|item| {
            item.from == caller.name
                && item.to == TOWER_NAME
                && item.subject == subject
                && item.action.as_deref() == Some("complete")
                && item.body == completion_report
        });
        if let Some(item) = existing {
            return Ok(TowerCompleteResult {
                mission: None,
                recovered_at: Some(recovered_at),
                message: Some(TowerSendResult {
                    item,
                    mission_id: None,
                    activity_log_error: None,
                }),
                notification_created: false,
                notification_error: None,
                activity_log_error: None,
            });
        }
        if options.validate_only {
            return Ok(TowerCompleteResult {
                mission: None,
                recovered_at: Some(recovered_at),
                message: None,
                notification_created: false,
                notification_error: None,
                activity_log_error: None,
            });
        }
        let input = TowerSendInput {
            to: TOWER_NAME.into(),
            subject: subject.into(),
            body: completion_report,
            scope: None,
            action: Some("complete".into()),
            consent_ref: None,
        };
        match self
            .send_detailed_snapshot(&caller.name, input, state)
            .await
        {
            Ok(message) => Ok(TowerCompleteResult {
                mission: None,
                recovered_at: Some(recovered_at),
                activity_log_error: message.activity_log_error.clone(),
                message: Some(message),
                notification_created: true,
                notification_error: None,
            }),
            Err(error) => Ok(TowerCompleteResult {
                mission: None,
                recovered_at: Some(recovered_at.clone()),
                message: None,
                notification_created: false,
                notification_error: Some(format!(
                    "the tower state was recovered at {recovered_at} after a loss, so no mission was marked completed — and storing the degraded completion report also failed: {error} — retry with the same report to deliver it"
                )),
                activity_log_error: None,
            }),
        }
    }

    async fn find_completion_message(
        &self,
        caller_name: &str,
        mission: &TowerMission,
        subject: &str,
        report: &str,
        inbox_items: Option<&[TowerInboxItem]>,
    ) -> Result<Option<TowerInboxItem>, String> {
        let items = match inbox_items {
            Some(items) => items.to_vec(),
            None => self.list_inbox_items().await?,
        };
        Ok(items.into_iter().find(|item| {
            item.from == caller_name
                && item.to == TOWER_NAME
                && item.subject == subject
                && item.scope.as_deref() == Some(mission.id.as_str())
                && item.action.as_deref() == Some("complete")
                && item.body == report
        }))
    }

    pub async fn file_finding(
        &self,
        caller_name: &str,
        input: TowerFindingInput,
    ) -> Result<String, String> {
        let state = self.load().await?;
        let caller = self.find_agent(&state, caller_name);
        let mission = caller
            .as_ref()
            .and_then(|agent| agent.mission_id.as_deref())
            .and_then(|id| state.missions.iter().find(|mission| mission.id == id));
        let mission_desc = match mission {
            Some(mission) => format!("{} — {}", mission.id, mission.title),
            None => "(none)".to_string(),
        };
        let not_fixed_reason = match mission {
            Some(mission) => format!(
                "This finding is outside the scope of mission {} ({}). Fixing it directly would violate scope isolation. Assigning to the control tower for routing.",
                mission.id,
                mission.scope.join(", ")
            ),
            None => "This finding is outside the reporting agent’s assignment. Assigning to the control tower for routing.".to_string(),
        };
        let lines = [
            format!("# Finding: {}", input.title),
            String::new(),
            format!("**Date**: {}", date_dash().replace('-', "")),
            format!("**Agent**: {caller_name}"),
            format!("**Type**: {}", input.finding_type.as_str()),
            format!(
                "**Severity**: {}",
                input.severity.unwrap_or_default().as_str()
            ),
            format!("**Mission**: {mission_desc}"),
            String::new(),
            "---".into(),
            String::new(),
            "## Summary".into(),
            input.summary.trim().into(),
            String::new(),
            "## Location".into(),
            input
                .location
                .clone()
                .unwrap_or_else(|| "(not specified)".into())
                .trim()
                .into(),
            String::new(),
            "## Details".into(),
            input.details.trim().into(),
            String::new(),
            "## Suggested Fix / Action".into(),
            input.suggested_fix.trim().into(),
            String::new(),
            "## Why Not Fixed Directly".into(),
            not_fixed_reason,
            String::new(),
            "---".into(),
            String::new(),
            format!("*Filed by tower agent {caller_name} via `{FINDINGS_DIR}/`*"),
            String::new(),
        ];
        let base_name = finding_file_name(
            caller_name,
            input.finding_type.as_str(),
            &input.title,
        );
        let rel = self
            .write_unique(&format!("{FINDINGS_DIR}/{base_name}"), &lines.join("\n"))
            .await?;
        let title_slug = slugify(&input.title, 60);
        self.append_log(
            caller_name,
            "finding.file",
            &[
                ("type", Some(input.finding_type.as_str())),
                ("slug", Some(title_slug.as_str())),
            ],
            Some(&rel),
        )
        .await?;
        Ok(rel)
    }
}

impl TowerStore {
    pub async fn submit_review(
        &self,
        caller_name: &str,
        input: TowerReviewInput,
    ) -> Result<TowerReviewResult, String> {
        let prepare_store = self.clone();
        let prepare_caller = caller_name.to_string();
        let prepare_input = input.clone();
        let commit_store = self.clone();
        let commit_caller = caller_name.to_string();
        let commit_input = input.clone();
        self.run_prepared(
            move || {
                let store = prepare_store.clone();
                let caller = prepare_caller.clone();
                let input = prepare_input.clone();
                async move { store.prepare_review(&caller, &input).await }
            },
            move |preparation| {
                let store = commit_store.clone();
                let caller = commit_caller.clone();
                let input = commit_input.clone();
                async move {
                    let ReviewPreparation {
                        snapshot,
                        review_files,
                        inbox_files,
                        mut options,
                    } = preparation;
                    let lock_store = store.clone();
                    lock_store
                        .with_state_lock(move || async move {
                            let mut state = store
                                .assert_preparation_current(
                                    &snapshot,
                                    Some(inbox_files.as_slice()),
                                    Some(review_files.as_slice()),
                                )
                                .await?;
                            if branch_tip(&store.repo_root, &input.target).await?
                                != options.reviewed_commit
                            {
                                return Err(PREPARATION_CONFLICT.into());
                            }
                            store
                                .submit_review_locked(&caller, &input, &mut state, &mut options)
                                .await
                        })
                        .await
                }
            },
        )
        .await
    }

    async fn prepare_review(
        &self,
        caller_name: &str,
        input: &TowerReviewInput,
    ) -> Result<ReviewPreparation, String> {
        let snapshot = self.load_snapshot().await?;
        self.assert_review_submission(caller_name, input, &snapshot.state)?;
        let review_files = self.review_file_names().await;
        let existing = self.reviews_for(&input.target).await?;
        let next_review_seq = self.next_review_seq().await?;
        let reviewed_commit = branch_tip(&self.repo_root, &input.target).await?;
        let inbox = self.list_inbox_snapshot().await?;
        let mut options = ReviewLockedOptions {
            existing,
            next_review_seq,
            reviewed_commit,
            inbox_items: inbox.0,
            activity_log_refs: HashMap::new(),
            validate_only: false,
        };
        let mut validation_options = ReviewLockedOptions {
            existing: options.existing.clone(),
            next_review_seq: options.next_review_seq,
            reviewed_commit: options.reviewed_commit.clone(),
            inbox_items: options.inbox_items.clone(),
            activity_log_refs: HashMap::new(),
            validate_only: true,
        };
        let mut validation_state = snapshot.state.clone();
        self.submit_review_locked(
            caller_name,
            input,
            &mut validation_state,
            &mut validation_options,
        )
        .await?;
        options.validate_only = false;
        Ok(ReviewPreparation {
            snapshot,
            review_files,
            inbox_files: inbox.1,
            options,
        })
    }

    fn assert_review_submission(
        &self,
        caller_name: &str,
        input: &TowerReviewInput,
        state: &TowerState,
    ) -> Result<Option<TowerRosterEntry>, String> {
        let caller = if caller_name == TOWER_NAME {
            None
        } else {
            let caller = self.find_agent(state, caller_name);
            if caller.as_ref().map(|agent| agent.kind) != Some(TowerAgentKind::Reviewer)
                || caller.as_ref().and_then(|agent| agent.review_target.as_deref())
                    != Some(input.target.as_str())
            {
                return Err(format!(
                    "agent \"{caller_name}\" is not an assigned reviewer for \"{}\"",
                    input.target
                ));
            }
            caller
        };
        let status_regex = regex::Regex::new(r"^(clean|p[12]-\d+items)$")
            .map_err(|error| error.to_string())?;
        if !status_regex.is_match(&input.status) {
            return Err(format!(
                "review status must be clean | p1-Nitems | p2-Nitems, got \"{}\"",
                input.status
            ));
        }
        if !["merge", "fix-then-merge", "hold"].contains(&input.merge.as_str()) {
            return Err(format!(
                "review merge verdict must be merge | fix-then-merge | hold, got \"{}\"",
                input.merge
            ));
        }
        Ok(caller)
    }

    async fn submit_review_locked(
        &self,
        caller_name: &str,
        input: &TowerReviewInput,
        state: &mut TowerState,
        options: &mut ReviewLockedOptions,
    ) -> Result<TowerReviewResult, String> {
        let caller_entry = self.assert_review_submission(caller_name, input, state)?;
        let reviewed_commit = options.reviewed_commit.clone();
        let review_mission_id = match caller_entry.as_ref() {
            None => resolve_mission_by_branch(state, &input.target).map(|mission| mission.id),
            Some(entry) => entry.review_mission_id.clone(),
        };
        let review_mission = match review_mission_id.as_deref() {
            Some(id) => state
                .missions
                .iter()
                .find(|mission| mission.id == id)
                .cloned(),
            None => resolve_mission_by_branch(state, &input.target),
        };
        let artifact_tower_feedback = caller_name == TOWER_NAME
            && review_mission
                .as_ref()
                .is_some_and(|mission| mission.kind == TowerMissionKind::Artifact);
        if artifact_tower_feedback
            && input.status == "clean"
            && input.merge == "merge"
            && let Some(mission) = review_mission.as_ref()
        {
            let Some(completion_tip) = mission.completion_tip.as_deref() else {
                return Err(format!(
                    "artifact mission {} has no completion tip recorded — its acceptance-request cannot be bound to an exact branch tip. Ask the worker to run TowerComplete at the current tip with key_artifacts, present those key artifacts and the whole-tip approval scope for explicit human approval, then record TowerReview again.",
                    mission.id
                ));
            };
            let Some(acceptance_tip) = input.acceptance_tip.as_deref() else {
                return Err(format!(
                    "artifact mission {} requires acceptance_tip from the TowerComplete acceptance-request the human approved — present the latest request and pass its completion tip explicitly",
                    mission.id
                ));
            };
            if acceptance_tip != completion_tip {
                return Err(format!(
                    "artifact acceptance references {}, but the latest acceptance-request for mission {} covers {} — the approval is tied to a different presentation; present the latest request and pass its acceptance_tip",
                    &acceptance_tip[..acceptance_tip.len().min(7)],
                    mission.id,
                    &completion_tip[..completion_tip.len().min(7)]
                ));
            }
            if reviewed_commit != completion_tip {
                return Err(format!(
                    "branch \"{}\" advanced after the artifact acceptance-request (completion {}, current {}) — the user has not approved the delivery at the current tip. Ask the worker to run TowerComplete at the current tip with key_artifacts, present those key artifacts and the whole-tip approval scope for fresh explicit human approval, then record TowerReview again.",
                    input.target,
                    &completion_tip[..completion_tip.len().min(7)],
                    &reviewed_commit[..reviewed_commit.len().min(7)]
                ));
            }
        }

        let mut activity_log_errors: Vec<String> = Vec::new();
        let inbox_items = options.inbox_items.clone();
        let mut review = self
            .find_review_recovery(
                state,
                caller_name,
                input,
                &options.existing,
                &reviewed_commit,
                review_mission.as_ref(),
                &inbox_items,
                &mut options.activity_log_refs,
            )
            .await?;
        let my_rounds = options
            .existing
            .iter()
            .filter(|candidate| candidate.reviewer == caller_name)
            .count() as u32;
        if !artifact_tower_feedback && review.is_none() && my_rounds >= MAX_REVIEW_ROUNDS {
            return Err(format!(
                "branch \"{}\" has already been through {MAX_REVIEW_ROUNDS} review rounds by \"{caller_name}\" — the rework loop is not converging, so another round from the same reviewer is refused; redirect instead: reassign the work (spawn a different worker or a fresh reviewer), split the mission into smaller pieces, or descope it (TowerMission status=abandoned)",
                input.target
            ));
        }
        if options.validate_only {
            if review.is_none() {
                let round = my_rounds + 1;
                review = Some(super::types::TowerReviewInfo {
                    reviewer: caller_name.to_string(),
                    target: input.target.clone(),
                    round,
                    status: input.status.clone(),
                    merge: input.merge.clone(),
                    reviewed_commit: reviewed_commit.clone(),
                    date: date_dash(),
                    file: format!(
                        "{REVIEWS_DIR}/{}",
                        review_file_name(&input.target, caller_name, round)
                    ),
                    mtime_ms: 0,
                    seq: Some(options.next_review_seq),
                    mission: review_mission_id,
                });
            }
            let review = review.expect("review synthesized");
            let (_, notification_errors) = self
                .store_review_messages(
                    state,
                    caller_name,
                    input,
                    &review,
                    review_mission.as_ref(),
                    &mut activity_log_errors,
                    &inbox_items,
                    &mut options.activity_log_refs,
                    true,
                )
                .await?;
            let _ = notification_errors;
            return Ok(TowerReviewResult {
                review,
                stored_messages: Vec::new(),
                notification_error: None,
                activity_log_error: None,
            });
        }

        if let Some(existing) = review.as_ref() {
            let log_present = self
                .has_cached_activity_log_ref(
                    "review.write",
                    &existing.file,
                    &mut options.activity_log_refs,
                )
                .await?;
            if !log_present {
                let round = existing.round.to_string();
                let function_short = &reviewed_commit[..reviewed_commit.len().min(7)];
                match self
                    .append_log(
                        caller_name,
                        "review.write",
                        &[
                            ("target", Some(input.target.as_str())),
                            ("round", Some(round.as_str())),
                            ("verdict", Some(existing.status.as_str())),
                            ("reviewed", Some(function_short)),
                        ],
                        Some(&existing.file),
                    )
                    .await
                {
                    Ok(()) => {}
                    Err(error) => activity_log_errors.push(format!(
                        "review {} was stored, but appending its review.write activity log failed: {error}",
                        existing.file
                    )),
                }
            }
        }

        if review.is_none() {
            let round = my_rounds + 1;
            let seq = options.next_review_seq;
            let date = date_dash();
            let mut fields: Vec<(&str, String)> = vec![
                ("date", date.clone()),
                ("reviewer", caller_name.to_string()),
                ("target", input.target.clone()),
                ("round", round.to_string()),
                ("seq", seq.to_string()),
                ("status", input.status.clone()),
                ("merge", input.merge.clone()),
                ("reviewed_commit", reviewed_commit.clone()),
            ];
            if let Some(mission) = review_mission_id.clone() {
                fields.push(("mission", mission));
            }
            let rendered = render_frontmatter(
                &fields
                    .iter()
                    .map(|(key, value)| (*key, value.as_str()))
                    .collect::<Vec<_>>(),
            )?;
            let content = format!("{rendered}\n\n{}\n", review_document_body(input));
            let rel = self
                .write_unique(
                    &format!(
                        "{REVIEWS_DIR}/{}",
                        review_file_name(&input.target, caller_name, round)
                    ),
                    &content,
                )
                .await?;
            let mtime_ms = fs::metadata(self.abs(&rel))
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| {
                    modified
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                        .map(|duration| duration.as_millis() as u64)
                })
                .unwrap_or(0);
            review = Some(super::types::TowerReviewInfo {
                reviewer: caller_name.to_string(),
                target: input.target.clone(),
                round,
                status: input.status.clone(),
                merge: input.merge.clone(),
                reviewed_commit: reviewed_commit.clone(),
                date,
                file: rel.clone(),
                mtime_ms,
                seq: Some(seq),
                mission: review_mission_id.clone(),
            });
            let round_text = round.to_string();
            let short = &reviewed_commit[..reviewed_commit.len().min(7)];
            match self
                .append_log(
                    caller_name,
                    "review.write",
                    &[
                        ("target", Some(input.target.as_str())),
                        ("round", Some(round_text.as_str())),
                        ("verdict", Some(input.status.as_str())),
                        ("reviewed", Some(short)),
                    ],
                    Some(&rel),
                )
                .await
            {
                Ok(()) => {}
                Err(error) => activity_log_errors.push(format!(
                    "review {rel} was stored, but appending its review.write activity log failed: {error}"
                )),
            }
        }

        let review = review.expect("review exists");
        if input.status != "clean"
            && let Some(mission) = review_mission.as_ref()
            && mission.status == TowerMissionStatus::Completed
            && let Some(index) = state.missions.iter().position(|item| item.id == mission.id)
        {
            state.missions[index].status = TowerMissionStatus::Active;
            let mission = state.missions[index].clone();
            self.save(state).await?;
            self.render_missions_index(state).await?;
            self.render_mission_file(&mission).await?;
            let mission_ref = format!(
                "{MISSIONS_DIR}/{}",
                mission_file_name(&mission.id, &mission.slug)
            );
            match self
                .append_log(
                    caller_name,
                    "mission.rework",
                    &[
                        ("id", Some(mission.id.as_str())),
                        ("verdict", Some(input.status.as_str())),
                    ],
                    Some(&mission_ref),
                )
                .await
            {
                Ok(()) => {}
                Err(error) => activity_log_errors.push(format!(
                    "mission {} was marked active, but appending mission.rework activity log failed: {error}",
                    mission.id
                )),
            }
        }

        let (stored_messages, notification_errors) = self
            .store_review_messages(
                state,
                caller_name,
                input,
                &review,
                review_mission.as_ref(),
                &mut activity_log_errors,
                &inbox_items,
                &mut options.activity_log_refs,
                false,
            )
            .await?;
        Ok(TowerReviewResult {
            notification_error: if notification_errors.is_empty() {
                None
            } else {
                Some(format!(
                    "review {} was stored, but storing review-result notification(s) failed: {}",
                    review.file,
                    notification_errors.join(" | ")
                ))
            },
            activity_log_error: if activity_log_errors.is_empty() {
                None
            } else {
                Some(activity_log_errors.join(" | "))
            },
            review,
            stored_messages,
        })
    }

    async fn find_review_recovery(
        &self,
        state: &TowerState,
        caller_name: &str,
        input: &TowerReviewInput,
        existing: &[super::types::TowerReviewInfo],
        reviewed_commit: &str,
        review_mission: Option<&TowerMission>,
        inbox_items: &[TowerInboxItem],
        activity_log_refs: &mut HashMap<String, bool>,
    ) -> Result<Option<super::types::TowerReviewInfo>, String> {
        let expected_body = review_document_body(input);
        let recipients = self.review_recipients(state, caller_name, input, review_mission);
        for candidate in existing.iter().rev() {
            if candidate.reviewer != caller_name
                || candidate.reviewed_commit != reviewed_commit
                || candidate.status != input.status
                || candidate.merge != input.merge
            {
                continue;
            }
            let text = fs::read_to_string(self.abs(&candidate.file))
                .await
                .map_err(|error| error.to_string())?;
            let (_, body) = parse_frontmatter(&text);
            if body != expected_body {
                continue;
            }
            if !self
                .has_cached_activity_log_ref(
                    "review.write",
                    &candidate.file,
                    activity_log_refs,
                )
                .await?
            {
                return Ok(Some(candidate.clone()));
            }
            let mut needs_recovery = false;
            for recipient in &recipients {
                let message = inbox_items.iter().find(|item| {
                    self.is_review_result_message(
                        item,
                        caller_name,
                        recipient,
                        candidate,
                        review_mission,
                        input,
                    )
                });
                let Some(message) = message else {
                    needs_recovery = true;
                    break;
                };
                if !self
                    .has_cached_activity_log_ref(
                        "inbox.send",
                        &message.file,
                        activity_log_refs,
                    )
                    .await?
                {
                    needs_recovery = true;
                    break;
                }
            }
            if needs_recovery {
                return Ok(Some(candidate.clone()));
            }
            return Ok(None);
        }
        Ok(None)
    }

    async fn store_review_messages(
        &self,
        state: &TowerState,
        caller_name: &str,
        input: &TowerReviewInput,
        review: &super::types::TowerReviewInfo,
        review_mission: Option<&TowerMission>,
        activity_log_errors: &mut Vec<String>,
        inbox_items: &[TowerInboxItem],
        activity_log_refs: &mut HashMap<String, bool>,
        validate_only: bool,
    ) -> Result<(Vec<TowerSendResult>, Vec<String>), String> {
        let recipients = self.review_recipients(state, caller_name, input, review_mission);
        let body = self.review_notification_body(review, review_mission, input);
        let scope = review_mission
            .map(|mission| mission.id.clone())
            .or_else(|| review.mission.clone());
        let mut stored_messages = Vec::new();
        let mut notification_errors = Vec::new();
        for recipient in recipients {
            let existing = inbox_items.iter().find(|item| {
                self.is_review_result_message(
                    item,
                    caller_name,
                    &recipient,
                    review,
                    review_mission,
                    input,
                )
            });
            if let Some(existing) = existing {
                stored_messages.push(TowerSendResult {
                    item: existing.clone(),
                    mission_id: review_mission
                        .map(|mission| mission.id.clone())
                        .or_else(|| review.mission.clone()),
                    activity_log_error: None,
                });
                let log_present = self
                    .has_cached_activity_log_ref(
                        "inbox.send",
                        &existing.file,
                        activity_log_refs,
                    )
                    .await?;
                if validate_only {
                    continue;
                }
                if !log_present {
                    let subject_slug = slugify("review-result", 60);
                    match self
                        .append_log(
                            caller_name,
                            "inbox.send",
                            &[
                                ("to", Some(recipient.as_str())),
                                ("subject", Some(subject_slug.as_str())),
                            ],
                            Some(&existing.file),
                        )
                        .await
                    {
                        Ok(()) => {}
                        Err(error) => activity_log_errors.push(format!(
                            "inbox message {} was stored, but appending its inbox.send activity log failed: {error}",
                            existing.file
                        )),
                    }
                }
                continue;
            }
            if validate_only {
                continue;
            }
            let send_input = TowerSendInput {
                to: recipient.clone(),
                subject: "review-result".into(),
                body: body.clone(),
                scope: scope.clone(),
                action: Some("review-result".into()),
                consent_ref: None,
            };
            match self
                .send_detailed_snapshot(caller_name, send_input, state)
                .await
            {
                Ok(sent) => {
                    if let Some(error) = sent.activity_log_error.clone() {
                        activity_log_errors.push(error);
                    }
                    stored_messages.push(sent);
                }
                Err(error) => notification_errors.push(format!("{recipient}: {error}")),
            }
        }
        Ok((stored_messages, notification_errors))
    }

    fn review_recipients(
        &self,
        state: &TowerState,
        caller_name: &str,
        input: &TowerReviewInput,
        review_mission: Option<&TowerMission>,
    ) -> Vec<String> {
        let owner = review_mission
            .filter(|mission| is_open_mission(mission))
            .and_then(|mission| mission.owner.as_deref())
            .and_then(|owner| {
                state.roster.agents.iter().find(|agent| {
                    agent.name == owner
                        && agent.kind == TowerAgentKind::Worker
                        && agent.mission_id.as_deref() == review_mission.map(|mission| mission.id.as_str())
                        && agent.died_at.is_none()
                })
            });
        let mut recipients = Vec::new();
        if input.status != "clean"
            && let Some(owner) = owner
            && owner.name != caller_name
        {
            recipients.push(owner.name.clone());
        }
        if caller_name != TOWER_NAME {
            recipients.push(TOWER_NAME.to_string());
        }
        recipients
    }

    fn review_notification_body(
        &self,
        review: &super::types::TowerReviewInfo,
        review_mission: Option<&TowerMission>,
        input: &TowerReviewInput,
    ) -> String {
        [
            format!("file: {}", review.file),
            format!("target: {}", review.target),
            format!(
                "mission: {}",
                review_mission
                    .map(|mission| mission.id.as_str())
                    .or(review.mission.as_deref())
                    .unwrap_or("(none)")
            ),
            format!("round: {}", review.round),
            format!("verdict: {}", input.status),
            format!("merge recommendation: {}", input.merge),
            format!("reviewedCommit: {}", review.reviewed_commit),
            format!(
                "owner: {}",
                review_mission
                    .and_then(|mission| mission.owner.as_deref())
                    .unwrap_or("(none)")
            ),
        ]
        .join("\n")
    }

    fn is_review_result_message(
        &self,
        item: &TowerInboxItem,
        caller_name: &str,
        recipient: &str,
        review: &super::types::TowerReviewInfo,
        review_mission: Option<&TowerMission>,
        input: &TowerReviewInput,
    ) -> bool {
        item.from == caller_name
            && item.to.as_str() == recipient
            && item.subject == "review-result"
            && item.scope.as_deref()
                == review_mission
                    .map(|mission| mission.id.as_str())
                    .or(review.mission.as_deref())
            && item.action.as_deref() == Some("review-result")
            && item.body == self.review_notification_body(review, review_mission, input)
    }

    async fn has_activity_log_ref(&self, action: &str, rel: &str) -> Result<bool, String> {
        let content = match fs::read(self.abs(ACTIVITY_LOG)).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.to_string()),
        };
        let action_needle = format!(" {action} ");
        let ref_needle = format!("ref={rel}");
        for line in content.split(|byte| *byte == b'\n') {
            if line
                .windows(action_needle.len())
                .any(|window| window == action_needle.as_bytes())
                && line.ends_with(ref_needle.as_bytes())
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn has_cached_activity_log_ref(
        &self,
        action: &str,
        rel: &str,
        cache: &mut HashMap<String, bool>,
    ) -> Result<bool, String> {
        let key = format!("{action}\0{rel}");
        if let Some(cached) = cache.get(&key) {
            return Ok(*cached);
        }
        let present = self.has_activity_log_ref(action, rel).await?;
        cache.insert(key, present);
        Ok(present)
    }

    pub async fn reviews_for(
        &self,
        target: &str,
    ) -> Result<Vec<super::types::TowerReviewInfo>, String> {
        let mut files = match fs::read_dir(self.abs(REVIEWS_DIR)).await {
            Ok(mut entries) => {
                let mut files = Vec::new();
                while let Some(entry) = entries
                    .next_entry()
                    .await
                    .map_err(|error| error.to_string())?
                {
                    files.push(entry.file_name().to_string_lossy().to_string());
                }
                files
            }
            Err(_) => return Ok(Vec::new()),
        };
        let prefix = format!("review-{}-", target_slug(target));
        files.retain(|file| file.starts_with(&prefix) && file.ends_with(".md"));
        let loaded = map_with_concurrency(
            files,
            TOWER_FILE_IO_CONCURRENCY,
            |_, file| async move {
                let rel = format!("{REVIEWS_DIR}/{file}");
                let text = read_frontmatter_prefix(self.abs(&rel)).await.ok()?;
                let (fields, _) = parse_frontmatter(&text);
                let round = fields.get("round")?.parse::<u32>().ok()?;
                let seq = fields.get("seq").and_then(|value| value.parse::<u64>().ok());
                let mtime_ms = fs::metadata(self.abs(&rel))
                    .await
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| {
                        modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()
                            .map(|duration| duration.as_millis() as u64)
                    })
                    .unwrap_or(0);
                Some(super::types::TowerReviewInfo {
                    reviewer: fields
                        .get("reviewer")
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                    target: fields
                        .get("target")
                        .cloned()
                        .unwrap_or_else(|| target.to_string()),
                    round,
                    status: fields.get("status").cloned().unwrap_or_default(),
                    merge: fields.get("merge").cloned().unwrap_or_default(),
                    reviewed_commit: fields.get("reviewed_commit").cloned().unwrap_or_default(),
                    date: fields.get("date").cloned().unwrap_or_default(),
                    file: rel,
                    mtime_ms,
                    seq,
                    mission: fields.get("mission").cloned(),
                })
            },
        )
        .await;
        let mut reviews: Vec<super::types::TowerReviewInfo> =
            loaded.into_iter().flatten().collect();
        reviews.sort_by(|a, b| {
            let a_seq = a.seq.map(|seq| seq as i128).unwrap_or(-1);
            let b_seq = b.seq.map(|seq| seq as i128).unwrap_or(-1);
            a_seq
                .cmp(&b_seq)
                .then_with(|| a.mtime_ms.cmp(&b.mtime_ms))
                .then_with(|| a.round.cmp(&b.round))
                .then_with(|| a.file.cmp(&b.file))
        });
        Ok(reviews)
    }

    pub async fn latest_review(
        &self,
        target: &str,
    ) -> Result<Option<super::types::TowerReviewInfo>, String> {
        let mut reviews = self.reviews_for(target).await?;
        Ok(reviews.pop())
    }

    async fn review_file_names(&self) -> Vec<String> {
        let mut files = match fs::read_dir(self.abs(REVIEWS_DIR)).await {
            Ok(mut entries) => {
                let mut files = Vec::new();
                while let Some(entry) = entries.next_entry().await.ok().flatten() {
                    files.push(entry.file_name().to_string_lossy().to_string());
                }
                files
            }
            Err(_) => return Vec::new(),
        };
        files.retain(|file| file.starts_with("review-") && file.ends_with(".md"));
        files.sort();
        files
    }

    async fn next_review_seq(&self) -> Result<u64, String> {
        let candidates = self.review_file_names().await;
        let sequences = map_with_concurrency(
            candidates,
            TOWER_FILE_IO_CONCURRENCY,
            |_, file| async move {
                let rel = format!("{REVIEWS_DIR}/{file}");
                let text = read_frontmatter_prefix(self.abs(&rel)).await.ok()?;
                parse_frontmatter(&text)
                    .0
                    .get("seq")
                    .and_then(|value| value.parse::<u64>().ok())
            },
        )
        .await;
        Ok(sequences.into_iter().flatten().max().unwrap_or(0) + 1)
    }
}

impl TowerStore {
    pub async fn merge(
        &self,
        branch: &str,
        runtime: TowerMissionRuntimeOptions,
    ) -> Result<TowerMergeResult, String> {
        let branch = branch.to_string();
        self.with_state_lock(move || async move {
            self.merge_locked(&branch, &runtime).await
        })
        .await
    }

    async fn merge_locked(
        &self,
        branch: &str,
        runtime: &TowerMissionRuntimeOptions,
    ) -> Result<TowerMergeResult, String> {
        let mut state = self.load().await?;
        let Some(mut mission) = resolve_mission_by_branch(&state, branch) else {
            let closed: Vec<&TowerMission> = state
                .missions
                .iter()
                .filter(|candidate| candidate.branch == branch)
                .collect();
            if !closed.is_empty() {
                let listed = closed
                    .iter()
                    .map(|candidate| format!("{} ({})", candidate.id, candidate.status.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.append_log(
                    TOWER_NAME,
                    "merge.blocked",
                    &[
                        ("branch", Some(branch)),
                        ("reason", Some("branch-owned-by-closed-missions")),
                    ],
                    None,
                )
                .await?;
                return Err(format!(
                    "merge blocked: branch \"{branch}\" resolves only to closed mission(s) {listed} — TowerMerge never flips a closed mission's status; re-plan the work under a new title if it should land"
                ));
            }
            return Err(format!("no tower mission owns branch \"{branch}\""));
        };

        let gate = self.mission_gate(&state, &mission, runtime).await?;
        if !gate.ready {
            let primary = gate.blocks.first().expect("a closed gate has a block");
            self.append_log(
                TOWER_NAME,
                "merge.blocked",
                &[
                    ("branch", Some(branch)),
                    ("reason", Some(primary.reason)),
                ],
                None,
            )
            .await?;
            return Err(primary.message.clone());
        }
        let observation = gate
            .observations
            .branch
            .clone()
            .expect("a ready gate always observes its branch");
        let evaluated_branch_tip = observation.tip.clone();

        if gate.noop {
            let current_branch_tip = branch_tip(&self.repo_root, branch).await?;
            if current_branch_tip != evaluated_branch_tip {
                let evaluated_short = &evaluated_branch_tip[..evaluated_branch_tip.len().min(7)];
                let current_short = &current_branch_tip[..current_branch_tip.len().min(7)];
                self.append_log(
                    TOWER_NAME,
                    "merge.advanced",
                    &[
                        ("branch", Some(branch)),
                        ("evaluated", Some(evaluated_short)),
                        ("current", Some(current_short)),
                    ],
                    None,
                )
                .await?;
                return Ok(TowerMergeResult {
                    status: TowerMergeStatus::Advanced,
                    merge_commit: gate.observations.head_tip.clone(),
                    evaluated_branch_tip,
                    current_branch_tip,
                    conflicts_with: Vec::new(),
                    noop: true,
                });
            }
            let index = state
                .missions
                .iter()
                .position(|candidate| candidate.id == mission.id)
                .expect("mission was resolved from state");
            state.missions[index].status = TowerMissionStatus::Merged;
            mission = state.missions[index].clone();
            self.save(&state).await?;
            self.render_missions_index(&state).await?;
            self.render_mission_file(&mission).await?;
            self.append_log(
                TOWER_NAME,
                "merge.noop",
                &[("branch", Some(branch)), ("kind", Some("survey"))],
                None,
            )
            .await?;
            return Ok(TowerMergeResult {
                status: TowerMergeStatus::Noop,
                merge_commit: gate.observations.head_tip.clone(),
                evaluated_branch_tip,
                current_branch_tip,
                conflicts_with: Vec::new(),
                noop: true,
            });
        }

        let merge_commit = merge_no_ff_with_message(
            &self.repo_root,
            &evaluated_branch_tip,
            &format!("Merge branch '{branch}'"),
        )
        .await?;
        let head_tip = branch_tip(&self.repo_root, "HEAD").await?;
        if !is_ancestor(&self.repo_root, &evaluated_branch_tip, &head_tip).await? {
            return Err(format!(
                "recovery-required: merge of {branch} returned but HEAD {head_tip} does not contain the evaluated commit {evaluated_branch_tip} — mission {} was not marked merged; inspect the base checkout before retrying",
                mission.id
            ));
        }
        let current_branch_tip = branch_tip(&self.repo_root, branch).await?;
        if current_branch_tip != evaluated_branch_tip {
            let evaluated_short = &evaluated_branch_tip[..evaluated_branch_tip.len().min(7)];
            let current_short = &current_branch_tip[..current_branch_tip.len().min(7)];
            let merge_short = &merge_commit[..merge_commit.len().min(7)];
            self.append_log(
                TOWER_NAME,
                "merge.advanced",
                &[
                    ("branch", Some(branch)),
                    ("evaluated", Some(evaluated_short)),
                    ("current", Some(current_short)),
                    ("merge_commit", Some(merge_short)),
                ],
                None,
            )
            .await?;
            return Ok(TowerMergeResult {
                status: TowerMergeStatus::Advanced,
                merge_commit,
                evaluated_branch_tip,
                current_branch_tip,
                conflicts_with: Vec::new(),
                noop: false,
            });
        }

        let changed_set: HashSet<String> = observation.changed_files.iter().cloned().collect();
        let other_missions: Vec<TowerMission> = state
            .missions
            .iter()
            .filter(|other| other.branch != branch && is_open_mission(other))
            .cloned()
            .collect();
        let conflicts = map_with_concurrency(
            other_missions,
            TOWER_GIT_CONCURRENCY,
            |_, other| {
                let changed_set = changed_set.clone();
                let state = &state;
                async move {
                    if !branch_exists(&self.repo_root, &other.branch).await? {
                        return Ok(None);
                    }
                    let base = self.diff_base(state, &other).await?;
                    let other_changed =
                        diff_name_only(&self.repo_root, &base, &other.branch).await?;
                    let overlap: Vec<String> = other_changed
                        .into_iter()
                        .filter(|file| changed_set.contains(file))
                        .collect();
                    Ok::<_, String>((!overlap.is_empty()).then(|| TowerBranchConflict {
                        branch: other.branch.clone(),
                        files: overlap,
                    }))
                }
            },
        )
        .await;
        let mut conflicts_with = Vec::new();
        for conflict in conflicts {
            if let Some(conflict) = conflict? {
                conflicts_with.push(conflict);
            }
        }

        let index = state
            .missions
            .iter()
            .position(|candidate| candidate.id == mission.id)
            .expect("mission was resolved from state");
        state.missions[index].status = TowerMissionStatus::Merged;
        mission = state.missions[index].clone();
        self.save(&state).await?;
        self.render_missions_index(&state).await?;
        self.render_mission_file(&mission).await?;
        let merge_short = &merge_commit[..merge_commit.len().min(7)];
        let review_waived = if gate.review_binding == Some(TowerReviewBinding::CleanRebaseWaived) {
            Some("yes")
        } else {
            None
        };
        self.append_log(
            TOWER_NAME,
            "merge",
            &[
                ("branch", Some(branch)),
                ("base", Some(state.base.as_str())),
                ("merge_commit", Some(merge_short)),
                ("review_waived", review_waived),
            ],
            None,
        )
        .await?;
        Ok(TowerMergeResult {
            status: TowerMergeStatus::Merged,
            merge_commit,
            evaluated_branch_tip,
            current_branch_tip,
            conflicts_with,
            noop: false,
        })
    }

    pub async fn mission_gate(
        &self,
        state: &TowerState,
        mission: &TowerMission,
        runtime: &TowerMissionRuntimeOptions,
    ) -> Result<TowerMissionGateResult, String> {
        let (reviews, observations) = tokio::join!(
            self.reviews_for(&mission.branch),
            self.observe_mission_git(state, mission),
        );
        let reviews = reviews?;
        let observations = observations?;
        let active_agent_ids = runtime
            .active_agent_ids
            .as_ref()
            .map(TowerActiveAgentIds::resolve);
        let workers: Vec<TowerWorkerRuntime> = state
            .roster
            .agents
            .iter()
            .filter(|agent| {
                agent.kind == TowerAgentKind::Worker
                    && (agent.mission_id.as_deref() == Some(mission.id.as_str())
                        || agent.branch.as_deref() == Some(mission.branch.as_str())
                        || agent.worktree.as_deref() == Some(mission.worktree.as_str()))
            })
            .map(|agent| TowerWorkerRuntime {
                name: agent.name.clone(),
                status: match active_agent_ids.as_ref() {
                    None => TowerWorkerRuntimeStatus::Unknown,
                    Some(active) if active.contains(&agent.agent_id) => {
                        TowerWorkerRuntimeStatus::Busy
                    }
                    Some(_) => TowerWorkerRuntimeStatus::Idle,
                },
            })
            .collect();
        Ok(evaluate_mission_gate(
            state,
            mission,
            &reviews,
            &workers,
            observations,
            mission.last_rebase.as_ref(),
        ))
    }

    async fn observe_mission_git(
        &self,
        state: &TowerState,
        mission: &TowerMission,
    ) -> Result<TowerMissionGitObservations, String> {
        let (base_tip, head_tip, checked_out, dirty, has_branch) = tokio::join!(
            branch_tip(&self.repo_root, &state.base),
            branch_tip(&self.repo_root, "HEAD"),
            async { current_branch(&self.repo_root).await.ok() },
            list_base_dirty_entries(&self.repo_root),
            branch_exists(&self.repo_root, &mission.branch),
        );
        let base_tip = base_tip?;
        let head_tip = head_tip?;
        let has_branch = has_branch?;
        let base_dirty_files: Vec<String> =
            dirty?.into_iter().map(|entry| entry.path).collect();
        if !has_branch {
            return Ok(TowerMissionGitObservations {
                base_tip,
                head_tip,
                checked_out_branch: checked_out,
                base_dirty_files,
                branch: None,
            });
        }
        let tip = branch_tip(&self.repo_root, mission.branch.as_str()).await?;
        let (base_is_ancestor, spawn_base_is_ancestor) = tokio::join!(
            is_ancestor(&self.repo_root, &base_tip, &tip),
            async {
                match mission.spawn_base.as_deref() {
                    Some(spawn_base) => is_ancestor(&self.repo_root, spawn_base, &tip).await,
                    None => Ok(false),
                }
            },
        );
        let base_is_ancestor = base_is_ancestor?;
        let diff_base = if spawn_base_is_ancestor? {
            mission
                .spawn_base
                .clone()
                .unwrap_or_else(|| base_tip.clone())
        } else {
            base_tip.clone()
        };
        let (changed_files, merge_touched_files) = tokio::join!(
            diff_name_only(&self.repo_root, &diff_base, &tip),
            diff_name_only(&self.repo_root, &head_tip, &tip),
        );
        Ok(TowerMissionGitObservations {
            base_tip,
            head_tip,
            checked_out_branch: checked_out,
            base_dirty_files,
            branch: Some(TowerMissionBranchObservation {
                tip,
                base_is_ancestor,
                diff_base,
                changed_files: changed_files?,
                merge_touched_files: merge_touched_files?,
            }),
        })
    }

    pub async fn rebase_mission(
        &self,
        caller_name: &str,
        id: &str,
        runtime: TowerMissionRuntimeOptions,
    ) -> Result<TowerRebaseMissionResult, String> {
        let caller_name = caller_name.to_string();
        let id = id.to_string();
        self.with_state_lock(move || async move {
            self.rebase_mission_locked(&caller_name, &id, &runtime)
                .await
        })
        .await
    }

    async fn rebase_mission_locked(
        &self,
        caller_name: &str,
        id: &str,
        runtime: &TowerMissionRuntimeOptions,
    ) -> Result<TowerRebaseMissionResult, String> {
        if caller_name != TOWER_NAME {
            return Err(format!(
                "agent \"{caller_name}\" cannot rebase a mission branch — only the tower runs TowerRebase; a worker rebases only when the tower asks it to"
            ));
        }
        let mut state = self.load().await?;
        let Some(index) = state.missions.iter().position(|mission| mission.id == id) else {
            return Err(format!("unknown mission \"{id}\""));
        };
        let mission = state.missions[index].clone();
        if !is_open_mission(&mission) {
            return Err(format!(
                "mission {id} is {} — only open missions rebase; closed missions are historical records",
                mission.status.as_str()
            ));
        }
        if mission.kind == TowerMissionKind::Survey {
            return Err(format!(
                "mission {id} is a read-only survey — it has no work branch to rebase"
            ));
        }
        let active_agent_ids = runtime
            .active_agent_ids
            .as_ref()
            .map(TowerActiveAgentIds::resolve);
        let workers: Vec<&TowerRosterEntry> = state
            .roster
            .agents
            .iter()
            .filter(|agent| {
                agent.kind == TowerAgentKind::Worker
                    && (agent.mission_id.as_deref() == Some(mission.id.as_str())
                        || agent.branch.as_deref() == Some(mission.branch.as_str())
                        || agent.worktree.as_deref() == Some(mission.worktree.as_str()))
            })
            .collect();
        let busy: Vec<&str> = workers
            .iter()
            .filter(|worker| {
                active_agent_ids
                    .as_ref()
                    .is_some_and(|active| active.contains(&worker.agent_id))
            })
            .map(|worker| worker.name.as_str())
            .collect();
        if !busy.is_empty() {
            return Err(format!(
                "worker(s) {} are mid-turn — rebasing under a running worker could race its edits; TowerSend a message asking for a worker-run rebase, or retry once every worker is idle",
                busy.join(", ")
            ));
        }
        if active_agent_ids.is_none() && !workers.is_empty() {
            return Err(format!(
                "worker runtime for {} cannot be observed authoritatively — unknown is not idle; retry TowerRebase from the main tower agent",
                workers
                    .iter()
                    .map(|worker| worker.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !branch_exists(&self.repo_root, &mission.branch).await? {
            return Err(format!(
                "mission {id} has no branch \"{}\" yet — spawn a worker to create it before rebasing",
                mission.branch
            ));
        }
        let worktree_rel = format!("{WORKTREES_DIR}/{}", mission.worktree);
        let worktree = self.abs(&worktree_rel);
        if !is_registered_worktree(&self.repo_root, &worktree).await? {
            return Err(format!(
                "mission {id} has no registered worktree at {worktree_rel} — spawn a worker to create it before rebasing"
            ));
        }
        if is_worktree_dirty(&worktree).await? {
            return Err(format!(
                "worktree {} has uncommitted changes — rebasing under uncommitted work could lose it; have the worker commit or stash first, then retry",
                mission.worktree
            ));
        }
        let git_dir_raw = git(&worktree, &["rev-parse", "--git-dir"]).await?;
        let git_dir_raw = PathBuf::from(git_dir_raw.trim());
        let git_dir = if git_dir_raw.is_absolute() {
            git_dir_raw
        } else {
            worktree.join(git_dir_raw)
        };
        if rebase_in_progress(&git_dir) {
            return Err(format!(
                "worktree {} already has a rebase in progress — finish or abort it (git rebase --continue / --abort in the worktree) before the tower rebases the branch",
                mission.worktree
            ));
        }
        let base_commit = branch_tip(&self.repo_root, &state.base).await?;
        let from_commit = branch_tip(&self.repo_root, mission.branch.as_str()).await?;
        let checked_out = git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        if checked_out != mission.branch {
            return Err(format!(
                "worktree {} is on \"{checked_out}\", not mission branch \"{}\" — restore the registered checkout before rebasing; nothing was changed",
                mission.worktree, mission.branch
            ));
        }
        let worktree_head = branch_tip(&worktree, "HEAD").await?;
        if worktree_head != from_commit {
            return Err(format!(
                "worktree {} HEAD {worktree_head} does not match mission branch \"{}\" at {from_commit} — repair the diverged checkout before rebasing; nothing was changed",
                mission.worktree, mission.branch
            ));
        }
        if is_ancestor(&self.repo_root, &base_commit, &from_commit).await? {
            self.append_log(
                TOWER_NAME,
                "rebase.mission",
                &[("id", Some(id)), ("result", Some("up-to-date"))],
                None,
            )
            .await?;
            return Ok(TowerRebaseMissionResult {
                status: TowerRebaseMissionStatus::UpToDate,
                from_commit: None,
                to_commit: None,
                files: Vec::new(),
            });
        }

        let rebase_error = git(&worktree, &["rebase", base_commit.as_str()]).await.err();
        if let Some(rebase_error) = rebase_error {
            let conflicted = match git(
                &worktree,
                &["diff", "--name-only", "--diff-filter=U"],
            )
            .await
            {
                Ok(conflicted) => conflicted,
                Err(error) => {
                    return Err(format!(
                        "recovery-required: rebase of mission {id} failed and its state could not be inspected: {error} — no waiver was recorded and the mission was not marked blocked"
                    ));
                }
            };
            let files: Vec<String> = conflicted
                .split('\n')
                .map(str::trim)
                .filter(|file| !file.is_empty())
                .map(str::to_string)
                .collect();
            let in_progress = rebase_in_progress(&git_dir);
            if in_progress {
                if let Err(error) = git(&worktree, &["rebase", "--abort"]).await {
                    return Err(format!(
                        "recovery-required: rebase of mission {id} failed and git rebase --abort also failed: {error} — no recovery is claimed, no waiver was recorded, and the mission was not marked blocked"
                    ));
                }
            }
            self.assert_rebase_recovered(&worktree, &git_dir, &mission, &from_commit)
                .await?;
            if in_progress && !files.is_empty() {
                let blocker = if mission.kind == TowerMissionKind::Artifact {
                    format!(
                        "tower rebase onto {} conflicted in: {} — the rebase was aborted and your worktree is back to the pre-rebase state. Resolve it yourself: run `git rebase {}` in your worktree, fix the listed conflicts, `git add` the resolved files, then continue with `GIT_EDITOR=true git rebase --continue` (a plain `git rebase --continue` opens an editor and hangs a non-interactive shell). Re-run your checks, clear this blocker (TowerMission clear_blockers), then run TowerComplete with the current report and key_artifacts so the tower can present the new acceptance-request for fresh explicit human acceptance of the entire tip — the moved tip cannot reuse the old acceptance",
                        state.base,
                        files.join(", "),
                        state.base
                    )
                } else {
                    format!(
                        "tower rebase onto {} conflicted in: {} — the rebase was aborted and your worktree is back to the pre-rebase state. Resolve it yourself: run `git rebase {}` in your worktree, fix the listed conflicts, `git add` the resolved files, then continue with `GIT_EDITOR=true git rebase --continue` (a plain `git rebase --continue` opens an editor and hangs a non-interactive shell). Re-run your tests, then clear this blocker (TowerMission clear_blockers) and TowerSend the tower asking for a fresh review — a worker-run rebase is not covered by the clean-rebase waiver, so the moved tip needs a new clean review before the merge can land",
                        state.base,
                        files.join(", "),
                        state.base
                    )
                };
                state.missions[index].blockers.push(blocker);
                state.missions[index].status = TowerMissionStatus::Blocked;
                let mission = state.missions[index].clone();
                self.save(&state).await?;
                self.render_missions_index(&state).await?;
                self.render_mission_file(&mission).await?;
                let files_joined = files.join(",");
                self.append_log(
                    TOWER_NAME,
                    "rebase.mission",
                    &[
                        ("id", Some(id)),
                        ("result", Some("conflict")),
                        ("files", Some(files_joined.as_str())),
                    ],
                    None,
                )
                .await?;
                return Ok(TowerRebaseMissionResult {
                    status: TowerRebaseMissionStatus::Conflict,
                    from_commit: Some(from_commit),
                    to_commit: None,
                    files,
                });
            }
            return Err(format!(
                "rebase of mission {id} failed: {rebase_error} — no rebase conflict with unmerged paths was observed, so the mission was not marked blocked and no review waiver was recorded"
            ));
        }

        let to_commit = match branch_tip(&self.repo_root, mission.branch.as_str()).await {
            Ok(commit) => commit,
            Err(error) => {
                return Err(format!(
                    "recovery-required: rebase of mission {id} returned success but its postconditions could not be inspected: {error} — no review waiver was recorded"
                ));
            }
        };
        let mut problems = Vec::new();
        match git(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            Ok(checked_out) if checked_out != mission.branch.as_str() => {
                problems.push("the worktree no longer has the mission branch checked out");
            }
            Err(error) => {
                return Err(format!(
                    "recovery-required: rebase of mission {id} returned success but its postconditions could not be inspected: {error} — no review waiver was recorded"
                ));
            }
            Ok(_) => {}
        }
        match branch_tip(&worktree, "HEAD").await {
            Ok(head) if head != to_commit => {
                problems.push("worktree HEAD and the mission branch tip differ");
            }
            Err(error) => {
                return Err(format!(
                    "recovery-required: rebase of mission {id} returned success but its postconditions could not be inspected: {error} — no review waiver was recorded"
                ));
            }
            Ok(_) => {}
        }
        match is_worktree_dirty(&worktree).await {
            Ok(true) => problems.push("the worktree is dirty"),
            Ok(false) => {}
            Err(error) => {
                return Err(format!(
                    "recovery-required: rebase of mission {id} returned success but its postconditions could not be inspected: {error} — no review waiver was recorded"
                ));
            }
        }
        match is_ancestor(&self.repo_root, &base_commit, &to_commit).await {
            Ok(true) => {}
            Ok(false) => {
                problems.push("the rebased branch does not contain the evaluated base commit")
            }
            Err(error) => {
                return Err(format!(
                    "recovery-required: rebase of mission {id} returned success but its postconditions could not be inspected: {error} — no review waiver was recorded"
                ));
            }
        }
        if !problems.is_empty() {
            return Err(format!(
                "recovery-required: rebase of mission {id} returned success but its postconditions failed ({}) — no review waiver was recorded; inspect the worktree and branch before retrying",
                problems.join("; ")
            ));
        }
        state.missions[index].last_rebase = Some(super::types::TowerRebaseRecord {
            from_commit: from_commit.clone(),
            to_commit: to_commit.clone(),
        });
        let mission = state.missions[index].clone();
        self.save(&state).await?;
        self.render_missions_index(&state).await?;
        self.render_mission_file(&mission).await?;
        let from_short = &from_commit[..from_commit.len().min(7)];
        let to_short = &to_commit[..to_commit.len().min(7)];
        self.append_log(
            TOWER_NAME,
            "rebase.mission",
            &[
                ("id", Some(id)),
                ("result", Some("clean")),
                ("from", Some(from_short)),
                ("to", Some(to_short)),
            ],
            None,
        )
        .await?;
        Ok(TowerRebaseMissionResult {
            status: TowerRebaseMissionStatus::Rebased,
            from_commit: Some(from_commit),
            to_commit: Some(to_commit),
            files: Vec::new(),
        })
    }

    async fn assert_rebase_recovered(
        &self,
        worktree: &Path,
        git_dir: &Path,
        mission: &TowerMission,
        from_commit: &str,
    ) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        if rebase_in_progress(git_dir) {
            problems.push("a rebase is still in progress".to_string());
        }
        match git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
            Ok(checked_out) if checked_out != mission.branch.as_str() => {
                problems.push("the mission branch is not checked out".to_string());
            }
            Ok(_) => {}
            Err(error) => problems.push(format!("recovery inspection failed: {error}")),
        }
        match branch_tip(worktree, "HEAD").await {
            Ok(head) if head != from_commit => {
                problems.push("worktree HEAD was not restored to the pre-rebase commit".to_string());
            }
            Ok(_) => {}
            Err(error) => problems.push(format!("recovery inspection failed: {error}")),
        }
        match branch_tip(&self.repo_root, mission.branch.as_str()).await {
            Ok(tip) if tip != from_commit => {
                problems.push("the mission branch tip was not restored".to_string());
            }
            Ok(_) => {}
            Err(error) => problems.push(format!("recovery inspection failed: {error}")),
        }
        match is_worktree_dirty(worktree).await {
            Ok(true) => problems.push("the worktree is not clean".to_string()),
            Ok(false) => {}
            Err(error) => problems.push(format!("recovery inspection failed: {error}")),
        }
        if !problems.is_empty() {
            return Err(format!(
                "recovery-required: rebase recovery for mission {} failed its postconditions ({}) — no recovery is claimed, no waiver was recorded, and the mission was not marked blocked",
                mission.id,
                problems.join("; ")
            ));
        }
        Ok(())
    }

    pub async fn add_worktree(
        &self,
        worktree: &str,
        branch: &str,
        base: &str,
    ) -> Result<TowerAddWorktreeResult, String> {
        let rel = format!("{WORKTREES_DIR}/{worktree}");
        let abs_path = self.abs(&rel);
        if branch_exists(&self.repo_root, branch).await? {
            let state = self.load().await?;
            let mission = state
                .missions
                .iter()
                .find(|mission| mission.worktree == worktree && mission.branch == branch);
            let registered = is_registered_worktree(&self.repo_root, &abs_path).await?;
            let checked_out = if registered {
                git(&abs_path, &["rev-parse", "--abbrev-ref", "HEAD"])
                    .await
                    .ok()
            } else {
                None
            };
            let checked_out_matches = checked_out.as_deref().map(str::trim) == Some(branch);
            if mission.and_then(|mission| mission.owner.as_deref()).is_none()
                && !checked_out_matches
            {
                return Err(unowned_branch_message(branch));
            }
            worktree_add(&self.repo_root, &abs_path, branch, base).await?;
            self.append_log(
                TOWER_NAME,
                "worktree.add",
                &[
                    ("worktree", Some(worktree)),
                    ("branch", Some(branch)),
                    ("base", Some(base)),
                    ("spawn_base", None),
                ],
                None,
            )
            .await?;
            return Ok(TowerAddWorktreeResult {
                rel,
                spawn_base: None,
            });
        }

        let dirty = list_base_dirty_entries(&self.repo_root).await?;
        if dirty.iter().any(|entry| entry.unmerged) {
            return Err(
                "the base checkout has unmerged paths (an in-progress merge, rebase, or cherry-pick) — finish or abort it before spawning workers"
                    .into(),
            );
        }
        if !dirty.is_empty() {
            let checkout = match current_branch(&self.repo_root).await {
                Ok(branch) => branch,
                Err(_) => {
                    return Err(format!(
                        "the main checkout is in a detached HEAD state with uncommitted changes, and the recorded base is \"{base}\" — a WIP snapshot would carry detached-HEAD content into the mission branch; check out \"{base}\" (`git checkout {base}`) or commit/stash the changes before spawning workers"
                    ));
                }
            };
            if checkout != base {
                return Err(format!(
                    "the main checkout is on \"{checkout}\" with uncommitted changes, not the recorded base \"{base}\" — a WIP snapshot would carry \"{checkout}\" content into the mission branch; switch back to \"{base}\" (`git checkout {base}`) or commit/stash the changes before spawning workers"
                ));
            }
        }
        let paths: Vec<String> = dirty.into_iter().map(|entry| entry.path).collect();
        let spawn_base = snapshot_base_wip(
            &self.repo_root,
            base,
            &paths,
            &format!("tower: snapshot of uncommitted base checkout changes (worktree {worktree})"),
        )
        .await?;
        let start = spawn_base.as_deref().unwrap_or(base);
        match worktree_add_new_branch(&self.repo_root, &abs_path, branch, start).await {
            Ok(()) => {}
            Err(error) => {
                if branch_exists(&self.repo_root, branch).await? {
                    return Err(unowned_branch_message(branch));
                }
                return Err(error);
            }
        }
        self.append_log(
            TOWER_NAME,
            "worktree.add",
            &[
                ("worktree", Some(worktree)),
                ("branch", Some(branch)),
                ("base", Some(base)),
                ("spawn_base", spawn_base.as_deref()),
            ],
            None,
        )
        .await?;
        Ok(TowerAddWorktreeResult { rel, spawn_base })
    }

    pub async fn teardown(
        &self,
        options: TowerTeardownOptions,
    ) -> Result<Vec<String>, String> {
        let state = self.load().await?;
        let dry_run = options.dry_run;
        let force = options.force;
        let live_agent_ids = options.live_agent_ids;
        let mut excluded: Vec<(String, String)> = Vec::new();
        for raw in &options.exclude {
            let trimmed = raw.trim().trim_end_matches('/');
            if trimmed.is_empty() {
                continue;
            }
            let prefix = format!("{WORKTREES_DIR}/");
            let short = trimmed.strip_prefix(&prefix).unwrap_or(trimmed);
            excluded.push((short.to_string(), raw.clone()));
        }
        let mut report = Vec::new();
        for mission in &state.missions {
            let rel = format!("{WORKTREES_DIR}/{}", mission.worktree);
            let abs_path = self.abs(&rel);
            let was_excluded = match excluded
                .iter()
                .position(|(short, _)| short == &mission.worktree)
            {
                Some(index) => {
                    excluded.remove(index);
                    true
                }
                None => false,
            };
            if !is_registered_worktree(&self.repo_root, &abs_path).await? {
                report.push(format!("already removed {rel}"));
                if !dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.remove.skipped",
                        &[
                            ("worktree", Some(mission.worktree.as_str())),
                            ("reason", Some("already-removed")),
                        ],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            let verb = if dry_run { "would keep" } else { "kept" };
            if was_excluded {
                report.push(format!("{verb} {rel} (excluded)"));
                if !dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.keep",
                        &[
                            ("worktree", Some(mission.worktree.as_str())),
                            ("reason", Some("excluded")),
                        ],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            if let Some(live) = state.roster.agents.iter().find(|agent| {
                agent.worktree.as_deref() == Some(mission.worktree.as_str())
                    && live_agent_ids.contains(&agent.agent_id)
            }) {
                report.push(format!("{verb} {rel} (live agent: {})", live.name));
                if !dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.keep",
                        &[
                            ("worktree", Some(mission.worktree.as_str())),
                            ("reason", Some("live-agent")),
                            ("agent", Some(live.name.as_str())),
                        ],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            if !force {
                let dirty = worktree_dirty_state(&abs_path).await;
                if dirty != "clean" {
                    let (reason_text, reason) = if dirty == "dirty" {
                        (
                            "uncommitted changes — rerun with force to remove",
                            "uncommitted-changes",
                        )
                    } else {
                        (
                            "git status unavailable — rerun with force to remove",
                            "status-unavailable",
                        )
                    };
                    report.push(format!("{verb} {rel} ({reason_text})"));
                    if !dry_run {
                        self.append_log(
                            TOWER_NAME,
                            "worktree.keep",
                            &[
                                ("worktree", Some(mission.worktree.as_str())),
                                ("reason", Some(reason)),
                            ],
                            None,
                        )
                        .await?;
                    }
                    continue;
                }
            }
            if dry_run {
                report.push(format!("would remove {rel}"));
                continue;
            }
            match worktree_remove(&self.repo_root, &abs_path).await {
                Ok(()) => {
                    report.push(format!("removed {rel}"));
                    self.append_log(
                        TOWER_NAME,
                        "worktree.remove",
                        &[("worktree", Some(mission.worktree.as_str()))],
                        None,
                    )
                    .await?;
                }
                Err(error) => {
                    report.push(format!("failed to remove {rel}: {error}"));
                    self.append_log(
                        TOWER_NAME,
                        "worktree.remove.failed",
                        &[
                            ("worktree", Some(mission.worktree.as_str())),
                            ("reason", Some(error.as_str())),
                        ],
                        None,
                    )
                    .await?;
                }
            }
        }
        for (_, original) in &excluded {
            report.push(format!(
                "excluded worktree \"{original}\" matched no mission worktree"
            ));
        }
        if !dry_run {
            self.append_log(
                TOWER_NAME,
                "teardown",
                &[("force", force.then_some("yes"))],
                None,
            )
            .await?;
        }
        Ok(report)
    }

    async fn render_missions_index(&self, state: &TowerState) -> Result<(), String> {
        let rows = state
            .missions
            .iter()
            .map(|mission| {
                format!(
                    "| {} | {} | {} | {} | {} | {} | {} |",
                    mission.id,
                    mission.title,
                    mission.branch,
                    mission.worktree,
                    mission.status.emoji(),
                    mission.owner.as_deref().unwrap_or("—"),
                    mission.kind.as_str()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let deps = state
            .missions
            .iter()
            .flat_map(|mission| {
                mission
                    .deps
                    .iter()
                    .map(move |dep| format!("{dep} → {}", mission.id))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let scopes = state
            .missions
            .iter()
            .map(|mission| {
                format!(
                    "- {}{}: {}",
                    mission.id,
                    if mission.kind == TowerMissionKind::Survey {
                        " (survey — informational, reserves nothing)"
                    } else {
                        ""
                    },
                    mission.scope.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut content = vec![
            "# MISSIONS".to_string(),
            String::new(),
            "<!-- Generated by tower tools from state.json — do not edit by hand. -->".to_string(),
            String::new(),
            "| ID | Mission | Branch | Worktree | Status | Owner | Kind |".to_string(),
            "| -- | ------- | ------ | -------- | ------ | ----- | ---- |".to_string(),
        ];
        if !rows.is_empty() {
            content.push(rows);
        }
        content.extend([
            String::new(),
            "Status: 🟡 planned · 🔵 active · 🟢 completed · 🔴 blocked · ⏸️ paused · ✅ merged · 🚫 abandoned".to_string(),
            format!("Mode: {} — Base: {}", state.mode, state.base),
            String::new(),
            "## Dependency Flow".to_string(),
            if deps.is_empty() {
                "(none)".to_string()
            } else {
                deps
            },
            String::new(),
            "## Scope Map".to_string(),
            if scopes.is_empty() {
                "(none)".to_string()
            } else {
                scopes
            },
            String::new(),
        ]);
        fs::write(self.abs(MISSIONS_INDEX), content.join("\n").as_bytes())
            .await
            .map_err(|error| error.to_string())
    }

    async fn render_mission_file(&self, mission: &TowerMission) -> Result<(), String> {
        let rel = format!(
            "{MISSIONS_DIR}/{}",
            mission_file_name(&mission.id, &mission.slug)
        );
        let mut content = vec![
            format!(
                "# Mission {}: {}{}",
                mission.id,
                mission.title,
                if mission.kind == TowerMissionKind::Survey {
                    " 🔍 (read-only survey)"
                } else {
                    ""
                }
            ),
            String::new(),
            "<!-- Generated by tower tools from state.json — update via the TowerMission tool. -->"
                .to_string(),
            String::new(),
            "| Branch | Worktree | Status | Scope | Owner | Kind |".to_string(),
            "| ------ | -------- | ------ | ----- | ----- | ---- |".to_string(),
            format!(
                "| {} | {} | {} | {} | {} | {} |",
                mission.branch,
                mission.worktree,
                mission.status.emoji(),
                mission.scope.join(", "),
                mission.owner.as_deref().unwrap_or("—"),
                mission.kind.as_str()
            ),
            String::new(),
        ];
        if let Some(context) = mission.context.as_deref() {
            content.extend([
                "## Context — the user's own words, verbatim".to_string(),
                String::new(),
                context.to_string(),
                String::new(),
            ]);
        }
        content.push("## Tasks".to_string());
        if mission.tasks.is_empty() {
            content.push("- [ ] (no tasks recorded)".to_string());
        } else {
            for task in &mission.tasks {
                let marker = if task.done {
                    "x"
                } else if task.dropped {
                    "-"
                } else {
                    " "
                };
                let suffix = if task.dropped { " (dropped)" } else { "" };
                content.push(format!("- [{marker}] {}{suffix}", task.text));
            }
        }
        content.extend([
            String::new(),
            "## Dependencies".to_string(),
            if mission.deps.is_empty() {
                "(none)".to_string()
            } else {
                mission.deps.join(", ")
            },
            String::new(),
            "## Blockers".to_string(),
        ]);
        if mission.blockers.is_empty() {
            content.push("- (none)".to_string());
        } else {
            for blocker in &mission.blockers {
                content.push(format!("- {blocker}"));
            }
        }
        content.extend([String::new(), "## Notes".to_string()]);
        if mission.notes.is_empty() {
            content.push("- (none)".to_string());
        } else {
            for note in &mission.notes {
                content.push(format!("- {note}"));
            }
        }
        content.push(String::new());
        fs::write(self.abs(&rel), content.join("\n").as_bytes())
            .await
            .map_err(|error| error.to_string())
    }

    async fn write_unique(&self, rel: &str, content: &str) -> Result<String, String> {
        let (stem, ext) = match rel.rfind('.') {
            Some(position) => (&rel[..position], &rel[position..]),
            None => (rel, ""),
        };
        for attempt in 0..100 {
            let candidate = if attempt == 0 {
                rel.to_string()
            } else {
                format!("{stem}-{}{ext}", attempt + 1)
            };
            let temporary = format!(
                "{candidate}.tmp-{}-{}",
                std::process::id(),
                new_token()
            );
            let temporary_path = self.abs(&temporary);
            let candidate_path = self.abs(&candidate);
            let published = async {
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary_path)
                    .await?;
                file.write_all(content.as_bytes()).await?;
                drop(file);
                fs::hard_link(&temporary_path, &candidate_path).await
            }
            .await;
            match published {
                Ok(()) => {
                    let _ = fs::remove_file(&temporary_path).await;
                    return Ok(candidate);
                }
                Err(error) => {
                    let _ = fs::remove_file(&temporary_path).await;
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                    ) {
                        match OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&candidate_path)
                            .await
                        {
                            Ok(mut file) => {
                                file.write_all(content.as_bytes())
                                    .await
                                    .map_err(|error| error.to_string())?;
                                return Ok(candidate);
                            }
                            Err(error)
                                if error.kind() == std::io::ErrorKind::AlreadyExists =>
                            {
                                continue;
                            }
                            Err(error) => return Err(error.to_string()),
                        }
                    }
                    return Err(error.to_string());
                }
            }
        }
        Err(format!("could not create a unique file for {rel}"))
    }
}

fn normalize_artifact_paths(paths: &[String]) -> Result<Vec<String>, String> {
    let mut normalized = Vec::new();
    for path in paths {
        let trimmed = path.trim();
        let replaced = trimmed.replace('\\', "/");
        let mut parts: Vec<&str> = Vec::new();
        let mut escapes = false;
        for segment in replaced.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        escapes = true;
                    }
                }
                other => parts.push(other),
            }
        }
        let candidate = parts.join("/");
        if trimmed.is_empty()
            || trimmed.contains(['\0', '\r', '\n'])
            || candidate.is_empty()
            || candidate == "."
            || candidate == ".."
            || candidate.starts_with("../")
            || escapes
            || replaced.starts_with('/')
        {
            return Err(format!(
                "TowerComplete key_artifacts entries must be non-empty relative file paths inside the repository; received \"{path}\""
            ));
        }
        normalized.push(candidate);
    }
    let mut seen = HashSet::new();
    normalized.retain(|path| seen.insert(path.clone()));
    Ok(normalized)
}

fn same_artifact_paths(left: Option<&Vec<String>>, right: &[String]) -> bool {
    match left {
        Some(left) => left.len() == right.len() && left.iter().zip(right.iter()).all(|(a, b)| a == b),
        None => false,
    }
}

fn artifact_acceptance_report(
    report: &str,
    key_artifacts: &[String],
    changed: &[String],
    completion_tip: &str,
) -> String {
    let key: HashSet<&str> = key_artifacts.iter().map(String::as_str).collect();
    let additional = changed
        .iter()
        .filter(|path| !key.contains(path.as_str()))
        .count();
    [
        "Key artifacts for human acceptance:".to_string(),
        key_artifacts
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n"),
        String::new(),
        format!("Completion tip: {completion_tip}"),
        "Approval authorizes the entire current mission tip presented for acceptance, not only these key artifacts."
            .to_string(),
        format!("Additional committed files included in this approval: {additional}."),
        "Optional full manifest: inspect the current mission branch diff on request; reading every path is not required."
            .to_string(),
        String::new(),
        report.to_string(),
    ]
    .join("\n")
}

fn review_document_body(input: &TowerReviewInput) -> String {
    let checks = input
        .checks
        .as_ref()
        .map(|checks| {
            checks
                .iter()
                .map(|check| format!("- [x] {check}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    [
        "## Findings",
        "",
        input.findings.trim(),
        "",
        "## Checks",
        if checks.is_empty() {
            "- [x] (reviewer reported no formal checks)"
        } else {
            checks.as_str()
        },
        "",
        "## Decision",
        input.decision.trim(),
    ]
    .join("\n")
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    match globset::Glob::new(pattern) {
        Ok(glob) => glob.compile_matcher().is_match(path),
        Err(_) => path.starts_with(pattern.trim_end_matches('*')),
    }
}

async fn worktree_dirty_state(path: &Path) -> &'static str {
    match git(path, &["status", "--porcelain"]).await {
        Ok(status) if status.trim().is_empty() => "clean",
        Ok(_) => "dirty",
        Err(_) => "unknown",
    }
}

fn rebase_in_progress(git_dir: &Path) -> bool {
    git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists()
}

async fn has_merge_head(cwd: &Path) -> Result<bool, String> {
    match git_probe(cwd, &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"]).await {
        ReadOutcome::Found(_) => Ok(true),
        ReadOutcome::Absent => Ok(false),
        ReadOutcome::Unreadable(cause) => Err(cause.to_string()),
    }
}

async fn merge_no_ff_with_message(
    cwd: &Path,
    revision: &str,
    message: &str,
) -> Result<String, String> {
    if has_merge_head(cwd).await? {
        return Err(
            "cannot start a tower merge while MERGE_HEAD already exists; finish or abort the existing merge first"
                .into(),
        );
    }
    let head_before = branch_tip(cwd, "HEAD").await?;
    let status_before = git(cwd, &["status", "--porcelain"]).await?;
    if let Err(original) = git(cwd, &["merge", "--no-ff", "-m", message, revision]).await {
        let merge_left_behind = match has_merge_head(cwd).await {
            Ok(value) => value,
            Err(error) => {
                return Err(format!(
                    "recovery-required: could not inspect MERGE_HEAD after the merge failed: {error}"
                ));
            }
        };
        if merge_left_behind {
            if let Err(abort_error) = git(cwd, &["merge", "--abort"]).await {
                return Err(format!("recovery-required: {abort_error}"));
            }
            let mut problems: Vec<String> = Vec::new();
            match has_merge_head(cwd).await {
                Ok(true) => problems.push("MERGE_HEAD still exists".to_string()),
                Ok(false) => {}
                Err(error) => problems.push(format!("recovery inspection failed: {error}")),
            }
            if branch_tip(cwd, "HEAD").await? != head_before {
                problems.push("HEAD was not restored to the pre-merge commit".to_string());
            }
            if git(cwd, &["status", "--porcelain"]).await? != status_before {
                problems.push(
                    "the index or worktree differs from its pre-merge state".to_string(),
                );
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
