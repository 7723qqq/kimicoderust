mod complete;
mod gate;
mod lock;
mod messaging;
mod rebase;
mod recovery;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::tools::tower::concurrency::{
    TOWER_FILE_IO_CONCURRENCY, TOWER_GIT_CONCURRENCY, map_with_concurrency,
};
use crate::tools::tower::frontmatter::{
    parse_frontmatter, read_frontmatter_prefix, render_frontmatter,
};
use crate::tools::tower::git::{
    branch_exists, branch_tip, current_branch, diff_name_only, has_any_commit, is_ancestor,
    is_inside_repo, is_registered_worktree, is_worktree_dirty, merge_no_ff, worktree_add,
    worktree_remove,
};
use crate::tools::tower::mission_gate::{
    REASON_NOT_COMPLETED, add_auto_dependencies, assert_scopes_disjoint,
};
use crate::tools::tower::paths::{
    ACTIVITY_LOG, BROADCAST_NAME, FINDINGS_DIR, INBOX_DIR, LOG_DIR, MISSIONS_DIR, MISSIONS_INDEX,
    REVIEWS_DIR, STATE_FILE, TOWER_NAME, WORKTREES_DIR, date_dash, finding_file_name,
    inbox_file_name, mission_file_name, review_file_name, slugify, target_slug, unique_slug,
};
use crate::tools::tower::types::{
    TowerFindingInput, TowerInboxItem, TowerInitResult, TowerMergeResult, TowerMergeStatus,
    TowerMission, TowerMissionKind, TowerMissionPatch, TowerMissionStatus, TowerMissionTask,
    TowerPlanInput, TowerReviewInfo, TowerReviewInput, TowerRoster, TowerRosterEntry,
    TowerSendInput, TowerState,
};

/// v2 `MAX_REVIEW_ROUNDS` (#3976): a branch stops converging long before this
/// many rounds from the same reviewer.
pub const MAX_REVIEW_ROUNDS: u32 = 5;

pub(crate) const PREPARATION_CONFLICT: &str = "tower preparation conflict";
const PREPARED_OPERATION_TIMEOUT_MS: u64 = 60_000;
const PREPARED_OPERATION_MAX_BACKOFF_MS: u64 = 100;

#[derive(Clone)]
struct TowerStateSnapshot {
    raw: String,
    state: TowerState,
}

#[derive(Default)]
struct MissionUpdateOptions {
    validate_only: bool,
    completion_prepared: bool,
    prepared_inbox: Option<(serde_json::Value, Vec<TowerInboxItem>)>,
}

struct ReviewPreparation {
    snapshot: TowerStateSnapshot,
    inbox_files: Vec<String>,
    review_files: Vec<String>,
    round: u32,
    reviewed_commit: String,
    seq: u64,
    review_mission: Option<String>,
}

/// Process-global tower-state locks keyed by the resolved repo root. Tower
/// workers run as concurrent tasks in one engine process and share a single
/// `.tower/comms/state.json`; every read-modify-write must be serialized per
/// repo so a later save cannot clobber an earlier one (lost update) or publish
/// a half-written file. Keyed by the *resolved* root so a worktree cwd and the
/// main checkout contend on one lock. The guard is held across a whole
/// `execute_tower_*` call — the outermost entry — so the store's own methods
/// nest freely (init → adopt_foreign_roster → save) without re-entering a
/// non-reentrant mutex.
static REPO_STATE_LOCKS: OnceLock<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn state_lock_for(root: &Path) -> Arc<Mutex<()>> {
    let registry = REPO_STATE_LOCKS.get_or_init(|| StdMutex::new(HashMap::new()));
    let mut map = registry.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(root.to_path_buf()).or_default().clone()
}

#[derive(Debug, Clone, Default)]
pub struct TowerTeardownOptions {
    pub force: bool,
    pub exclude: Vec<String>,
    pub dry_run: bool,
    pub live_agent_ids: HashSet<String>,
}

#[derive(Debug, Clone, Copy)]
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
pub struct TowerStore {
    pub repo_root: PathBuf,
    state_lock_timeout_ms: u64,
    state_lock_poll_ms: u64,
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

    pub(crate) async fn run_prepared<
        TPrepared,
        TResult,
        Prepare,
        PrepareFuture,
        Commit,
        CommitFuture,
    >(
        &self,
        prepare: Prepare,
        commit: Commit,
    ) -> Result<TResult, String>
    where
        Prepare: Fn() -> PrepareFuture + 'static,
        PrepareFuture: std::future::Future<Output = Result<TPrepared, String>> + 'static,
        Commit: Fn(TPrepared) -> CommitFuture + 'static,
        CommitFuture: std::future::Future<Output = Result<TResult, String>> + 'static,
        TPrepared: 'static,
        TResult: 'static,
    {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(PREPARED_OPERATION_TIMEOUT_MS);
        let mut backoff_ms = self.state_lock_poll_ms;
        loop {
            let prepared = prepare().await?;
            match commit(prepared).await {
                Ok(result) => return Ok(result),
                Err(error) if error == PREPARATION_CONFLICT => {
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "tower protocol inputs kept changing for {PREPARED_OPERATION_TIMEOUT_MS}ms while preparing the operation; no stale snapshot was written, so retry the tool call"
                        ));
                    }
                    if backoff_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }
                    backoff_ms = backoff_ms
                        .saturating_mul(2)
                        .min(PREPARED_OPERATION_MAX_BACKOFF_MS);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.repo_root.join(rel)
    }

    /// The process-global mutex serializing this repo's tower state mutations.
    /// Bind the returned `Arc` before locking so the guard can borrow it:
    /// `let lock = store.state_lock(); let _guard = lock.lock().await;`.
    pub fn state_lock(&self) -> Arc<Mutex<()>> {
        state_lock_for(&self.repo_root)
    }

    /// Whether this workspace has a tower state file. Only a *missing* file
    /// counts as uninitialized: an existing but unstattable one (EACCES,
    /// ELOOP, transient I/O) must not read as a fresh workspace, or `init`
    /// would overwrite the foreign roster with a new one (v2 `adopt`'s
    /// ENOENT-only rule).
    pub async fn is_initialized(&self) -> Result<bool, String> {
        let path = self.abs(STATE_FILE);
        match fs::metadata(&path).await {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!(
                "could not read the tower state at {}: {error}",
                path.display()
            )),
        }
    }

    pub async fn init(
        &self,
        session_id: Option<String>,
        base: Option<String>,
    ) -> Result<TowerInitResult, String> {
        fs::create_dir_all(self.abs(STATE_FILE).parent().unwrap())
            .await
            .map_err(|e| e.to_string())?;
        self.with_state_lock(move || async move { self.init_locked(session_id, base).await })
            .await
    }

    async fn init_locked(
        &self,
        session_id: Option<String>,
        base: Option<String>,
    ) -> Result<TowerInitResult, String> {
        if !is_inside_repo(&self.repo_root).await {
            return Err(
                "tower needs a git repository (the session working directory is not inside one)"
                    .into(),
            );
        }
        if !has_any_commit(&self.repo_root).await {
            return Err(
                "the repository has no commits yet — create an initial commit first".into(),
            );
        }
        if self.is_initialized().await? {
            let mut state = self.load().await?;
            let mut rebase_digest = None;
            if let Some(ref requested_base) = base
                && requested_base != &state.base
            {
                rebase_digest = Some(self.reanchor_locked(&mut state, requested_base).await?);
            }
            let retired_agents = self.adopt_foreign_roster(&mut state, session_id).await?;
            let checkout = self.checked_out_branch().await;
            let open_missions = state
                .missions
                .iter()
                .filter(|mission| mission.status.is_open())
                .map(|mission| mission.id.clone())
                .collect();
            return Ok(TowerInitResult {
                base: state.base,
                created: false,
                retired_agents,
                checkout,
                ignored_base: None,
                rebase_digest,
                open_missions,
            });
        }

        let checkout = self.checked_out_branch().await;
        let resolved_base = match base {
            Some(b) => {
                if !branch_exists(&self.repo_root, &b).await {
                    return Err(format!(
                        "base branch \"{b}\" does not exist as a local branch — merges land on a local branch, so remote-tracking refs and tags are not accepted; create a local branch first"
                    ));
                }
                b
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
                .map_err(|e| e.to_string())?;
        }
        self.ensure_git_exclude().await?;

        let state = TowerState {
            version: 1,
            base: resolved_base.clone(),
            mode: "branch".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            session_id,
            roster: TowerRoster::default(),
            missions: Vec::new(),
        };
        self.save(&state).await?;
        fs::write(self.abs(ACTIVITY_LOG), b"")
            .await
            .map_err(|e| e.to_string())?;
        self.render_missions_index(&state).await?;
        self.append_log(
            TOWER_NAME,
            "init",
            &[("mode", &state.mode), ("base", &resolved_base)],
            Some(MISSIONS_INDEX),
        )
        .await?;

        Ok(TowerInitResult {
            base: resolved_base,
            created: true,
            retired_agents: Vec::new(),
            checkout,
            open_missions: Vec::new(),
            ignored_base: None,
            rebase_digest: None,
        })
    }

    async fn checked_out_branch(&self) -> String {
        current_branch(&self.repo_root)
            .await
            .unwrap_or_else(|_| "HEAD".into())
    }

    async fn adopt_foreign_roster(
        &self,
        state: &mut TowerState,
        session_id: Option<String>,
    ) -> Result<Vec<String>, String> {
        let Some(ref sid) = session_id else {
            return Ok(Vec::new());
        };
        if state.session_id.as_deref() == Some(sid.as_str()) {
            return Ok(Vec::new());
        }

        let previous = state.session_id.clone();
        let stale: Vec<String> = state
            .roster
            .agents
            .iter()
            .filter(|a| a.session_id.as_deref() != Some(sid.as_str()))
            .map(|a| a.name.clone())
            .collect();

        state
            .roster
            .agents
            .retain(|a| a.session_id.as_deref() == Some(sid.as_str()));
        state.session_id = Some(sid.clone());
        self.save(state).await?;

        let retired_str = stale.join(",");
        self.append_log(
            TOWER_NAME,
            "adopt",
            &[
                ("session", sid.as_str()),
                ("previous", previous.as_deref().unwrap_or("unknown")),
                ("retired", if stale.is_empty() { "" } else { &retired_str }),
            ],
            None,
        )
        .await?;

        Ok(stale)
    }

    async fn ensure_git_exclude(&self) -> Result<(), String> {
        let git_dir = match read_git_dir(&self.repo_root).await {
            Some(d) => PathBuf::from(d),
            None => self.repo_root.join(".git"),
        };
        let exclude_path = git_dir.join("info").join("exclude");
        if let Some(parent) = exclude_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
        let existing = fs::read_to_string(&exclude_path).await.unwrap_or_default();
        if existing.lines().any(|l| l.trim() == ".tower/") {
            return Ok(());
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclude_path)
            .await
            .map_err(|e| e.to_string())?;
        let newline = if existing.ends_with('\n') || existing.is_empty() {
            ""
        } else {
            "\n"
        };
        file.write_all(format!("{newline}.tower/\n").as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn load(&self) -> Result<TowerState, String> {
        Ok(self.load_snapshot().await?.state)
    }

    async fn load_snapshot(&self) -> Result<TowerStateSnapshot, String> {
        let path = self.abs(STATE_FILE);
        let raw = match fs::read_to_string(&path).await {
            Ok(raw) => raw,
            Err(_) if self.state_was_lost().await => {
                return Err(
                    "tower state was lost — .tower still exists but state.json is unreadable; TowerSend or TowerComplete can recover a minimal state"
                        .into(),
                );
            }
            Err(_) => {
                return Err(
                    "tower is not initialized in this repository — run TowerInit first".into(),
                );
            }
        };
        let state =
            serde_json::from_str(&raw).map_err(|e| format!("corrupted tower state: {e}"))?;
        Ok(TowerStateSnapshot { raw, state })
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
            let files = self.protocol_file_names(INBOX_DIR, "").await?;
            if files != expected {
                return Err(PREPARATION_CONFLICT.into());
            }
        }
        if let Some(expected) = review_files {
            let files = self.protocol_file_names(REVIEWS_DIR, "review-").await?;
            if files != expected {
                return Err(PREPARATION_CONFLICT.into());
            }
        }
        Ok(current.state)
    }

    async fn protocol_file_names(&self, dir: &str, prefix: &str) -> Result<Vec<String>, String> {
        let mut read_dir = match fs::read_dir(self.abs(dir)).await {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        let mut files = Vec::new();
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| e.to_string())? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(prefix) && name.ends_with(".md") {
                files.push(name);
            }
        }
        files.sort();
        Ok(files)
    }

    /// One field of an activity-log line.
    ///
    /// The log is a space-separated line per event, so a value carrying a
    /// newline would forge a second entry — a drop reason or a note is free text
    /// that reaches here from the model. Fold the line structure away and keep
    /// the text readable: a backslash-escaped space would be lossless, but every
    /// reader of this log is a human, so readability wins over reversibility.
    fn log_field(value: &str) -> String {
        value.replace(['\r', '\n', '\t'], " ")
    }

    async fn load_state_value(&self) -> Result<serde_json::Value, String> {
        let raw = match fs::read_to_string(self.abs(STATE_FILE)).await {
            Ok(raw) => raw,
            Err(_) if self.state_was_lost().await => {
                return Err("tower state was lost — state.json is unreadable".into());
            }
            Err(_) => {
                return Err(
                    "tower is not initialized in this repository — run TowerInit first".into(),
                );
            }
        };
        serde_json::from_str(&raw).map_err(|e| format!("corrupted tower state: {e}"))
    }

    pub async fn save(&self, state: &TowerState) -> Result<(), String> {
        let mut value = serde_json::to_value(state).map_err(|e| e.to_string())?;
        let previous = self.load_state_value().await.ok();
        if let Some(previous) = previous.as_ref() {
            for key in ["recoveredAt", "inboxAckIds"] {
                if let Some(metadata) = previous.get(key) {
                    value[key] = metadata.clone();
                }
            }
        }
        if let Some(missions) = value
            .get_mut("missions")
            .and_then(serde_json::Value::as_array_mut)
        {
            for mission in missions {
                let Some(id) = mission.get("id").cloned() else {
                    continue;
                };
                let prior = previous.as_ref().and_then(|state| {
                    state
                        .get("missions")?
                        .as_array()?
                        .iter()
                        .find(|candidate| candidate.get("id") == Some(&id))
                });
                if let Some(last_rebase) = prior.and_then(|mission| mission.get("lastRebase")) {
                    mission["lastRebase"] = last_rebase.clone();
                }
            }
        }
        if let Some(agents) = value
            .get_mut("roster")
            .and_then(|roster| roster.get_mut("agents"))
            .and_then(serde_json::Value::as_array_mut)
        {
            for agent in agents {
                let Some(object) = agent.as_object_mut() else {
                    continue;
                };
                let prior = previous.as_ref().and_then(|state| {
                    state
                        .get("roster")?
                        .get("agents")?
                        .as_array()?
                        .iter()
                        .find(|candidate| {
                            candidate.get("agentId") == object.get("agentId")
                                && candidate.get("name") == object.get("name")
                        })
                });
                let last_read = prior
                    .and_then(|agent| agent.get("lastInboxReadAt"))
                    .cloned()
                    .or_else(|| object.get("spawnedAt").cloned());
                if let Some(last_read) = last_read {
                    object.insert("lastInboxReadAt".into(), last_read);
                }
                if let Some(ack_ids) = prior.and_then(|agent| agent.get("inboxAckIds")) {
                    object.insert("inboxAckIds".into(), ack_ids.clone());
                }
            }
        }
        self.save_state_value(&value).await
    }

    async fn save_state_value(&self, value: &serde_json::Value) -> Result<(), String> {
        let file = self.abs(STATE_FILE);
        // A random suffix keeps two saves from ever interleaving into one tmp
        // file and publishing a half-written state.json; the rename below is
        // atomic either way, and the per-repo lock already serializes writers.
        let tmp = format!("{}.{}.tmp", file.to_string_lossy(), fastrand::u64(..));
        let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
        let result = async {
            fs::write(&tmp, format!("{json}\n").as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            fs::rename(&tmp, &file).await.map_err(|e| e.to_string())
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&tmp).await;
        }
        result
    }

    pub async fn append_log(
        &self,
        actor: &str,
        action: &str,
        details: &[(&str, &str)],
        ref_path: Option<&str>,
    ) -> Result<(), String> {
        let mut parts = vec![
            chrono::Utc::now().to_rfc3339(),
            Self::log_field(actor),
            Self::log_field(action),
        ];
        for &(k, v) in details {
            if !v.is_empty() {
                parts.push(format!("{}={}", Self::log_field(k), Self::log_field(v)));
            }
        }
        if let Some(r) = ref_path {
            parts.push(format!("ref={}", Self::log_field(r)));
        }
        let line = format!("{}\n", parts.join(" "));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.abs(ACTIVITY_LOG))
            .await
            .map_err(|e| e.to_string())?;
        file.write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn recent_log(&self, lines_count: usize) -> Vec<String> {
        let content = fs::read_to_string(self.abs(ACTIVITY_LOG))
            .await
            .unwrap_or_default();
        let all: Vec<String> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect();
        all.iter().rev().take(lines_count).rev().cloned().collect()
    }

    pub fn resolve_caller_name(
        &self,
        state: &TowerState,
        agent_id: &str,
    ) -> Result<String, String> {
        if agent_id == "main" {
            return Ok(TOWER_NAME.into());
        }
        // The latest registration wins: agent ids restart per session, so a
        // stale entry carrying the same id can still sit in the roster, and a
        // find-first lookup would resolve the caller to that dead agent's name
        // (v2 `resolveAgent`).
        if let Some(entry) = state
            .roster
            .agents
            .iter()
            .rev()
            .find(|a| a.agent_id == agent_id)
        {
            return Ok(entry.name.clone());
        }
        Err(format!(
            "agent \"{agent_id}\" is not a tower participant — only spawned workers/reviewers and the tower can use tower tools"
        ))
    }

    pub fn find_agent(&self, state: &TowerState, name: &str) -> Option<TowerRosterEntry> {
        state.roster.agents.iter().find(|a| a.name == name).cloned()
    }

    /// Upstream v2 records `userCancellationReason().message` — "Aborted by the
    /// user" — while the fork's own abort path records "user cancelled"; both
    /// mean the human stopped the agent, not that it died on its own.
    pub fn is_stopped_by_user(entry: &TowerRosterEntry) -> bool {
        const UPSTREAM_USER_CANCELLATION_REASON: &str = "Aborted by the user";
        entry.died_at.is_some()
            && entry.death_reason.as_deref().is_some_and(|reason| {
                let reason = reason.trim();
                reason.eq_ignore_ascii_case(crate::tools::tower::TOWER_USER_CANCELLATION_REASON)
                    || reason.eq_ignore_ascii_case(UPSTREAM_USER_CANCELLATION_REASON)
            })
    }

    pub async fn register_spawned_agent(
        &self,
        entry: TowerRosterEntry,
        mission: Option<(&str, &str)>,
    ) -> Result<(), String> {
        let mission = mission.map(|(id, owner)| (id.to_string(), owner.to_string()));
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            self.register_agent_locked(entry, &mut state)?;
            if let Some((id, owner)) = mission {
                self.update_mission_in_state(
                    TOWER_NAME,
                    &id,
                    TowerMissionPatch {
                        status: Some(TowerMissionStatus::Active),
                        owner: Some(owner),
                        ..Default::default()
                    },
                    state,
                    MissionUpdateOptions::default(),
                )
                .await?;
            } else {
                self.save(&state).await?;
            }
            Ok(())
        })
        .await
    }

    pub async fn register_agent(&self, entry: TowerRosterEntry) -> Result<(), String> {
        self.with_state_lock(move || async move {
            let mut state = self.load().await?;
            self.register_agent_locked(entry, &mut state)?;
            self.save(&state).await
        })
        .await
    }

    fn register_agent_locked(
        &self,
        entry: TowerRosterEntry,
        state: &mut TowerState,
    ) -> Result<(), String> {
        // Retire same-agent-id entries before appending: a re-registered id
        // must leave exactly one live entry, or every later lookup resolves
        // through the stale one.
        state.roster.agents.retain(|a| a.agent_id != entry.agent_id);
        if self.find_agent(state, &entry.name).is_some() {
            return Err(format!(
                "tower agent name \"{}\" is already registered",
                entry.name
            ));
        }
        state.roster.agents.push(entry);
        Ok(())
    }

    /// Flag a roster entry as dead after its detached run ended in a failure
    /// outcome (failed, timed out, killed, or lost). Idempotent: an entry that
    /// was already retired or re-registered under a new id is left alone.
    pub async fn mark_agent_dead(
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
            if session_id.is_some() && state.session_id.is_some() && state.session_id != session_id
            {
                return Ok(None);
            }
            let Some(index) = state
                .roster
                .agents
                .iter()
                .rposition(|a| a.agent_id == agent_id)
            else {
                return Ok(None);
            };
            let existing = &state.roster.agents[index];
            if existing.died_at.is_some() {
                return Ok(Some(existing.clone()));
            }
            let entry = TowerRosterEntry {
                died_at: Some(chrono::Utc::now().to_rfc3339()),
                death_status: Some(status.clone()),
                death_reason: reason.clone(),
                status: Some("dead".into()),
                ..existing.clone()
            };
            state.roster.agents[index] = entry.clone();
            self.save(&state).await?;
            let mission = state
                .missions
                .iter()
                .find(|m| Some(&m.id) == entry.mission_id.as_ref());
            let mission_ref =
                mission.map(|m| format!("{MISSIONS_DIR}/{}", mission_file_name(&m.id, &m.slug)));
            let reason_log = reason
                .as_deref()
                .map(|r| Self::log_field(r).chars().take(200).collect::<String>());
            let session_log = session_id.clone().unwrap_or_default();
            let pid_log = std::process::id().to_string();
            self.append_log(
                TOWER_NAME,
                "died",
                &[
                    ("name", entry.name.as_str()),
                    ("agent", agent_id.as_str()),
                    (
                        "kind",
                        match entry.kind {
                            crate::tools::tower::types::TowerAgentKind::Worker => "worker",
                            crate::tools::tower::types::TowerAgentKind::Reviewer => "reviewer",
                        },
                    ),
                    ("status", status.as_str()),
                    ("reason", reason_log.as_deref().unwrap_or("")),
                    ("session", session_log.as_str()),
                    ("pid", pid_log.as_str()),
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
            if session_id.is_some() && state.session_id.is_some() && state.session_id != session_id
            {
                return Ok(false);
            }
            let Some(index) = state
                .roster
                .agents
                .iter()
                .rposition(|a| a.agent_id == agent_id)
            else {
                return Ok(false);
            };
            let existing = &state.roster.agents[index];
            if existing.died_at.is_none() {
                return Ok(false);
            }
            let entry = TowerRosterEntry {
                died_at: None,
                death_status: None,
                death_reason: None,
                status: None,
                ..existing.clone()
            };
            state.roster.agents[index] = entry.clone();
            self.save(&state).await?;
            let session_log = session_id.clone().unwrap_or_default();
            let pid_log = std::process::id().to_string();
            self.append_log(
                TOWER_NAME,
                "revived",
                &[
                    ("name", entry.name.as_str()),
                    ("agent", agent_id.as_str()),
                    ("session", session_log.as_str()),
                    ("pid", pid_log.as_str()),
                ],
                None,
            )
            .await?;
            Ok(true)
        })
        .await
    }

    pub async fn plan(&self, input: &[TowerPlanInput]) -> Result<Vec<TowerMission>, String> {
        let input = input.to_vec();
        self.with_state_lock(move || async move { self.plan_locked(&input).await })
            .await
    }

    async fn plan_locked(&self, input: &[TowerPlanInput]) -> Result<Vec<TowerMission>, String> {
        if input.is_empty() {
            return Err("TowerPlan needs at least one mission".into());
        }
        let mut state = self.load().await?;
        let start_index = state.missions.len();

        // Slugs feed the branch name. `slugify` keeps ASCII alphanumerics only,
        // so every non-Latin title collapses to the same fallback (`item`) and
        // every repeated title also collides — two missions then produced the
        // same `feat/<slug>` branch, and the second `git worktree add` failed
        // with "already checked out". Deduplicate against the existing roster and
        // within this batch.
        let mut taken_slugs: std::collections::HashSet<String> =
            state.missions.iter().map(|m| m.slug.clone()).collect();

        let mut missions: Vec<TowerMission> = Vec::with_capacity(input.len());
        for (index, item) in input.iter().enumerate() {
            let n = start_index + index + 1;
            let slug = unique_slug(&slugify(&item.title, 40), &mut taken_slugs);
            missions.push(TowerMission {
                id: format!("M{n}"),
                title: item.title.clone(),
                slug: slug.clone(),
                kind: item.kind.unwrap_or_default(),
                scope: item.scope.clone(),
                branch: format!("feat/{slug}"),
                worktree: format!("wt-{n}"),
                spawn_base: None,
                deps: item.deps.clone().unwrap_or_default(),
                status: TowerMissionStatus::Planned,
                context: None,
                tasks: item
                    .tasks
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|text| crate::tools::tower::types::TowerMissionTask {
                        text,
                        done: false,
                        dropped: false,
                    })
                    .collect(),
                notes: Vec::new(),
                blockers: Vec::new(),
                owner: None,
            });
        }

        let known_ids: std::collections::HashSet<String> = state
            .missions
            .iter()
            .map(|m| m.id.clone())
            .chain(missions.iter().map(|m| m.id.clone()))
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

        let existing_open_count = state
            .missions
            .iter()
            .filter(|mission| mission.status.is_open())
            .count();
        let mut all_open: Vec<TowerMission> = state
            .missions
            .iter()
            .filter(|mission| mission.status.is_open())
            .cloned()
            .chain(missions)
            .collect();
        for index in existing_open_count..all_open.len() {
            add_auto_dependencies(&mut all_open, index);
        }
        assert_scopes_disjoint(&all_open)?;
        let missions = all_open.split_off(existing_open_count);

        state.missions.extend(missions.clone());
        self.save(&state).await?;
        self.render_missions_index(&state).await?;
        for mission in &missions {
            self.render_mission_file(mission).await?;
        }

        let ids = missions
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(",");
        self.append_log(
            TOWER_NAME,
            "plan",
            &[("missions", &ids)],
            Some(MISSIONS_INDEX),
        )
        .await?;

        Ok(missions)
    }

    pub async fn update_mission(
        &self,
        caller_name: &str,
        id: &str,
        patch: TowerMissionPatch,
    ) -> Result<TowerMission, String> {
        if patch.status != Some(TowerMissionStatus::Completed) {
            let caller_name = caller_name.to_string();
            let id = id.to_string();
            return self
                .with_state_lock(move || async move {
                    self.update_mission_locked(&caller_name, &id, patch).await
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
        self.run_prepared(
            move || {
                let store = prepare_store.clone();
                let caller = prepare_caller.clone();
                let id = prepare_id.clone();
                let patch = prepare_patch.clone();
                async move {
                    let snapshot = store.load_snapshot().await?;
                    let inbox_files = store.protocol_file_names(INBOX_DIR, "").await?;
                    let inbox_items = store.read_inbox(TOWER_NAME, usize::MAX).await?;
                    let raw = serde_json::from_str(&snapshot.raw)
                        .map_err(|error| format!("corrupted tower state: {error}"))?;
                    store
                        .update_mission_in_state(
                            &caller,
                            &id,
                            patch,
                            snapshot.state.clone(),
                            MissionUpdateOptions {
                                validate_only: true,
                                prepared_inbox: Some((raw, inbox_items)),
                                ..Default::default()
                            },
                        )
                        .await?;
                    Ok((snapshot, inbox_files))
                }
            },
            move |(snapshot, inbox_files)| {
                let store = commit_store.clone();
                let caller = commit_caller.clone();
                let id = commit_id.clone();
                let patch = patch.clone();
                async move {
                    let lock_store = store.clone();
                    lock_store
                        .with_completion_lock(move || async move {
                            let state = store
                                .assert_preparation_current(
                                    &snapshot,
                                    Some(inbox_files.as_slice()),
                                    None,
                                )
                                .await?;
                            store
                                .update_mission_in_state(
                                    &caller,
                                    &id,
                                    patch,
                                    state,
                                    MissionUpdateOptions {
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
        patch: TowerMissionPatch,
    ) -> Result<TowerMission, String> {
        let state = self.load().await?;
        self.update_mission_in_state(
            caller_name,
            id,
            patch,
            state,
            MissionUpdateOptions::default(),
        )
        .await
    }

    async fn update_mission_in_state(
        &self,
        caller_name: &str,
        id: &str,
        patch: TowerMissionPatch,
        mut state: TowerState,
        options: MissionUpdateOptions,
    ) -> Result<TowerMission, String> {
        if !state.missions.iter().any(|m| m.id == id) {
            return Err(format!("unknown mission \"{id}\""));
        }

        if caller_name != TOWER_NAME {
            let caller = self.find_agent(&state, caller_name);
            if caller.as_ref().and_then(|c| c.mission_id.as_deref()) != Some(id) {
                return Err(format!(
                    "agent \"{caller_name}\" does not own mission {id} — workers update only their own mission file"
                ));
            }
        }

        if patch.owner.is_some() && caller_name != TOWER_NAME {
            return Err(format!(
                "agent \"{caller_name}\" cannot assign mission ownership — only the tower sets owner"
            ));
        }

        let mut scope_reconciliation = None;
        if let Some(ref scope) = patch.scope {
            if caller_name != TOWER_NAME {
                return Err(format!(
                    "agent \"{caller_name}\" cannot change mission scope — only the tower widens a scope, and every change is logged"
                ));
            }
            let mut test_missions = state
                .missions
                .iter()
                .filter(|mission| mission.id != id && mission.status.is_open())
                .cloned()
                .collect::<Vec<_>>();
            let mut patched = state
                .missions
                .iter()
                .find(|mission| mission.id == id)
                .cloned()
                .expect("mission existence checked above");
            patched.scope = scope.clone();
            test_missions.push(patched);
            let target_index = test_missions.len() - 1;
            add_auto_dependencies(&mut test_missions, target_index);
            assert_scopes_disjoint(&test_missions)?;
            let reconciled = test_missions.pop().expect("patched mission was appended");
            scope_reconciliation = Some((reconciled.scope, reconciled.deps, reconciled.notes));
        }

        if patch.owner.is_some() {
            let current_owner = state
                .missions
                .iter()
                .find(|m| m.id == id)
                .and_then(|m| m.owner.as_deref());
            if current_owner
                .and_then(|owner| self.find_agent(&state, owner))
                .as_ref()
                .is_some_and(Self::is_stopped_by_user)
            {
                return Err(format!(
                    "mission {id} owner was stopped by the user — never resume, reassign, or replace it unless the human explicitly asks"
                ));
            }
        }

        let current_status = state
            .missions
            .iter()
            .find(|mission| mission.id == id)
            .expect("mission existence checked above")
            .status;
        if patch.status == Some(TowerMissionStatus::Merged)
            && current_status != TowerMissionStatus::Merged
        {
            return Err(format!(
                "mission {id} cannot be marked merged through TowerMission — use TowerMerge so the merge gates and git state stay transactional"
            ));
        }
        if !current_status.is_open()
            && (patch.status.is_some_and(|status| status != current_status)
                || patch.blocker.is_some())
        {
            return Err(format!(
                "merged and abandoned are terminal states — mission {id} cannot change status or gain blockers"
            ));
        }

        let caller_entry = self.find_agent(&state, caller_name);
        let mission = state
            .missions
            .iter_mut()
            .find(|m| m.id == id)
            .expect("mission existence checked above");

        if let Some(ref owner) = patch.owner {
            mission.owner = Some(owner.clone());
        }

        if let Some((scope, deps, notes)) = scope_reconciliation {
            mission.scope = scope;
            mission.deps = deps;
            mission.notes = notes;
        }

        if let Some(status) = patch.status {
            if status == TowerMissionStatus::Abandoned && caller_name != TOWER_NAME {
                return Err(format!(
                    "agent \"{caller_name}\" cannot abandon mission {id} — abandoning releases the mission scope, so only the tower does it"
                ));
            }
            mission.status = status;
        }

        if let Some(ref note) = patch.note {
            mission.notes.push(note.clone());
        }

        if let Some(ref blocker) = patch.blocker {
            mission.blockers.push(blocker.clone());
            mission.status = TowerMissionStatus::Blocked;
        }

        if patch.clear_blockers == Some(true) {
            mission.blockers.clear();
        }

        if let Some(ref task_done_text) = patch.task_done {
            let task = mission
                .tasks
                .iter_mut()
                .find(|t| !t.done && !t.dropped && t.text.contains(task_done_text));
            match task {
                Some(t) => t.done = true,
                None => {
                    return Err(format!(
                        "mission {id} has no open task matching \"{task_done_text}\""
                    ));
                }
            }
        }

        let mut task_drop_log: Option<String> = None;
        if let Some(ref drop) = patch.task_drop {
            // v2 #3976: the drop is the escape hatch for legitimately
            // descoped work, so the reason is mandatory and auditable.
            let reason = drop.reason.as_deref().unwrap_or("").trim();
            if reason.is_empty() {
                return Err(format!(
                    "dropping a task from mission {id} requires a reason — the drop is the escape hatch for legitimately descoped work, and the reason is recorded in the mission notes and the activity log for audit"
                ));
            }
            // An empty needle matches every task (`str::contains("")`), which
            // would silently drop the first open one — reject it instead.
            let needle = drop.text.trim();
            if needle.is_empty() {
                return Err(format!(
                    "dropping a task from mission {id} requires the task text — pass task_drop with the task to drop"
                ));
            }
            let Some(task) = mission
                .tasks
                .iter_mut()
                .find(|t| !t.done && !t.dropped && t.text.contains(needle))
            else {
                return Err(format!(
                    "mission {id} has no open task matching \"{needle}\""
                ));
            };
            task.dropped = true;
            let log = format!("dropped task \"{}\": {reason}", task.text);
            mission.notes.push(log.clone());
            task_drop_log = Some(log);
        }

        // v2 `assertCompletable` (#3976): completing with open tasks or a
        // zero-diff build branch is refused outright. Runs after the task
        // mutations so ticking/dropping the last task in the same patch
        // passes.
        if patch.status == Some(TowerMissionStatus::Completed)
            && patch.blocker.is_none()
            && !options.completion_prepared
        {
            let patched = mission.clone();
            let base = state.base.clone();
            self.assert_completable(&patched, &base).await?;
            if caller_name != TOWER_NAME && caller_entry.is_some() {
                self.assert_inbox_read(caller_name, &patched, options.prepared_inbox)
                    .await?;
            }
        }

        let updated = mission.clone();
        if options.validate_only {
            return Ok(updated);
        }
        self.save(&state).await?;
        self.render_missions_index(&state).await?;
        self.render_mission_file(&updated).await?;

        let status_str = patch
            .status
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();
        let mut log_fields: Vec<(&str, &str)> = vec![("id", id), ("status", &status_str)];
        if let Some(ref dropped) = task_drop_log {
            log_fields.push(("task_drop", dropped.as_str()));
        }
        self.append_log(caller_name, "mission.update", &log_fields, None)
            .await?;

        Ok(updated)
    }

    /// v2 `assertCompletable` (#3976): a mission may not flip to `completed`
    /// while open tasks remain or its build branch carries no work. Survey
    /// missions are exempt — they are read-only and close with a zero-diff
    /// merge.
    async fn assert_completable(&self, mission: &TowerMission, base: &str) -> Result<(), String> {
        let open: Vec<&TowerMissionTask> = mission
            .tasks
            .iter()
            .filter(|t| !t.done && !t.dropped)
            .collect();
        if !open.is_empty() {
            return Err(format!(
                "mission {} cannot transition to completed — {} open task(s): {}; tick finished tasks with task_done, or drop legitimately descoped ones with task_drop (a reason is mandatory and lands in the mission notes and the activity log)",
                mission.id,
                open.len(),
                open.iter()
                    .map(|t| format!("\"{}\"", t.text))
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        if !mission.blockers.is_empty() {
            return Err(format!(
                "mission {} cannot transition to completed — {} blocker(s) remain: {}; clear them with clear_blockers after resolving them",
                mission.id,
                mission.blockers.len(),
                mission.blockers.join(", "),
            ));
        }
        if mission.kind == TowerMissionKind::Survey {
            if !branch_exists(&self.repo_root, &mission.branch).await {
                return Ok(());
            }
            let base = self.diff_base(base, mission).await?;
            let changed = diff_name_only(&self.repo_root, &base, &mission.branch).await?;
            if !changed.is_empty() {
                return Err(format!(
                    "mission {} cannot transition to completed — survey missions are read-only and require a zero-diff branch, but \"{}\" changed {} file(s): {}",
                    mission.id,
                    mission.branch,
                    changed.len(),
                    changed.join(", "),
                ));
            }
            return Ok(());
        }
        if !branch_exists(&self.repo_root, &mission.branch).await {
            return Err(format!(
                "mission {} cannot transition to completed — its branch \"{}\" is not available (missing, or git cannot resolve it), so no work has landed; a build mission must produce a diff on its branch: spawn a worker to do the work, or have the tower abandon the mission (status=abandoned) if it is no longer needed",
                mission.id, mission.branch,
            ));
        }
        // An undecidable ancestry check is not "not an ancestor": falling back to
        // the tower base on a git error would judge the diff against the wrong
        // base, so the completion is refused instead.
        let base = self.diff_base(base, mission).await?;
        let changed = diff_name_only(&self.repo_root, &base, &mission.branch).await?;
        if changed.is_empty() {
            return Err(format!(
                "mission {} cannot transition to completed — branch \"{}\" has no changes vs \"{base}\"; a build mission must produce a diff on its branch: commit the work there first, or if the work turned out unnecessary, have the tower abandon the mission (status=abandoned) instead",
                mission.id, mission.branch,
            ));
        }
        Ok(())
    }

    /// v2 `diffBase`: the mission's spawn base wins while it is still an
    /// ancestor of the mission branch; otherwise the tower base does. An
    /// undecidable check propagates: this base decides whether the mission has
    /// any work at all, so guessing it would be the wrong answer to give.
    async fn diff_base(&self, base: &str, mission: &TowerMission) -> Result<String, String> {
        match mission.spawn_base {
            Some(ref spawn_base) => {
                if is_ancestor(&self.repo_root, spawn_base, &mission.branch).await? {
                    Ok(spawn_base.clone())
                } else {
                    Ok(base.to_string())
                }
            }
            None => Ok(base.to_string()),
        }
    }

    pub async fn send(&self, caller_name: &str, input: TowerSendInput) -> Result<String, String> {
        let caller_name = caller_name.to_string();
        self.with_inbox_publication_lock(move || async move {
            self.send_locked(&caller_name, input).await
        })
        .await
    }

    async fn send_locked(
        &self,
        caller_name: &str,
        input: TowerSendInput,
    ) -> Result<String, String> {
        let sent = self.send_detailed_locked(caller_name, input, false).await?;
        if let Some(error) = sent.activity_log_error {
            return Err(error);
        }
        Ok(sent.item.file)
    }

    pub async fn read_inbox(
        &self,
        caller_name: &str,
        limit: usize,
    ) -> Result<Vec<TowerInboxItem>, String> {
        let dir = self.abs(INBOX_DIR);
        let mut read_dir = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            // A missing inbox is genuinely empty (nothing has been sent yet).
            // Any other failure is surfaced: presenting a permission or IO error
            // as "inbox empty" makes broken shared state look healthy.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!(
                    "could not read the tower inbox at {}: {error}",
                    dir.display()
                ));
            }
        };

        let mut entries = Vec::new();
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| e.to_string())? {
            if entry.path().extension().and_then(|s| s.to_str()) == Some("md") {
                entries.push(entry);
            }
        }
        let loaded =
            map_with_concurrency(entries, TOWER_FILE_IO_CONCURRENCY, |_, entry| async move {
                let Ok(text) = fs::read_to_string(entry.path()).await else {
                    return None;
                };
                let (fields, body) = parse_frontmatter(&text);
                if fields.get("type").map(|s| s.as_str()) != Some("inbox") {
                    return None;
                }
                let to = fields.get("to").cloned().unwrap_or_default();
                if caller_name != TOWER_NAME && to != caller_name && to != BROADCAST_NAME {
                    return None;
                }
                let file = format!("{INBOX_DIR}/{}", entry.file_name().to_string_lossy());
                let message_id = fields
                    .get("message_id")
                    .cloned()
                    .unwrap_or_else(|| file.clone());
                Some(TowerInboxItem {
                    message_id,
                    file,
                    from: fields
                        .get("from")
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                    to,
                    subject: fields.get("subject").cloned().unwrap_or_default(),
                    sent_at: fields.get("sent_at").cloned().unwrap_or_default(),
                    scope: fields.get("scope").cloned(),
                    action: fields.get("action").cloned(),
                    consent_ref: fields.get("consent_ref").cloned(),
                    body,
                })
            })
            .await;
        let mut items: Vec<_> = loaded.into_iter().flatten().collect();

        items.sort_by(|a, b| b.sent_at.cmp(&a.sent_at).then_with(|| a.file.cmp(&b.file)));
        items.truncate(limit.max(1));
        Ok(items)
    }

    pub async fn mark_inbox_read(
        &self,
        caller_name: &str,
        newest_seen_sent_at: String,
    ) -> Result<(), String> {
        let caller_name = caller_name.to_string();
        self.with_state_lock(move || async move {
            let mut raw = self.load_state_value().await?;
            let Some(agent) = raw
                .get_mut("roster")
                .and_then(|roster| roster.get_mut("agents"))
                .and_then(serde_json::Value::as_array_mut)
                .and_then(|agents| {
                    agents.iter_mut().find(|agent| {
                        agent.get("name").and_then(serde_json::Value::as_str)
                            == Some(caller_name.as_str())
                    })
                })
            else {
                return Ok(());
            };
            let current = agent
                .get("lastInboxReadAt")
                .or_else(|| agent.get("spawnedAt"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if current >= newest_seen_sent_at.as_str() {
                return Ok(());
            }
            agent["lastInboxReadAt"] = serde_json::Value::String(newest_seen_sent_at);
            self.save_state_value(&raw).await
        })
        .await
    }

    async fn assert_inbox_read(
        &self,
        caller_name: &str,
        mission: &TowerMission,
        prepared: Option<(serde_json::Value, Vec<TowerInboxItem>)>,
    ) -> Result<(), String> {
        let unread = match prepared {
            Some((raw, items)) => {
                messaging::unread_inbox_count_from_items(&raw, caller_name, &items)
            }
            None => self.unread_inbox_count_locked(caller_name).await?,
        };
        if unread == 0 {
            return Ok(());
        }
        Err(format!(
            "mission {} cannot transition to completed — {unread} unread inbox message(s) for {caller_name} arrived since the inbox was last read; call TowerInbox, incorporate anything new into the delivery, then retry status=completed",
            mission.id,
        ))
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
            .and_then(|c| c.mission_id.as_deref())
            .and_then(|mid| state.missions.iter().find(|m| m.id == mid));

        let mission_desc = match mission {
            Some(m) => format!("{} — {}", m.id, m.title),
            None => "(none)".to_string(),
        };

        let not_fixed_reason = match mission {
            Some(m) => format!(
                "This finding is outside the scope of mission {} ({}). Fixing it directly would violate scope isolation. Assigning to the control tower for routing.",
                m.id,
                m.scope.join(", ")
            ),
            None => "This finding is outside the reporting agent's assignment. Assigning to the control tower for routing.".to_string(),
        };

        let mut lines = vec![
            format!("# Finding: {}", input.title),
            String::new(),
            format!("**Date**: {}", date_dash().replace('-', "")),
            format!("**Agent**: {caller_name}"),
            format!("**Type**: {}", input.r#type.as_str()),
            format!(
                "**Severity**: {}",
                input.severity.unwrap_or_default().as_str()
            ),
            format!("**Mission**: {mission_desc}"),
        ];
        lines.extend([
            String::new(),
            "---".into(),
            String::new(),
            "## Summary".into(),
            input.summary.trim().into(),
            String::new(),
            "## Location".into(),
            input
                .location
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
        ]);

        let file_name = finding_file_name(caller_name, input.r#type.as_str(), &input.title);
        let rel = format!("{FINDINGS_DIR}/{file_name}");
        let written = self.write_unique(&rel, &lines.join("\n")).await?;

        self.append_log(
            caller_name,
            "finding.file",
            &[
                ("type", input.r#type.as_str()),
                ("slug", &slugify(&input.title, 60)),
            ],
            Some(&written),
        )
        .await?;

        Ok(written)
    }

    async fn next_review_seq(&self) -> Result<u64, String> {
        let dir = self.abs(REVIEWS_DIR);
        let mut read_dir = match fs::read_dir(&dir).await {
            Ok(read_dir) => read_dir,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(1),
            Err(error) => {
                return Err(format!(
                    "could not read the tower reviews directory at {}: {error}",
                    dir.display()
                ));
            }
        };
        let mut paths = Vec::new();
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| e.to_string())? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("review-") && name.ends_with(".md") {
                paths.push(entry.path());
            }
        }
        let sequences =
            map_with_concurrency(paths, TOWER_FILE_IO_CONCURRENCY, |_, path| async move {
                let text = read_frontmatter_prefix(path).await.ok()?;
                parse_frontmatter(&text)
                    .0
                    .get("seq")
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .await;
        let maximum = sequences.into_iter().flatten().max().unwrap_or(0);
        Ok(maximum + 1)
    }

    pub async fn submit_review(
        &self,
        caller_name: &str,
        input: TowerReviewInput,
    ) -> Result<String, String> {
        self.submit_review_detailed(caller_name, input)
            .await
            .map(|result| result.file)
    }

    pub async fn submit_review_detailed(
        &self,
        caller_name: &str,
        input: TowerReviewInput,
    ) -> Result<crate::tools::tower::types::TowerReviewResult, String> {
        let prepare_store = self.clone();
        let prepare_caller = caller_name.to_string();
        let prepare_input = input.clone();
        let commit_store = self.clone();
        let commit_caller = caller_name.to_string();
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
                let input = input.clone();
                async move {
                    let lock_store = store.clone();
                    lock_store
                        .with_state_lock(move || async move {
                            let state = store
                                .assert_preparation_current(
                                    &preparation.snapshot,
                                    Some(preparation.inbox_files.as_slice()),
                                    Some(preparation.review_files.as_slice()),
                                )
                                .await?;
                            if branch_tip(&store.repo_root, &input.target).await?
                                != preparation.reviewed_commit
                            {
                                return Err(PREPARATION_CONFLICT.into());
                            }
                            store
                                .submit_review_prepared_locked(&caller, &input, preparation, state)
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
        let caller = self.assert_review_submission(caller_name, input, &snapshot.state)?;
        let existing = self.reviews_for(&input.target).await.map_err(|error| {
            format!("could not list existing reviews for round numbering: {error}")
        })?;
        let my_rounds = existing
            .iter()
            .filter(|review| review.reviewer == caller_name)
            .count() as u32;
        // v2 #3976: cap the rework loop. Past the cap the same reviewer is
        // not converging, so the fix is to reassign or descope, not round 6.
        if my_rounds >= MAX_REVIEW_ROUNDS {
            return Err(format!(
                "branch \"{}\" has already been through {MAX_REVIEW_ROUNDS} review rounds by \"{caller_name}\" — the rework loop is not converging, so another round from the same reviewer is refused; redirect instead: reassign the work (spawn a different worker or a fresh reviewer), split the mission into smaller pieces, or descope it (TowerMission status=abandoned)",
                input.target,
            ));
        }
        let review_mission = if caller_name == TOWER_NAME {
            snapshot
                .state
                .missions
                .iter()
                .rev()
                .find(|mission| mission.branch == input.target && mission.status.is_open())
                .map(|mission| mission.id.clone())
        } else {
            caller.and_then(|agent| agent.review_mission_id)
        };
        Ok(ReviewPreparation {
            snapshot,
            inbox_files: self.protocol_file_names(INBOX_DIR, "").await?,
            review_files: self.protocol_file_names(REVIEWS_DIR, "review-").await?,
            round: my_rounds + 1,
            reviewed_commit: branch_tip(&self.repo_root, &input.target).await?,
            seq: self.next_review_seq().await?,
            review_mission,
        })
    }

    fn assert_review_submission(
        &self,
        caller_name: &str,
        input: &TowerReviewInput,
        state: &TowerState,
    ) -> Result<Option<TowerRosterEntry>, String> {
        let caller = self.find_agent(state, caller_name);
        if caller_name != TOWER_NAME
            && (caller.as_ref().map(|c| c.kind)
                != Some(crate::tools::tower::types::TowerAgentKind::Reviewer)
                || caller.as_ref().and_then(|c| c.review_target.as_deref()) != Some(&input.target))
        {
            return Err(format!(
                "agent \"{caller_name}\" is not an assigned reviewer for \"{}\"",
                input.target
            ));
        }

        let status_regex = regex::Regex::new(r"^(clean|p[12]-\d+items)$").unwrap();
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

    async fn submit_review_prepared_locked(
        &self,
        caller_name: &str,
        input: &TowerReviewInput,
        preparation: ReviewPreparation,
        mut state: TowerState,
    ) -> Result<crate::tools::tower::types::TowerReviewResult, String> {
        let round = preparation.round;
        let reviewed_commit = preparation.reviewed_commit.clone();
        let seq = preparation.seq;
        let round_str = round.to_string();
        let seq_str = seq.to_string();
        let mut frontmatter_fields = vec![
            ("date", date_dash()),
            ("reviewer", caller_name.to_string()),
            ("target", input.target.clone()),
            ("round", round_str.clone()),
            ("status", input.status.clone()),
            ("merge", input.merge.clone()),
            ("reviewed_commit", reviewed_commit.clone()),
            ("seq", seq_str),
        ];
        if let Some(mission) = preparation.review_mission.clone() {
            frontmatter_fields.push(("mission", mission));
        }
        let frontmatter = render_frontmatter(
            &frontmatter_fields
                .iter()
                .map(|(k, v)| (*k, v.as_str()))
                .collect::<Vec<_>>(),
        )?;

        let checks_text = match input.checks {
            Some(ref list) if !list.is_empty() => list
                .iter()
                .map(|c| format!("- [x] {c}"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "- [x] (reviewer reported no formal checks)".into(),
        };

        let content = format!(
            "{frontmatter}\n\n## Findings\n\n{}\n\n## Checks\n\n{checks_text}\n\n## Decision\n\n{}\n",
            input.findings.trim(),
            input.decision.trim()
        );

        let file_name = review_file_name(&input.target, caller_name, round);
        let rel = format!("{REVIEWS_DIR}/{file_name}");
        let written = self.write_unique(&rel, &content).await?;

        let rev_short = if reviewed_commit.len() >= 7 {
            &reviewed_commit[..7]
        } else {
            &reviewed_commit
        };
        self.append_log(
            caller_name,
            "review.write",
            &[
                ("target", &input.target),
                ("round", &round_str),
                ("verdict", &input.status),
                ("reviewed", rev_short),
            ],
            Some(&written),
        )
        .await?;

        // v2 #3976: a non-clean verdict against a mission the reviewer tracks
        // re-opens that completed mission so the rework loop can resume.
        if input.status != "clean" {
            let caller = self.find_agent(&state, caller_name);
            let reviewed_branch = &input.target;
            // A recorded `review_mission_id` only counts when that mission
            // actually owns the reviewed branch; a stale roster entry must not
            // be able to flip an unrelated mission.
            let recorded = caller
                .and_then(|c| c.review_mission_id.clone())
                .filter(|id| {
                    state
                        .missions
                        .iter()
                        .any(|m| &m.id == id && &m.branch == reviewed_branch)
                });
            // Prefer the recorded id, then the live mission on that branch, and
            // only then a closed one: with two missions sharing a branch, the
            // first array match may be the historical record, not the work that
            // is actually under review.
            let rework_idx = recorded
                .and_then(|id| {
                    state
                        .missions
                        .iter()
                        .position(|m| m.id == id && m.status == TowerMissionStatus::Completed)
                })
                .or_else(|| {
                    state.missions.iter().position(|m| {
                        m.branch == *reviewed_branch && m.status != TowerMissionStatus::Merged
                    })
                })
                .or_else(|| {
                    state
                        .missions
                        .iter()
                        .position(|m| m.branch == *reviewed_branch)
                });
            if let Some(idx) = rework_idx
                && state.missions[idx].status == TowerMissionStatus::Completed
            {
                state.missions[idx].status = TowerMissionStatus::Active;
                let mission = state.missions[idx].clone();
                self.save(&state).await?;
                self.render_missions_index(&state).await?;
                self.render_mission_file(&mission).await?;
                self.append_log(
                    caller_name,
                    "mission.rework",
                    &[("id", &mission.id), ("verdict", &input.status)],
                    Some(&format!(
                        "{MISSIONS_DIR}/{}",
                        mission_file_name(&mission.id, &mission.slug)
                    )),
                )
                .await?;
            }
        }

        let review = self
            .latest_review(&input.target)
            .await?
            .expect("the newly written review must be readable");
        let (notifications, notification_errors) =
            self.notify_review_result_locked(caller_name, &review).await;
        Ok(crate::tools::tower::types::TowerReviewResult {
            file: written,
            review,
            notifications,
            notification_errors,
        })
    }

    pub async fn reviews_for(&self, target: &str) -> Result<Vec<TowerReviewInfo>, String> {
        let dir = self.abs(REVIEWS_DIR);
        let mut read_dir = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            // A missing reviews directory is legitimately empty; anything else is
            // an error, because "no review found" would otherwise let an unreviewed
            // merge through a gate that believes it looked.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(format!(
                    "could not read the tower reviews directory at {}: {error}",
                    dir.display()
                ));
            }
        };

        let prefix = format!("review-{}-", target_slug(target));
        let mut entries = Vec::new();
        while let Some(entry) = read_dir.next_entry().await.map_err(|e| e.to_string())? {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.ends_with(".md") {
                entries.push(entry);
            }
        }
        let loaded =
            map_with_concurrency(entries, TOWER_FILE_IO_CONCURRENCY, |_, entry| async move {
                let name = entry.file_name().to_string_lossy().to_string();
                let text = read_frontmatter_prefix(entry.path()).await.ok()?;
                let (fields, _) = parse_frontmatter(&text);
                let round = fields.get("round")?.parse::<u32>().ok()?;
                if round == 0 {
                    return None;
                }
                // The file-name prefix is a lossy key: `feat/foo` and `feat-foo`
                // slug to the same prefix, and a long branch shares one with its
                // truncation. Only a review whose own frontmatter names this exact
                // branch counts, so one branch's rounds never refuse another's
                // reviewer. A file with no `target` cannot be attributed and is not
                // counted — one redundant round beats a wedged mission.
                if fields.get("target").map(String::as_str) != Some(target) {
                    return None;
                }
                let modified_at_ms = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or(0);
                Some(TowerReviewInfo {
                    reviewer: fields
                        .get("reviewer")
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                    target: fields
                        .get("target")
                        .cloned()
                        .unwrap_or_else(|| target.into()),
                    round,
                    status: fields.get("status").cloned().unwrap_or_default(),
                    merge: fields.get("merge").cloned().unwrap_or_default(),
                    reviewed_commit: fields.get("reviewed_commit").cloned().unwrap_or_default(),
                    date: fields.get("date").cloned().unwrap_or_default(),
                    file: format!("{REVIEWS_DIR}/{name}"),
                    mission: fields.get("mission").cloned(),
                    seq: fields.get("seq").and_then(|value| value.parse().ok()),
                    modified_at_ms,
                })
            })
            .await;
        let mut reviews: Vec<_> = loaded.into_iter().flatten().collect();
        reviews.sort_by_key(|review| {
            (
                review.seq.map(i128::from).unwrap_or(-1),
                review.modified_at_ms,
                review.round,
                review.file.clone(),
            )
        });
        Ok(reviews)
    }

    pub async fn latest_review(&self, target: &str) -> Result<Option<TowerReviewInfo>, String> {
        Ok(self.reviews_for(target).await?.into_iter().last())
    }

    pub async fn merge(&self, branch: &str) -> Result<TowerMergeResult, String> {
        self.merge_with_workers(branch, None).await
    }

    pub async fn merge_with_workers(
        &self,
        branch: &str,
        active_agent_ids: Option<HashSet<String>>,
    ) -> Result<TowerMergeResult, String> {
        let branch = branch.to_string();
        self.with_state_lock(move || async move {
            self.merge_locked(&branch, active_agent_ids.as_ref()).await
        })
        .await
    }

    async fn merge_locked(
        &self,
        branch: &str,
        active_agent_ids: Option<&HashSet<String>>,
    ) -> Result<TowerMergeResult, String> {
        let mut state = self.load().await?;
        // Branch → mission resolution (v2 #3648): every record for the branch
        // closed (merged/abandoned) means a merge would flip a historical
        // mission's state — refuse; the work must land under a fresh mission.
        // Otherwise the latest open record wins.
        let has_any_record = state.missions.iter().any(|m| m.branch == branch);
        let has_open_record = state
            .missions
            .iter()
            .any(|m| m.branch == branch && m.status.is_open());
        if has_any_record && !has_open_record {
            self.append_log(
                TOWER_NAME,
                "merge.blocked",
                &[
                    ("branch", branch),
                    ("reason", "branch-owned-by-closed-missions"),
                ],
                None,
            )
            .await?;
            return Err(format!(
                "merge blocked: every mission record for {branch} is closed (merged or abandoned) — a merge never flips a historical mission's state; re-plan the work under a fresh mission title"
            ));
        }
        let Some(mission_index) = state
            .missions
            .iter()
            .rposition(|mission| mission.branch == branch && mission.status.is_open())
        else {
            return Err(format!("no tower mission owns branch \"{branch}\""));
        };
        let mission_snapshot = state.missions[mission_index].clone();
        let gate = self
            .mission_gate(&state, &mission_snapshot, active_agent_ids)
            .await?;
        if !gate.ready {
            let block = gate.blocks.first().expect("a closed gate has a block");
            self.append_log(
                TOWER_NAME,
                "merge.blocked",
                &[("branch", branch), ("reason", block.reason)],
                None,
            )
            .await?;
            return Err(block.message.clone());
        }
        let observation = gate
            .observations
            .branch
            .clone()
            .expect("a ready gate always observes its branch");
        if state.missions[mission_index].status != TowerMissionStatus::Completed {
            self.append_log(
                TOWER_NAME,
                "merge.blocked",
                &[("branch", branch), ("reason", REASON_NOT_COMPLETED)],
                None,
            )
            .await?;
            return Err(format!(
                "merge blocked: mission {} is {} — completed status is required",
                state.missions[mission_index].id,
                state.missions[mission_index].status.as_str()
            ));
        }
        let unmerged_deps: Vec<String> = state.missions[mission_index]
            .deps
            .iter()
            .filter(|dep| {
                state
                    .missions
                    .iter()
                    .any(|mission| mission.id == **dep && mission.status.is_open())
            })
            .cloned()
            .collect();
        let mission = &mut state.missions[mission_index];

        if !unmerged_deps.is_empty() {
            self.append_log(
                TOWER_NAME,
                "merge.blocked",
                &[("branch", branch), ("reason", "deps-unmerged")],
                None,
            )
            .await?;
            return Err(format!(
                "merge blocked: dependencies not merged yet ({}) — merge in Dependency Flow order",
                unmerged_deps.join(", ")
            ));
        }

        if mission.kind == crate::tools::tower::types::TowerMissionKind::Survey {
            let changed = observation.changed_files.clone();
            if !changed.is_empty() {
                self.append_log(
                    TOWER_NAME,
                    "merge.blocked",
                    &[("branch", branch), ("reason", "read-only-survey")],
                    None,
                )
                .await?;
                return Err(format!(
                    "merge blocked: survey mission {} is read-only but {} has {} changed file(s): {} — investigate the worker; if the changes are worth keeping, move them onto a build mission's branch",
                    mission.id,
                    branch,
                    changed.len(),
                    changed
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            let evaluated_branch_tip = observation.tip.clone();
            let current_branch_tip = branch_tip(&self.repo_root, branch).await?;
            let status = TowerMergeStatus::after(&evaluated_branch_tip, &current_branch_tip, true);
            if status == TowerMergeStatus::Advanced {
                self.append_log(
                    TOWER_NAME,
                    "merge.advanced",
                    &[
                        ("branch", branch),
                        ("evaluated", &evaluated_branch_tip),
                        ("current", &current_branch_tip),
                    ],
                    None,
                )
                .await?;
                return Ok(TowerMergeResult {
                    status,
                    merge_commit: gate.observations.head_tip.clone(),
                    evaluated_branch_tip,
                    current_branch_tip,
                    conflicts_with: Vec::new(),
                    noop: true,
                });
            }
            mission.status = TowerMissionStatus::Merged;
            let m_copy = mission.clone();
            self.save(&state).await?;
            self.render_missions_index(&state).await?;
            self.render_mission_file(&m_copy).await?;
            self.append_log(
                TOWER_NAME,
                "merge.noop",
                &[("branch", branch), ("kind", "survey")],
                None,
            )
            .await?;
            return Ok(TowerMergeResult {
                status,
                merge_commit: gate.observations.head_tip.clone(),
                evaluated_branch_tip,
                current_branch_tip,
                conflicts_with: Vec::new(),
                noop: true,
            });
        }

        let tip = observation.tip.clone();
        let changed = observation.changed_files.clone();
        let mut out_of_scope = Vec::new();
        for file in &changed {
            let in_scope = mission.scope.iter().any(|pattern| {
                if let Ok(glob) = globset::Glob::new(pattern) {
                    glob.compile_matcher().is_match(file)
                } else {
                    file.starts_with(pattern.trim_end_matches('*'))
                }
            });
            if !in_scope {
                out_of_scope.push(file.clone());
            }
        }
        if !out_of_scope.is_empty() {
            self.append_log(
                TOWER_NAME,
                "merge.blocked",
                &[("branch", branch), ("reason", "out-of-scope")],
                None,
            )
            .await?;
            return Err(format!(
                "merge blocked: {branch} changed files outside mission {} scope ({}): {} — the tower must widen the mission scope (TowerMission scope patch) or revert those changes",
                mission.id,
                mission.scope.join(", "),
                out_of_scope.join(", ")
            ));
        }

        let checked_out = current_branch(&self.repo_root).await.map_err(|_| {
            format!(
                "merge blocked: the main checkout is in a detached HEAD state — check out the recorded base branch \"{}\" before merging; nothing was merged",
                state.base
            )
        })?;
        if checked_out != state.base {
            return Err(format!(
                "merge blocked: the main checkout is on \"{checked_out}\", not the recorded base \"{}\" — switch it back (`git checkout {}`) and retry; nothing was merged",
                state.base, state.base
            ));
        }
        let merge_commit = merge_no_ff(&self.repo_root, &tip).await?;
        let head_tip = branch_tip(&self.repo_root, "HEAD").await?;
        if !is_ancestor(&self.repo_root, &tip, &head_tip).await? {
            return Err(format!(
                "recovery-required: merge of {branch} returned but HEAD {head_tip} does not contain the evaluated commit {tip} — mission {} was not marked merged; inspect the base checkout before retrying",
                mission.id
            ));
        }
        let current_branch_tip = branch_tip(&self.repo_root, branch).await?;
        if current_branch_tip != tip {
            self.append_log(
                TOWER_NAME,
                "merge.advanced",
                &[
                    ("branch", branch),
                    ("evaluated", &tip),
                    ("current", &current_branch_tip),
                ],
                None,
            )
            .await?;
            return Ok(TowerMergeResult {
                status: TowerMergeStatus::Advanced,
                merge_commit,
                evaluated_branch_tip: tip,
                current_branch_tip,
                conflicts_with: Vec::new(),
                noop: false,
            });
        }
        mission.status = TowerMissionStatus::Merged;
        let m_copy = mission.clone();

        let changed_set: std::collections::HashSet<String> = changed.into_iter().collect();
        let other_missions: Vec<_> = state
            .missions
            .iter()
            .filter(|other| other.branch != branch && other.status.is_open())
            .collect();
        let base = &state.base;
        let changed_set_ref = &changed_set;
        let conflicts = map_with_concurrency(
            other_missions,
            TOWER_GIT_CONCURRENCY,
            |_, other| async move {
                if !branch_exists(&self.repo_root, &other.branch).await {
                    return None;
                }
                let other_changed = diff_name_only(&self.repo_root, base, &other.branch)
                    .await
                    .unwrap_or_default();
                let overlap: Vec<String> = other_changed
                    .into_iter()
                    .filter(|file| changed_set_ref.contains(file))
                    .collect();
                (!overlap.is_empty()).then(|| (other.branch.clone(), overlap))
            },
        )
        .await;
        let conflicts_with: Vec<_> = conflicts.into_iter().flatten().collect();

        self.save(&state).await?;
        self.render_missions_index(&state).await?;
        self.render_mission_file(&m_copy).await?;
        let commit_short = if merge_commit.len() >= 7 {
            &merge_commit[..7]
        } else {
            &merge_commit
        };
        self.append_log(
            TOWER_NAME,
            "merge",
            &[
                ("branch", branch),
                ("base", &state.base),
                ("merge_commit", commit_short),
            ],
            None,
        )
        .await?;

        Ok(TowerMergeResult {
            status: TowerMergeStatus::Merged,
            merge_commit,
            evaluated_branch_tip: tip,
            current_branch_tip,
            conflicts_with,
            noop: false,
        })
    }

    pub async fn add_worktree(
        &self,
        worktree: &str,
        branch: &str,
        base: &str,
    ) -> Result<String, String> {
        let rel = format!("{WORKTREES_DIR}/{worktree}");
        let abs_path = self.abs(&rel);
        worktree_add(&self.repo_root, &abs_path, branch, base).await?;
        self.append_log(
            TOWER_NAME,
            "worktree.add",
            &[("worktree", worktree), ("branch", branch), ("base", base)],
            None,
        )
        .await?;
        Ok(rel)
    }

    pub async fn teardown(&self, options: TowerTeardownOptions) -> Result<Vec<String>, String> {
        let state = self.load().await?;
        let mut excluded = HashMap::new();
        for raw in &options.exclude {
            let trimmed = raw.trim().trim_end_matches('/');
            if trimmed.is_empty() {
                continue;
            }
            let prefix = format!("{WORKTREES_DIR}/");
            let short = trimmed.strip_prefix(&prefix).unwrap_or(trimmed);
            excluded.insert(short.to_string(), raw.clone());
        }
        let mut report = Vec::new();
        for mission in &state.missions {
            let rel = format!("{WORKTREES_DIR}/{}", mission.worktree);
            let abs_path = self.abs(&rel);
            let was_excluded = excluded.remove(&mission.worktree).is_some();
            if !is_registered_worktree(&self.repo_root, &abs_path).await {
                report.push(format!("already removed {rel}"));
                if !options.dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.remove.skipped",
                        &[
                            ("worktree", &mission.worktree),
                            ("reason", "already-removed"),
                        ],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            let verb = if options.dry_run {
                "would keep"
            } else {
                "kept"
            };
            if was_excluded {
                report.push(format!("{verb} {rel} (excluded)"));
                if !options.dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.keep",
                        &[("worktree", &mission.worktree), ("reason", "excluded")],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            if let Some(live) = state.roster.agents.iter().find(|agent| {
                agent.worktree.as_deref() == Some(mission.worktree.as_str())
                    && options.live_agent_ids.contains(&agent.agent_id)
            }) {
                report.push(format!("{verb} {rel} (live agent: {})", live.name));
                if !options.dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.keep",
                        &[
                            ("worktree", &mission.worktree),
                            ("reason", "live-agent"),
                            ("agent", &live.name),
                        ],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            if is_worktree_dirty(&abs_path).await && !options.force {
                report.push(format!(
                    "{verb} {rel} (uncommitted changes — rerun with force to remove)"
                ));
                if !options.dry_run {
                    self.append_log(
                        TOWER_NAME,
                        "worktree.keep",
                        &[
                            ("worktree", &mission.worktree),
                            ("reason", "uncommitted-changes"),
                        ],
                        None,
                    )
                    .await?;
                }
                continue;
            }
            if options.dry_run {
                report.push(format!("would remove {rel}"));
                continue;
            }
            match worktree_remove(&self.repo_root, &abs_path).await {
                Ok(_) => {
                    report.push(format!("removed {rel}"));
                    self.append_log(
                        TOWER_NAME,
                        "worktree.remove",
                        &[("worktree", &mission.worktree)],
                        None,
                    )
                    .await?;
                }
                Err(err) => {
                    report.push(format!("failed to remove {rel}: {err}"));
                    self.append_log(
                        TOWER_NAME,
                        "worktree.remove.failed",
                        &[("worktree", &mission.worktree), ("reason", &err)],
                        None,
                    )
                    .await?;
                }
            }
        }
        for original in excluded.values() {
            report.push(format!(
                "excluded worktree \"{original}\" matched no mission worktree"
            ));
        }
        if !options.dry_run {
            self.append_log(
                TOWER_NAME,
                "teardown",
                &[("force", if options.force { "yes" } else { "" })],
                None,
            )
            .await?;
        }
        Ok(report)
    }

    pub async fn render_missions_index(&self, state: &TowerState) -> Result<(), String> {
        let mut rows = Vec::new();
        for m in &state.missions {
            let owner = m.owner.as_deref().unwrap_or("—");
            rows.push(format!(
                "| {} | {} | {} | {} | {} | {} |",
                m.id,
                m.title,
                m.branch,
                m.worktree,
                m.status.emoji(),
                owner
            ));
        }

        let mut deps = Vec::new();
        for m in &state.missions {
            for dep in &m.deps {
                deps.push(format!("{dep} → {}", m.id));
            }
        }
        let deps_str = if deps.is_empty() {
            "(none)".into()
        } else {
            deps.join("\n")
        };

        let mut scopes = Vec::new();
        for m in &state.missions {
            let note = if m.kind == crate::tools::tower::types::TowerMissionKind::Survey {
                " (survey — informational, reserves nothing)"
            } else {
                ""
            };
            scopes.push(format!("- {}{note}: {}", m.id, m.scope.join(", ")));
        }
        let scopes_str = if scopes.is_empty() {
            "(none)".into()
        } else {
            scopes.join("\n")
        };

        let content = vec![
            "# MISSIONS".into(),
            String::new(),
            "<!-- Generated by tower tools from state.json — do not edit by hand. -->".into(),
            String::new(),
            "| ID | Mission | Branch | Worktree | Status | Owner |".into(),
            "| -- | ------- | ------ | -------- | ------ | ----- |".into(),
            rows.join("\n"),
            String::new(),
            "Status: 🟡 planned · 🔵 active · 🟢 completed · 🔴 blocked · ⏸️ paused · ✅ merged · 🚫 abandoned".into(),
            format!("Mode: {} — Base: {}", state.mode, state.base),
            String::new(),
            "## Dependency Flow".into(),
            deps_str,
            String::new(),
            "## Scope Map".into(),
            scopes_str,
            String::new(),
        ].join("\n");

        fs::write(self.abs(MISSIONS_INDEX), content.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub async fn render_mission_file(&self, mission: &TowerMission) -> Result<(), String> {
        let rel = format!(
            "{MISSIONS_DIR}/{}",
            mission_file_name(&mission.id, &mission.slug)
        );
        let survey_tag = if mission.kind == crate::tools::tower::types::TowerMissionKind::Survey {
            " 🔍 (read-only survey)"
        } else {
            ""
        };
        let owner = mission.owner.as_deref().unwrap_or("—");

        let tasks_str = if mission.tasks.is_empty() {
            "- [ ] (no tasks recorded)".into()
        } else {
            mission
                .tasks
                .iter()
                .map(|t| {
                    let marker = if t.done {
                        "x"
                    } else if t.dropped {
                        "-"
                    } else {
                        " "
                    };
                    let suffix = if t.dropped { " (dropped)" } else { "" };
                    format!("- [{marker}] {}{suffix}", t.text)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let deps_str = if mission.deps.is_empty() {
            "(none)".into()
        } else {
            mission.deps.join(", ")
        };

        let blockers_str = if mission.blockers.is_empty() {
            "- (none)".into()
        } else {
            mission
                .blockers
                .iter()
                .map(|b| format!("- {b}"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let notes_str = if mission.notes.is_empty() {
            "- (none)".into()
        } else {
            mission
                .notes
                .iter()
                .map(|n| format!("- {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let content = vec![
            format!("# Mission {}: {}{survey_tag}", mission.id, mission.title),
            String::new(),
            "<!-- Generated by tower tools from state.json — update via the TowerMission tool. -->"
                .into(),
            String::new(),
            "| Branch | Worktree | Status | Scope | Owner |".into(),
            "| ------ | -------- | ------ | ----- | ----- |".into(),
            format!(
                "| {} | {} | {} | {} | {} |",
                mission.branch,
                mission.worktree,
                mission.status.emoji(),
                mission.scope.join(", "),
                owner
            ),
            String::new(),
            "## Tasks".into(),
            tasks_str,
            String::new(),
            "## Dependencies".into(),
            deps_str,
            String::new(),
            "## Blockers".into(),
            blockers_str,
            String::new(),
            "## Notes".into(),
            notes_str,
            String::new(),
        ]
        .join("\n");

        fs::write(self.abs(&rel), content.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn write_unique_with<F, Fut>(
        &self,
        rel: &str,
        content: &str,
        link: F,
    ) -> Result<String, String>
    where
        F: Fn(PathBuf, PathBuf) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<()>>,
    {
        let (stem, ext) = match rel.rfind('.') {
            Some(pos) => (&rel[..pos], &rel[pos..]),
            None => (rel, ""),
        };

        for attempt in 0..100 {
            let candidate = if attempt == 0 {
                rel.to_string()
            } else {
                format!("{stem}-{}{ext}", attempt + 1)
            };
            let temporary = format!(
                "{candidate}.tmp-{}-{:016x}",
                std::process::id(),
                fastrand::u64(..)
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
                link(temporary_path.clone(), candidate_path.clone()).await
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
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
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

    async fn write_unique(&self, rel: &str, content: &str) -> Result<String, String> {
        self.write_unique_with(rel, content, |from, to| async move {
            fs::hard_link(from, to).await
        })
        .await
    }
}

async fn read_git_dir(cwd: &Path) -> Option<String> {
    let raw = fs::read_to_string(cwd.join(".git")).await.ok()?;
    let line = raw.lines().find(|l| l.starts_with("gitdir:"))?;
    Some(line["gitdir:".len()..].trim().to_string())
}

#[cfg(test)]
mod tests;
