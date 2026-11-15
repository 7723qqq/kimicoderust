use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use crate::turn_loop::types::LLMMessage;

use super::protocol::concurrency::{map_with_concurrency, TOWER_GIT_CONCURRENCY};
use super::protocol::git::{
    branch_exists, checkout_new_local_branch, commit_paths, list_base_dirty_entries,
};
use super::protocol::paths::{resolve_tower_repo_root, BROADCAST_NAME, TOWER_NAME};
use super::protocol::store::{
    assert_local_base_branch, TowerActiveAgentIds, TowerMissionRuntimeOptions, TowerStore,
};
use super::tower::{TowerEnterFailure, TowerEnterResult, TowerExitReason};
use super::towerOps::TowerInboxSent;

pub const TOWER_MODE_TOOLS: &[&str] = &[
    "TowerInit",
    "TowerPlan",
    "TowerSpawn",
    "TowerMerge",
    "TowerRebase",
    "TowerTeardown",
    "TowerSend",
    "TowerInbox",
    "TowerFinding",
    "TowerReview",
    "TowerMission",
    "TowerStatus",
];

#[allow(dead_code)]
const TOWER_FLAG_VETO_TOOLS: &[&str] = &[
    "TowerInit",
    "TowerPlan",
    "TowerSpawn",
    "TowerMerge",
    "TowerRebase",
    "TowerTeardown",
    "TowerSend",
    "TowerInbox",
    "TowerFinding",
    "TowerReview",
    "TowerMission",
    "TowerStatus",
    "TowerComplete",
];

pub const TOWER_INBOX_WAKE_VARIANT: &str = "tower_inbox";

pub fn active_tower_agent_ids(
    task_runner: Option<&crate::storage::TaskRunner>,
) -> Option<HashSet<String>> {
    task_runner.map(|runner| {
        runner
            .list()
            .into_iter()
            .filter(|task| {
                task.get("kind").and_then(serde_json::Value::as_str) == Some("agent")
                    && task.get("status").and_then(serde_json::Value::as_str) == Some("running")
            })
            .filter_map(|task| {
                task.get("agentId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    })
}

const WAKE_SUBJECT_PREVIEW_MAX: usize = 120;
const WAKE_BATCH_LIMIT: usize = 6;
const WAKE_DIGEST_LIMIT: usize = 4;
const WAKE_TRACKED_KEY_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxWakeItem {
    pub key: String,
    pub from: String,
    pub subject: String,
    pub message_id: Option<String>,
    pub file: Option<String>,
    pub sent_at: Option<String>,
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxWakeBatch {
    pub items: Vec<InboxWakeItem>,
    pub omitted: usize,
}

#[derive(Debug, Clone)]
pub struct TowerWakeNotification {
    pub message: LLMMessage,
    pub batch: InboxWakeBatch,
}

#[allow(async_fn_in_trait)]
pub trait TowerServiceHost {
    fn agent_id(&self) -> &str;
    fn session_id(&self) -> &str;
    fn cwd(&self) -> &Path;
    fn flag_enabled(&self) -> bool;
    fn feature_assembled(&self) -> bool;
    fn mode_state_active(&self) -> bool;
    fn owner_state(&self) -> Option<String>;
    fn add_active_tool(&self, name: &str);
    fn dispatch_tower_mode_enter(&self);
    fn dispatch_tower_mode_exit(&self);
    fn project_tower_mode(&self, active: bool);
    fn track_tower_mode_enter(&self, entered: bool, reason: Option<&str>);
    fn track_tower_mode_exit(&self, reason: TowerExitReason);
    fn owner_activity(&self, owner: &str) -> Option<bool>;
    async fn owner_title(&self, owner: &str) -> Option<String>;
    async fn exit_owner_session(&self, owner: &str);
    fn log_warn(&self, message: &str);
    fn active_agent_ids(&self) -> Option<HashSet<String>>;
    fn wake_loop_available(&self) -> bool;
}

struct BranchLeaseGuard {
    leases: Arc<StdMutex<HashSet<String>>>,
    branch: String,
}

impl Drop for BranchLeaseGuard {
    fn drop(&mut self) {
        let mut held = self
            .leases
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        held.remove(&self.branch);
    }
}

#[derive(Debug, Default)]
struct TowerInboxWakeState {
    items: Vec<InboxWakeItem>,
    omitted: usize,
    legacy_sequence: u64,
    generation: u64,
    recent_keys: HashSet<String>,
    recent_order: VecDeque<String>,
    scheduled: bool,
    pending: bool,
    last_notified: Option<InboxWakeBatch>,
    discarding: bool,
    disposed: bool,
    wake_turn_id: Option<u64>,
    wake_turn_batch: Option<InboxWakeBatch>,
    wake_aborted_for_user_prompt: bool,
}

impl TowerInboxWakeState {
    pub fn dispose(&mut self) {
        self.disposed = true;
    }

    pub fn on_inbox_sent(
        &mut self,
        event: &TowerInboxSent,
        is_main_agent: bool,
        is_active: bool,
    ) -> bool {
        if !is_main_agent {
            return false;
        }
        if !is_active {
            return false;
        }
        if event.from == TOWER_NAME {
            return false;
        }
        if event.to != TOWER_NAME && event.to != BROADCAST_NAME {
            return false;
        }
        let key = if let Some(message_id) = event.message_id.as_deref() {
            format!("id:{message_id}")
        } else if let Some(file) = event.file.as_deref() {
            format!("file:{file}")
        } else {
            self.legacy_sequence += 1;
            format!("legacy:{}", self.legacy_sequence)
        };
        self.enqueue(
            InboxWakeItem {
                key,
                from: event.from.clone(),
                subject: event.subject.clone(),
                message_id: event.message_id.clone(),
                file: event.file.clone(),
                sent_at: event.sent_at.clone(),
                mission_id: event.mission_id.clone(),
            },
            false,
        );
        self.schedule()
    }

    pub fn enqueue(&mut self, item: InboxWakeItem, replay: bool) {
        if self.items.iter().any(|candidate| candidate.key == item.key) {
            return;
        }
        if !replay && self.recent_keys.contains(&item.key) {
            return;
        }
        if self.recent_keys.insert(item.key.clone()) {
            self.recent_order.push_back(item.key.clone());
        }
        while self.recent_order.len() > WAKE_TRACKED_KEY_LIMIT {
            if let Some(oldest) = self.recent_order.pop_front() {
                self.recent_keys.remove(&oldest);
            }
        }
        self.items.push(item);
        if self.items.len() > WAKE_BATCH_LIMIT {
            self.items.remove(0);
            self.omitted += 1;
        }
    }

    pub fn requeue(&mut self, batch: &InboxWakeBatch) {
        for item in &batch.items {
            self.enqueue(item.clone(), true);
        }
        self.omitted += batch.omitted;
    }

    pub fn drop_all(&mut self) {
        self.generation += 1;
        self.discarding = true;
        self.items.clear();
        self.omitted = 0;
        self.recent_keys.clear();
        self.recent_order.clear();
        self.scheduled = false;
        self.pending = false;
        self.last_notified = None;
        self.wake_turn_batch = None;
        self.wake_turn_id = None;
        self.wake_aborted_for_user_prompt = false;
        self.discarding = false;
    }

    pub fn schedule(&mut self) -> bool {
        if self.scheduled || self.pending || self.items.is_empty() {
            return false;
        }
        self.scheduled = true;
        true
    }

    pub fn begin_flush(
        &mut self,
        active: bool,
        loop_available: bool,
    ) -> Option<(InboxWakeBatch, u64)> {
        self.scheduled = false;
        if self.disposed || !active || !loop_available {
            self.items.clear();
            self.omitted = 0;
            return None;
        }
        if self.items.is_empty() {
            return None;
        }
        let batch = InboxWakeBatch {
            items: std::mem::take(&mut self.items),
            omitted: self.omitted,
        };
        self.omitted = 0;
        self.last_notified = Some(batch.clone());
        self.pending = true;
        Some((batch, self.generation))
    }

    pub fn flush_is_current(
        &mut self,
        generation: u64,
        active: bool,
        loop_available: bool,
    ) -> bool {
        if generation != self.generation || self.disposed || !active || !loop_available {
            if generation == self.generation {
                self.pending = false;
            }
            return false;
        }
        true
    }

    pub fn consume(&mut self) -> bool {
        self.pending = false;
        self.schedule()
    }

    pub fn handle_dropped(&mut self, batch: &InboxWakeBatch, active: bool) -> bool {
        self.pending = false;
        if self.discarding || self.disposed || !active {
            return false;
        }
        self.requeue(batch);
        self.schedule()
    }

    pub fn on_wake_turn_started(&mut self, turn_id: u64) {
        self.wake_turn_id = Some(turn_id);
        self.wake_turn_batch = self.last_notified.clone();
    }

    pub fn on_wake_turn_ended(&mut self, turn_id: u64, loop_queue_len: usize) -> bool {
        if self.wake_turn_id == Some(turn_id) {
            self.wake_turn_id = None;
            if !self.wake_aborted_for_user_prompt {
                self.wake_turn_batch = None;
            }
            return false;
        }
        if !self.wake_aborted_for_user_prompt {
            return false;
        }
        if loop_queue_len > 0 {
            return false;
        }
        self.wake_aborted_for_user_prompt = false;
        let batch = self.wake_turn_batch.take();
        if let Some(batch) = batch {
            self.requeue(&batch);
        }
        self.schedule()
    }

    pub fn on_prompt_submitted(&self, turn_id: u64) -> bool {
        self.wake_turn_id == Some(turn_id)
    }

    pub fn mark_prompt_aborted(&mut self) {
        self.wake_aborted_for_user_prompt = true;
    }
}

#[derive(Clone, Default)]
pub struct TowerService {
    branch_leases: Arc<StdMutex<HashSet<String>>>,
    inbox_wake: Arc<StdMutex<TowerInboxWakeState>>,
    last_published: Arc<StdMutex<Option<bool>>>,
}

impl TowerService {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn store(&self, cwd: &Path) -> TowerStore {
        TowerStore::new(PathBuf::from(resolve_tower_repo_root(
            &cwd.to_string_lossy(),
        )))
    }

    pub fn is_branch_leased(&self, branch: &str) -> bool {
        self.branch_leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(branch)
    }

    pub async fn with_branch_lease<T, F, Fut>(
        &self,
        branch: &str,
        execute: F,
    ) -> Result<T, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, String>>,
    {
        {
            let mut held = self
                .branch_leases
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !held.insert(branch.to_string()) {
                return Err(format!(
                    "branch \"{branch}\" is leased by another tower merge/rebase — not waiting; retry after that operation finishes"
                ));
            }
        }
        let _guard = BranchLeaseGuard {
            leases: self.branch_leases.clone(),
            branch: branch.to_string(),
        };
        execute().await
    }

    pub async fn enter<H: TowerServiceHost>(
        &self,
        host: &H,
        base: Option<&str>,
    ) -> Result<TowerEnterResult, String> {
        let result = self.resolve_enter(host, base).await?;
        match &result {
            TowerEnterResult::Entered => host.track_tower_mode_enter(true, None),
            TowerEnterResult::Failed(failure) => {
                host.track_tower_mode_enter(false, Some(failure.reason()))
            }
        }
        Ok(result)
    }

    async fn resolve_enter<H: TowerServiceHost>(
        &self,
        host: &H,
        base: Option<&str>,
    ) -> Result<TowerEnterResult, String> {
        if host.agent_id() != "main" {
            return Ok(TowerEnterResult::Failed(TowerEnterFailure::NotMainAgent));
        }
        if !host.flag_enabled() {
            return Ok(TowerEnterResult::Failed(TowerEnterFailure::ExperimentOff));
        }
        if !host.feature_assembled() {
            return Ok(TowerEnterResult::Failed(
                TowerEnterFailure::FeatureNotAssembled,
            ));
        }
        let store = self.store(host.cwd());
        if let Some(owner) = self
            .resolve_tower_owner(&store, host.owner_state())
            .await
            .filter(|owner| owner != host.session_id())
        {
            match host.owner_activity(&owner) {
                Some(true) => {
                    let owner_title = host.owner_title(&owner).await;
                    return Ok(TowerEnterResult::Failed(
                        TowerEnterFailure::OwnedByLiveSession { owner, owner_title },
                    ));
                }
                Some(false) => host.exit_owner_session(&owner).await,
                None => {}
            }
        }
        if let Some(base) = base {
            self.prepare_user_base(host, base).await?;
        }
        if host.mode_state_active() {
            return Ok(TowerEnterResult::Entered);
        }
        self.adopt_tower_roster(&store, host.session_id()).await?;
        for name in TOWER_MODE_TOOLS {
            host.add_active_tool(name);
        }
        self.set_last_published(Some(true));
        host.dispatch_tower_mode_enter();
        Ok(TowerEnterResult::Entered)
    }

    async fn prepare_user_base<H: TowerServiceHost>(
        &self,
        host: &H,
        base: &str,
    ) -> Result<(), String> {
        let repo_root = resolve_tower_repo_root(&host.cwd().to_string_lossy());
        let store = TowerStore::new(PathBuf::from(repo_root.as_str()));
        store.ensure_repository(Some(base)).await?;
        if store.is_initialized().await? {
            let state = store.load().await?;
            if state.base == base {
                assert_local_base_branch(Path::new(&repo_root), base).await?;
                return Ok(());
            }
            if !branch_exists(Path::new(&repo_root), base).await? {
                self.create_base_branch(Path::new(&repo_root), base).await?;
            }
            store.rebase(base).await?;
            return Ok(());
        }
        if branch_exists(Path::new(&repo_root), base).await? {
            store
                .init(Some(host.session_id().to_string()), Some(base.to_string()))
                .await?;
            return Ok(());
        }
        self.create_base_branch(Path::new(&repo_root), base).await?;
        store
            .init(Some(host.session_id().to_string()), Some(base.to_string()))
            .await?;
        Ok(())
    }

    async fn create_base_branch(&self, repo_root: &Path, base: &str) -> Result<(), String> {
        let dirty = list_base_dirty_entries(repo_root).await?;
        if dirty.iter().any(|entry| entry.unmerged) {
            return Err(
                "the checkout has unmerged paths (an in-progress merge, rebase, or cherry-pick) — finish or abort it before starting a tower on a new base"
                    .into(),
            );
        }
        checkout_new_local_branch(repo_root, base).await?;
        if dirty.is_empty() {
            return Ok(());
        }
        let paths: Vec<String> = dirty.into_iter().map(|entry| entry.path).collect();
        commit_paths(
            repo_root,
            &paths,
            &format!("tower: snapshot of uncommitted base checkout changes (base {base})"),
        )
        .await
        .map_err(|error| {
            format!(
                "created and switched to \"{base}\", but committing the checkout's uncommitted changes onto it failed: {error}. The changes are still uncommitted on \"{base}\" — commit or move them, then re-run /tower {base}."
            )
        })
    }

    pub async fn exit<H: TowerServiceHost>(&self, host: &H, reason: TowerExitReason) {
        if !host.mode_state_active() {
            return;
        }
        self.set_last_published(Some(false));
        self.drop_inbox_wake();
        host.dispatch_tower_mode_exit();
        host.track_tower_mode_exit(reason);
        let store = self.store(host.cwd());
        if let Err(error) = store.release(host.session_id()).await {
            host.log_warn(&format!(
                "failed to release tower workspace ownership: {error}"
            ));
        }
    }

    async fn adopt_tower_roster(&self, store: &TowerStore, session_id: &str) -> Result<(), String> {
        store.adopt(session_id).await.map(|_| ()).map_err(|error| {
            format!("failed to adopt the tower workspace roster: {error}")
        })
    }

    pub fn is_active<H: TowerServiceHost>(&self, host: &H) -> bool {
        host.agent_id() == "main"
            && host.flag_enabled()
            && host.feature_assembled()
            && host.mode_state_active()
    }

    pub async fn reconcile_foreign_tower<H: TowerServiceHost>(&self, host: &H) {
        if host.agent_id() != "main" {
            return;
        }
        if !host.mode_state_active() {
            return;
        }
        let store = self.store(host.cwd());
        let Some(owner) = self.resolve_tower_owner(&store, host.owner_state()).await else {
            return;
        };
        if owner == host.session_id() {
            return;
        }
        if host.owner_activity(&owner).is_none() {
            if let Err(error) = self.adopt_tower_roster(&store, host.session_id()).await {
                host.log_warn(&format!(
                    "failed to adopt tower workspace roster on restore: {error}"
                ));
                self.exit(host, TowerExitReason::ForeignReconcile).await;
            }
            return;
        }
        self.exit(host, TowerExitReason::ForeignReconcile).await;
    }

    async fn resolve_tower_owner(
        &self,
        store: &TowerStore,
        fallback: Option<String>,
    ) -> Option<String> {
        let store_owner = store.load().await.ok().and_then(|state| state.session_id);
        store_owner.or(fallback)
    }

    pub async fn record_tower_agent_death<H: TowerServiceHost>(
        &self,
        host: &H,
        agent_id: &str,
        status: &str,
        stop_reason: Option<&str>,
    ) {
        if status == "completed" {
            return;
        }
        if !self.is_active(host) {
            return;
        }
        let store = self.store(host.cwd());
        if let Some(owner) = resolve_foreign_store_owner(&store, host.session_id()).await {
            tracing::info!(
                event = "TaskTerminatedNotice",
                agent_id,
                session_id = host.session_id(),
                owner = owner.as_str(),
                pid = std::process::id(),
                "tower: skipping roster agent death mark — tower store is owned by another session"
            );
            return;
        }
        tracing::info!(
            event = "TaskTerminatedNotice",
            agent_id,
            status,
            stop_reason = stop_reason.unwrap_or(""),
            session_id = host.session_id(),
            pid = std::process::id(),
            "tower: marking roster agent died"
        );
        let _ = store
            .mark_agent_died(agent_id, status, stop_reason, Some(host.session_id()))
            .await;
    }

    pub async fn clear_tower_agent_death<H: TowerServiceHost>(
        &self,
        host: &H,
        agent_id: &str,
    ) {
        if !self.is_active(host) {
            return;
        }
        let store = self.store(host.cwd());
        if let Some(owner) = resolve_foreign_store_owner(&store, host.session_id()).await {
            tracing::info!(
                event = "SubagentStarted",
                agent_id,
                session_id = host.session_id(),
                owner = owner.as_str(),
                pid = std::process::id(),
                "tower: skipping roster agent death clear — tower store is owned by another session"
            );
            return;
        }
        tracing::info!(
            event = "SubagentStarted",
            agent_id,
            session_id = host.session_id(),
            pid = std::process::id(),
            "tower: clearing roster agent death mark"
        );
        let _ = store
            .clear_agent_died(agent_id, Some(host.session_id()))
            .await;
    }

    pub fn restore_tower_tools<H: TowerServiceHost>(&self, host: &H) {
        if !host.flag_enabled() {
            return;
        }
        if !self.is_active(host) {
            return;
        }
        if host.agent_id() != "main" {
            return;
        }
        for name in TOWER_MODE_TOOLS {
            host.add_active_tool(name);
        }
        self.set_last_published(Some(true));
        host.project_tower_mode(true);
    }

    pub fn reconcile_tower_projection<H: TowerServiceHost>(&self, host: &H) {
        if host.agent_id() != "main" {
            return;
        }
        if !host.mode_state_active() {
            self.set_last_published(Some(false));
            return;
        }
        let effective = self.is_active(host);
        if !effective {
            self.drop_inbox_wake();
        }
        if self.last_published() == Some(effective) {
            return;
        }
        self.set_last_published(Some(effective));
        host.project_tower_mode(effective);
    }

    fn last_published(&self) -> Option<bool> {
        *self
            .last_published
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn set_last_published(&self, value: Option<bool>) {
        *self
            .last_published
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = value;
    }

    pub fn dispose_inbox_wake(&self) {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dispose();
    }

    pub fn drop_inbox_wake(&self) {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drop_all();
    }

    pub fn on_tower_inbox_sent<H: TowerServiceHost>(
        &self,
        host: &H,
        event: &TowerInboxSent,
    ) -> bool {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .on_inbox_sent(event, host.agent_id() == "main", self.is_active(host))
    }

    pub async fn flush_inbox_wake<H: TowerServiceHost>(
        &self,
        host: &H,
    ) -> Option<TowerWakeNotification> {
        let (batch, generation) = self
            .inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .begin_flush(self.is_active(host), host.wake_loop_available())?;
        let store = self.store(host.cwd());
        let active_agent_ids = host.active_agent_ids();
        let text = render_inbox_wake(&store, &batch, active_agent_ids.as_ref()).await;
        let current = self
            .inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .flush_is_current(
                generation,
                self.is_active(host),
                host.wake_loop_available(),
            );
        if !current {
            return None;
        }
        Some(TowerWakeNotification {
            message: LLMMessage {
                role: kimi_core::types::Role::User,
                content: text,
                ..LLMMessage::default()
            },
            batch,
        })
    }

    pub fn consume_inbox_wake(&self) -> bool {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .consume()
    }

    pub fn inbox_wake_handle_dropped<H: TowerServiceHost>(
        &self,
        host: &H,
        batch: &InboxWakeBatch,
    ) -> bool {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .handle_dropped(batch, self.is_active(host))
    }

    pub fn on_wake_turn_started(&self, turn_id: u64) {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .on_wake_turn_started(turn_id);
    }

    pub fn on_wake_turn_ended(&self, turn_id: u64, loop_queue_len: usize) -> bool {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .on_wake_turn_ended(turn_id, loop_queue_len)
    }

    pub fn on_prompt_submitted(&self, turn_id: u64) -> bool {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .on_prompt_submitted(turn_id)
    }

    pub fn mark_prompt_aborted(&self) {
        self.inbox_wake
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .mark_prompt_aborted();
    }
}

async fn resolve_foreign_store_owner(store: &TowerStore, session_id: &str) -> Option<String> {
    store
        .load()
        .await
        .ok()
        .and_then(|state| state.session_id)
        .filter(|owner| owner != session_id)
}

async fn render_inbox_wake(
    store: &TowerStore,
    batch: &InboxWakeBatch,
    active_agent_ids: Option<&HashSet<String>>,
) -> String {
    let count = batch.items.len() + batch.omitted;
    let count_text = if count == 1 {
        "1 new tower inbox message".to_string()
    } else {
        format!("{count} new tower inbox messages")
    };
    let mut lines = vec![format!(
        "{count_text} — {} reference(s), omitted={}. Read and route with TowerInbox; this wake does not acknowledge them.",
        batch.items.len(),
        batch.omitted
    )];
    for item in &batch.items {
        let subject = if item.subject.chars().count() > WAKE_SUBJECT_PREVIEW_MAX {
            format!(
                "{}…",
                item.subject
                    .chars()
                    .take(WAKE_SUBJECT_PREVIEW_MAX)
                    .collect::<String>()
            )
        } else {
            item.subject.clone()
        };
        let mut references = Vec::new();
        if let Some(message_id) = item.message_id.as_deref() {
            references.push(format!("messageId={message_id}"));
        }
        if let Some(file) = item.file.as_deref() {
            references.push(format!("file={file}"));
        }
        if let Some(sent_at) = item.sent_at.as_deref() {
            references.push(format!("sentAt={sent_at}"));
        }
        if let Some(mission_id) = item.mission_id.as_deref() {
            references.push(format!("mission={mission_id}"));
        }
        lines.push(format!(
            "- {}: \"{}\"{}",
            item.from,
            subject,
            if references.is_empty() {
                String::new()
            } else {
                format!(" ({})", references.join(", "))
            }
        ));
    }
    lines.extend(render_inbox_wake_digests(store, &batch.items, active_agent_ids).await);
    lines.join("\n")
}

async fn render_inbox_wake_digests(
    store: &TowerStore,
    items: &[InboxWakeItem],
    active_agent_ids: Option<&HashSet<String>>,
) -> Vec<String> {
    let mut mission_ids: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        if let Some(mission_id) = item.mission_id.as_ref()
            && seen.insert(mission_id.clone())
        {
            mission_ids.push(mission_id.clone());
        }
    }
    let state = store.load().await.ok();
    let mut lines = Vec::new();
    if let Some(recovered_at) = state.as_ref().and_then(|state| state.recovered_at.as_deref()) {
        lines.push(format!(
            "⚠️ Tower state was recovered after a loss at {recovered_at} — missions, roster, and protocol history before that point are gone. Report the loss to the user instead of continuing silently."
        ));
    }
    if mission_ids.is_empty() {
        return lines;
    }
    lines.push("Gate digest:".to_string());
    let selected: Vec<String> = mission_ids
        .iter()
        .take(WAKE_DIGEST_LIMIT)
        .cloned()
        .collect();
    match state {
        None => {
            for mission_id in &selected {
                lines.push(format!("- {mission_id}: gate=unavailable"));
            }
        }
        Some(state) => {
            let runtime = TowerMissionRuntimeOptions {
                active_agent_ids: active_agent_ids
                    .map(|ids| TowerActiveAgentIds::Set(ids.clone())),
            };
            let digests = map_with_concurrency(
                selected,
                TOWER_GIT_CONCURRENCY,
                |_, mission_id| {
                    let state = &state;
                    let runtime = &runtime;
                    async move {
                        let Some(mission) = state
                            .missions
                            .iter()
                            .find(|candidate| candidate.id == mission_id)
                        else {
                            return format!("- {mission_id}: gate=mission-missing");
                        };
                        if !mission.status.is_open() {
                            return format!("- {mission_id}: gate={}", mission.status.as_str());
                        }
                        match store.mission_gate(state, mission, runtime).await {
                            Ok(gate) if gate.ready => {
                                let binding = gate
                                    .review_binding
                                    .map(|binding| binding.as_str())
                                    .unwrap_or("");
                                if binding.is_empty() {
                                    format!("- {mission_id}: gate=READY reasons=none")
                                } else {
                                    format!(
                                        "- {mission_id}: gate=READY reasons=none binding={binding}"
                                    )
                                }
                            }
                            Ok(gate) => format!(
                                "- {mission_id}: gate=BLOCKED reasons={}",
                                gate.blocks
                                    .iter()
                                    .map(|block| block.reason)
                                    .collect::<Vec<_>>()
                                    .join(",")
                            ),
                            Err(_) => format!("- {mission_id}: gate=unavailable"),
                        }
                    }
                },
            )
            .await;
            lines.extend(digests);
        }
    }
    if mission_ids.len() > WAKE_DIGEST_LIMIT {
        lines.push(format!(
            "- digest-omitted={}",
            mission_ids.len() - WAKE_DIGEST_LIMIT
        ));
    }
    lines
}
