use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use indexmap::IndexSet;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::_base::di::lifecycle::DisposableRef;
use crate::app::event::event2::Event2;
use crate::agent::profile::context::split_agents_md_files;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::agent::state::agent_state_v2::IAgentStateService;
use crate::features::reminder::reminder_service::ReminderService;
use crate::features::reminder::types::ReminderNotification;
use crate::session::agent_lifecycle::agent_lifecycle::MAIN_AGENT_ID;
use crate::session::sqlite_store::workspace_root_key;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

use super::change_notifier::{
    AGENTS_MD_CHANGE_VARIANT, BeliefsSeed, IAgentChangeNotifierService, SKILLS_CHANGE_VARIANT,
    SUBAGENTS_CHANGE_VARIANT, SubagentChangesInput, SubagentModelPoolSnapshot, SubagentProfile,
    WORKSPACE_DIRS_CHANGE_VARIANT,
};
use super::change_notifier_ops::{
    ChangeNotifierSnapshotEvent, ChangeNotifierSnapshotEventPayload, ChangeNotifierSnapshotState,
    change_notifier_snapshot_key,
};

#[derive(Clone, Default)]
pub struct AgentsMdChangeSnapshot {
    pub profile_bound: bool,
    pub system_prompt: String,
    pub previous_paths: Vec<String>,
    pub paths: Vec<String>,
    pub content: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Clone, Default)]
pub struct SkillChangeSnapshot {
    pub profile_bound: bool,
    pub system_prompt: String,
    pub listing: String,
}

#[derive(Clone, Default)]
pub struct SubagentChangeSnapshot {
    pub profile_bound: bool,
    pub profiles: Vec<SubagentProfile>,
    pub model_pool: Option<SubagentModelPoolSnapshot>,
    pub agent_tool_active: bool,
    pub explicit_allowlist: Option<Vec<String>>,
}

#[derive(Clone, Default)]
pub struct WorkspaceChangeSnapshot {
    pub primary_by_workspace: BTreeMap<String, String>,
    pub additional_dirs: Vec<String>,
    pub work_dir: String,
    pub effective_additional_dirs: Vec<String>,
    pub workspace_id: String,
    pub follows_primary: bool,
    pub explicit_cwd: bool,
}

pub type ChangeSnapshotProvider<T> = Arc<
    dyn Fn() -> BoxFuture<'static, Result<T, WireAsyncError>> + Send + Sync,
>;

#[derive(Clone)]
pub struct AgentChangeNotifierProviders {
    pub agents_md: ChangeSnapshotProvider<AgentsMdChangeSnapshot>,
    pub skills: ChangeSnapshotProvider<SkillChangeSnapshot>,
    pub subagents: ChangeSnapshotProvider<SubagentChangeSnapshot>,
    pub workspace: ChangeSnapshotProvider<WorkspaceChangeSnapshot>,
}

struct AgentChangeNotifierServiceInner {
    scope_context: Arc<dyn IAgentScopeContext>,
    states: Arc<dyn IAgentStateService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    reminder: Arc<dyn ReminderService>,
    providers: AgentChangeNotifierProviders,
    muted: AtomicBool,
    workspace_tail: Mutex<Shared<BoxFuture<'static, ()>>>,
    _registration: DisposableRef,
}

#[derive(Clone)]
pub struct AgentChangeNotifierService {
    inner: Arc<AgentChangeNotifierServiceInner>,
}

impl AgentChangeNotifierService {
    pub fn new(
        scope_context: Arc<dyn IAgentScopeContext>,
        states: Arc<dyn IAgentStateService>,
        dispatcher: Arc<dyn IEventDispatcher>,
        reminder: Arc<dyn ReminderService>,
        providers: AgentChangeNotifierProviders,
    ) -> Self {
        let registration = states
            .contribute_state(change_notifier_snapshot_key().state_key())
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            inner: Arc::new(AgentChangeNotifierServiceInner {
                scope_context,
                states,
                dispatcher,
                reminder,
                providers,
                muted: AtomicBool::new(false),
                workspace_tail: Mutex::new(async {}.boxed().shared()),
                _registration: registration,
            }),
        }
    }

    fn snapshot(&self) -> ChangeNotifierSnapshotState {
        serde_json::from_value(
            self.inner
                .states
                .get(&change_notifier_snapshot_key().state_key()),
        )
        .unwrap_or_else(|error| panic!("{error}"))
    }

    fn enabled(&self) -> bool {
        self.inner.scope_context.agent_id() == MAIN_AGENT_ID
            && !self.inner.muted.load(Ordering::Acquire)
    }

    async fn record_snapshot(&self, next: ChangeNotifierSnapshotState) -> bool {
        let event = ChangeNotifierSnapshotEvent::new(ChangeNotifierSnapshotEventPayload {
            agent_id: self.inner.scope_context.agent_id(),
            agents_md_hash: next.agents_md_hash,
            agents_md_files: next.agents_md_files,
            skills_hash: next.skills_hash,
            subagent_names: next.subagent_names,
            model_pool: next.model_pool,
            workspace_primary_by_workspace: next.workspace_primary_by_workspace,
            workspace_additional_dirs: next.workspace_additional_dirs,
        });
        let dispatcher = self.inner.dispatcher.clone();
        let event: Event2 = event.into();
        dispatcher.dispatch(event).await.is_ok()
    }

    fn notify(&self, content: String, variant: &str) {
        self.inner.reminder.notify(
            &content,
            &ReminderNotification {
                variant: variant.to_string(),
                owner_prompt_id: None,
            },
        );
    }

    async fn notify_agents_md(&self) {
        if !self.enabled() {
            return;
        }
        let Ok(fresh) = (self.inner.providers.agents_md)().await else {
            return;
        };
        if !fresh.profile_bound {
            return;
        }
        let hash = hash_text(&format!("{}\n\n{}", fresh.paths.join("\n"), fresh.content));
        let snapshot = self.snapshot();
        if snapshot.agents_md_hash.as_deref() == Some(hash.as_str()) || !self.enabled() {
            return;
        }
        let current_files = fresh
            .files
            .iter()
            .map(|(path, content)| (path.clone(), hash_text(content)))
            .collect::<BTreeMap<_, _>>();
        let mut next = snapshot.clone();
        next.agents_md_hash = Some(hash);
        next.agents_md_files = Some(current_files.clone());
        if !self.record_snapshot(next).await {
            return;
        }
        let changes = match &snapshot.agents_md_files {
            Some(beliefs) => classify_against_beliefs(beliefs, &fresh.paths, &current_files),
            None => classify_agents_md_changes(
                &fresh.system_prompt,
                &fresh.previous_paths,
                &fresh.paths,
                &fresh.files,
            ),
        };
        if changes.is_empty() {
            return;
        }
        self.notify(agents_md_change_text(&changes), AGENTS_MD_CHANGE_VARIANT);
    }

    async fn notify_skills(&self) {
        if !self.enabled() {
            return;
        }
        let Ok(current) = (self.inner.providers.skills)().await else {
            return;
        };
        if !current.profile_bound {
            return;
        }
        let hash = hash_text(&current.listing);
        let snapshot = self.snapshot();
        if snapshot.skills_hash.as_deref() == Some(hash.as_str()) || !self.enabled() {
            return;
        }
        let mut next = snapshot.clone();
        next.skills_hash = Some(hash);
        if !self.record_snapshot(next).await {
            return;
        }
        if snapshot.skills_hash.is_none() {
            if current.listing.is_empty() {
                if !current.system_prompt.contains("DISREGARD any earlier skill listings") {
                    return;
                }
            } else if current
                .system_prompt
                .contains(&format!("{}\n\n", current.listing))
                || current.system_prompt.ends_with(&current.listing)
            {
                return;
            }
        }
        self.notify(skills_change_text(&current.listing), SKILLS_CHANGE_VARIANT);
    }

    async fn notify_subagents(&self, input: Option<SubagentChangesInput>) {
        if !self.enabled() {
            return;
        }
        let Ok(current) = (self.inner.providers.subagents)().await else {
            return;
        };
        if !current.profile_bound {
            return;
        }
        let current_names = current
            .profiles
            .iter()
            .map(|profile| profile.name.clone())
            .collect::<Vec<_>>();
        let snapshot = self.snapshot();
        if snapshot.subagent_names.as_ref().is_some_and(|previous| {
            same_string_set(&current_names, previous)
                && same_model_pool(current.model_pool.as_ref(), snapshot.model_pool.as_ref())
        }) || !self.enabled()
        {
            return;
        }
        let base_names = snapshot
            .subagent_names
            .clone()
            .or_else(|| input.as_ref().and_then(|input| input.previous_subagent_names.clone()))
            .unwrap_or_else(|| current_names.clone());
        let base_pool = if snapshot.subagent_names.is_some() {
            snapshot.model_pool.clone()
        } else {
            input
                .as_ref()
                .and_then(|input| input.previous_model_pool.clone())
                .unwrap_or_else(|| current.model_pool.clone())
        };
        let added = current
            .profiles
            .iter()
            .filter(|profile| !base_names.contains(&profile.name))
            .filter(|profile| {
                current.explicit_allowlist.as_ref().is_none_or(|allowlist| {
                    allowlist.contains(&profile.name)
                })
            })
            .collect::<Vec<_>>();
        let removed = base_names
            .iter()
            .filter(|name| !current_names.contains(name))
            .filter(|name| {
                current
                    .explicit_allowlist
                    .as_ref()
                    .is_none_or(|allowlist| allowlist.contains(*name))
            })
            .cloned()
            .collect::<Vec<_>>();
        let current_aliases = pool_aliases(current.model_pool.as_ref());
        let base_aliases = pool_aliases(base_pool.as_ref());
        let models_added = difference(&current_aliases, &base_aliases);
        let models_removed = difference(&base_aliases, &current_aliases);
        let models_updated = current_aliases
            .iter()
            .filter(|alias| {
                base_pool
                    .as_ref()
                    .and_then(|pool| pool.models.get(*alias))
                    .is_some_and(|before| {
                        current
                            .model_pool
                            .as_ref()
                            .and_then(|pool| pool.models.get(*alias))
                            .is_some_and(|after| before != after)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        let default_model_change = match (&current.model_pool, &base_pool) {
            (Some(current), Some(base)) if current.default_model != base.default_model => {
                Some((base.default_model.clone(), current.default_model.clone()))
            }
            _ => None,
        };
        let mut next = snapshot.clone();
        next.subagent_names = Some(current_names);
        next.model_pool = current.model_pool.clone();
        if !self.record_snapshot(next).await {
            return;
        }
        if added.is_empty()
            && removed.is_empty()
            && models_added.is_empty()
            && models_removed.is_empty()
            && models_updated.is_empty()
            && default_model_change.is_none()
            || !current.agent_tool_active
        {
            return;
        }
        self.notify(
            subagents_change_text(
                &added,
                &removed,
                &models_added,
                &models_removed,
                &models_updated,
                default_model_change,
            ),
            SUBAGENTS_CHANGE_VARIANT,
        );
    }

    async fn notify_workspace(&self) {
        if !self.enabled() {
            return;
        }
        let Ok(current) = (self.inner.providers.workspace)().await else {
            return;
        };
        if !self.enabled() {
            return;
        }
        let snapshot = self.snapshot();
        if same_primary_snapshot(
            snapshot.workspace_primary_by_workspace.as_ref(),
            &current.primary_by_workspace,
        ) && same_directory_snapshot(
            snapshot.workspace_additional_dirs.as_ref(),
            &current.additional_dirs,
        ) {
            return;
        }
        let mut next = snapshot.clone();
        next.workspace_primary_by_workspace = Some(current.primary_by_workspace.clone());
        next.workspace_additional_dirs = Some(current.additional_dirs.clone());
        if !self.record_snapshot(next).await {
            return;
        }
        if !self.enabled() {
            self.record_snapshot(snapshot.clone()).await;
            return;
        }
        if snapshot.workspace_primary_by_workspace.is_none()
            && snapshot.workspace_additional_dirs.is_none()
        {
            return;
        }
        let changes = diff_workspace_dirs(
            snapshot.workspace_primary_by_workspace.as_ref(),
            snapshot.workspace_additional_dirs.as_deref(),
            &current,
        );
        if changes.primary.is_empty() && changes.added.is_empty() && changes.removed.is_empty() {
            return;
        }
        self.notify(
            workspace_dirs_change_text(&changes, &current),
            WORKSPACE_DIRS_CHANGE_VARIANT,
        );
    }
}

impl IAgentChangeNotifierService for AgentChangeNotifierService {
    fn notify_agents_md_changes(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone();
        async move {
            this.notify_agents_md().await;
            Ok(())
        }
        .boxed()
    }

    fn notify_skill_changes(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone();
        async move {
            this.notify_skills().await;
            Ok(())
        }
        .boxed()
    }

    fn notify_subagent_changes(
        &self,
        input: Option<SubagentChangesInput>,
    ) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone();
        async move {
            this.notify_subagents(input).await;
            Ok(())
        }
        .boxed()
    }

    fn notify_workspace_dirs_changes(&self) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let previous = self.inner.workspace_tail.lock().clone();
        let this = self.clone();
        let run = async move {
            previous.await;
            this.notify_workspace().await;
        }
        .boxed()
        .shared();
        *self.inner.workspace_tail.lock() = run.clone();
        async move {
            run.await;
            Ok(())
        }
        .boxed()
    }

    fn seed_beliefs(&self, seed: BeliefsSeed) -> BoxFuture<'static, Result<(), WireAsyncError>> {
        let this = self.clone();
        async move {
            if this.inner.scope_context.agent_id() != MAIN_AGENT_ID {
                return Ok(());
            }
            let mut next = this.snapshot();
            next.agents_md_hash = None;
            next.agents_md_files = Some(
                split_agents_md_files(&seed.agents_md, &seed.agents_md_paths)
                    .into_iter()
                    .map(|(path, content)| (path, hash_text(&content)))
                    .collect(),
            );
            next.skills_hash = Some(hash_text(&seed.skills_listing));
            let Ok(current) = (this.inner.providers.subagents)().await else {
                return Ok(());
            };
            next.subagent_names = Some(
                current
                    .profiles
                    .into_iter()
                    .map(|profile| profile.name)
                    .collect(),
            );
            next.model_pool = current.model_pool;
            this.record_snapshot(next).await;
            Ok(())
        }
        .boxed()
    }

    fn mute(&self) {
        self.inner.muted.store(true, Ordering::Release);
    }

    fn unmute(&self) {
        self.inner.muted.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct AgentsMdChanges {
    modified: Vec<String>,
    deleted: Vec<String>,
    added: Vec<String>,
}

impl AgentsMdChanges {
    fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.deleted.is_empty() && self.added.is_empty()
    }
}

fn hash_text(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn same_string_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().all(|item| right.contains(item))
}

fn same_model_pool(
    left: Option<&SubagentModelPoolSnapshot>,
    right: Option<&SubagentModelPoolSnapshot>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.default_model == right.default_model && left.models == right.models
        }
        _ => false,
    }
}

fn pool_aliases(pool: Option<&SubagentModelPoolSnapshot>) -> Vec<String> {
    pool.map(|pool| pool.models.keys().cloned().collect())
        .unwrap_or_default()
}

fn difference(left: &[String], right: &[String]) -> Vec<String> {
    left.iter()
        .filter(|item| !right.contains(item))
        .cloned()
        .collect()
}

fn normalize(path: &str) -> String {
    let mut normalized = crate::tool::path_access::normalize_path(
        path,
        crate::tool::path_access::PathClass::Posix,
    );
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_uppercase().to_string();
        normalized.replace_range(0..1, &drive);
    }
    normalized
}

fn classify_against_beliefs(
    beliefs: &BTreeMap<String, String>,
    current_paths: &[String],
    current_files: &BTreeMap<String, String>,
) -> AgentsMdChanges {
    let current = current_paths
        .iter()
        .map(|path| normalize(path))
        .collect::<IndexSet<_>>();
    let belief_paths = beliefs.keys().cloned().collect::<IndexSet<_>>();
    let deleted = belief_paths.difference(&current).cloned().collect();
    let added = current.difference(&belief_paths).cloned().collect();
    let modified = current
        .intersection(&belief_paths)
        .filter(|path| {
            current_files
                .get(*path)
                .is_some_and(|current| beliefs.get(*path) != Some(current))
        })
        .cloned()
        .collect();
    AgentsMdChanges {
        modified,
        deleted,
        added,
    }
}

fn classify_agents_md_changes(
    system_prompt: &str,
    previous_paths: &[String],
    current_paths: &[String],
    contents: &BTreeMap<String, String>,
) -> AgentsMdChanges {
    let previous = previous_paths
        .iter()
        .map(|path| normalize(path))
        .collect::<IndexSet<_>>();
    let current = current_paths
        .iter()
        .map(|path| normalize(path))
        .collect::<IndexSet<_>>();
    let deleted = previous.difference(&current).cloned().collect();
    let added = current.difference(&previous).cloned().collect();
    let modified = current
        .intersection(&previous)
        .filter(|path| {
            let Some(content) = contents.get(*path) else {
                return false;
            };
            let injected = format!("<!-- From: {} -->\n{content}", path);
            !system_prompt.contains(&format!("{injected}\n\n"))
                && !system_prompt.ends_with(&injected)
        })
        .cloned()
        .collect();
    AgentsMdChanges {
        modified,
        deleted,
        added,
    }
}

fn agents_md_change_text(changes: &AgentsMdChanges) -> String {
    let mut sections = Vec::new();
    if !changes.modified.is_empty() || !changes.deleted.is_empty() {
        let deleted = changes.deleted.iter().collect::<IndexSet<_>>();
        let paths = changes
            .modified
            .iter()
            .chain(&changes.deleted)
            .map(|path| {
                if deleted.contains(path) {
                    format!("- {path} (deleted)")
                } else {
                    format!("- {path}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "The AGENTS.md instruction file(s) below changed on disk after they were injected into the system prompt:\n{paths}\nRead the current file(s) and follow the latest contents; the copies injected in the system prompt are stale."
        ));
    }
    if !changes.added.is_empty() {
        sections.push(format!(
            "The following AGENTS.md file(s) exist in your workspace but were not included in your system prompt:\n{}\nRead them before making changes in those directories.",
            changes
                .added
                .iter()
                .map(|path| format!("- {path}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    sections.join("\n\n")
}

fn skills_change_text(listing: &str) -> String {
    if listing.is_empty() {
        "The skill catalog changed since your system prompt was rendered; no skills are currently available.".to_string()
    } else {
        format!(
            "The skill catalog changed since your system prompt was rendered; the listing below replaces it.\n\n{listing}"
        )
    }
}

fn subagents_change_text(
    added: &[&SubagentProfile],
    removed: &[String],
    models_added: &[String],
    models_removed: &[String],
    models_updated: &[String],
    default_model_change: Option<(Option<String>, Option<String>)>,
) -> String {
    let mut lines = vec!["The available subagent profiles and models changed:".to_string()];
    if !added.is_empty() {
        let labels = added
            .iter()
            .map(|profile| {
                let details = [
                    Some(profile.description.as_str()).filter(|value| !value.is_empty()),
                    profile.when_to_use.as_deref().filter(|value| !value.is_empty()),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
                if details.is_empty() {
                    profile.name.clone()
                } else {
                    format!("{} ({})", profile.name, details.join("; "))
                }
            })
            .collect::<Vec<_>>();
        lines.push(format!("- Profiles added: {}", labels.join(", ")));
    }
    if !removed.is_empty() {
        lines.push(format!("- Profiles removed: {}", removed.join(", ")));
    }
    if !models_added.is_empty() {
        lines.push(format!("- Models added: {}", models_added.join(", ")));
    }
    if !models_removed.is_empty() {
        lines.push(format!("- Models removed: {}", models_removed.join(", ")));
    }
    if !models_updated.is_empty() {
        lines.push(format!(
            "- Model descriptions updated: {}",
            models_updated.join(", ")
        ));
    }
    if let Some((from, to)) = default_model_change {
        match (from, to) {
            (Some(from), Some(to)) => {
                lines.push(format!("- Default model changed: {from} → {to}"))
            }
            (None, Some(to)) => lines.push(format!("- Default model set: {to}")),
            (Some(from), None) => {
                lines.push(format!("- Default model cleared (was {from})"))
            }
            (None, None) => {}
        }
    }
    lines.push(
        "You can use them right away — the Agent tool validates subagent_type against the live catalog."
            .to_string(),
    );
    lines.join("\n")
}

#[derive(Default)]
struct WorkspaceDirsChanges {
    primary: Vec<WorkspacePrimaryChange>,
    added: Vec<String>,
    removed: Vec<String>,
}

struct WorkspacePrimaryChange {
    workspace_id: String,
    from: String,
    to: String,
}

fn same_primary_snapshot(
    previous: Option<&BTreeMap<String, String>>,
    current: &BTreeMap<String, String>,
) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    previous.len() == current.len()
        && previous.iter().all(|(id, before)| {
            current
                .get(id)
                .is_some_and(|after| workspace_root_key(before) == workspace_root_key(after))
        })
}

fn directory_keys(directories: &[String]) -> IndexSet<String> {
    directories.iter().map(|dir| workspace_root_key(dir)).collect()
}

fn same_directory_snapshot(previous: Option<&Vec<String>>, current: &[String]) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    directory_keys(previous) == directory_keys(current)
}

fn directory_difference(
    left: &[String],
    right: &[String],
    excluded: &IndexSet<String>,
) -> Vec<String> {
    let right = directory_keys(right);
    let mut seen = IndexSet::new();
    left.iter()
        .filter(|dir| {
            let key = workspace_root_key(dir);
            !right.contains(&key) && !excluded.contains(&key) && seen.insert(key)
        })
        .cloned()
        .collect()
}

fn diff_workspace_dirs(
    previous_primary: Option<&BTreeMap<String, String>>,
    previous_dirs: Option<&[String]>,
    current: &WorkspaceChangeSnapshot,
) -> WorkspaceDirsChanges {
    let empty_primary = BTreeMap::new();
    let previous_primary = previous_primary.unwrap_or(&empty_primary);
    let previous_dirs = previous_dirs.unwrap_or(&[]);
    let mut primary = Vec::new();
    let mut workspace_ids = current.primary_by_workspace.keys().collect::<Vec<_>>();
    workspace_ids.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    for workspace_id in workspace_ids {
        let to = &current.primary_by_workspace[workspace_id];
        let Some(from) = previous_primary.get(workspace_id) else {
            continue;
        };
        if workspace_root_key(from) != workspace_root_key(to) {
            primary.push(WorkspacePrimaryChange {
                workspace_id: workspace_id.clone(),
                from: from.clone(),
                to: to.clone(),
            });
        }
    }
    let excluded = primary
        .iter()
        .flat_map(|change| [workspace_root_key(&change.from), workspace_root_key(&change.to)])
        .collect::<IndexSet<_>>();
    WorkspaceDirsChanges {
        added: directory_difference(&current.additional_dirs, previous_dirs, &excluded),
        removed: directory_difference(previous_dirs, &current.additional_dirs, &excluded),
        primary,
    }
}

fn workspace_dirs_change_text(
    changes: &WorkspaceDirsChanges,
    current: &WorkspaceChangeSnapshot,
) -> String {
    let mut lines = vec!["Project directory configuration changed.".to_string()];
    for change in &changes.primary {
        lines.push(format!(
            "Primary directory for project {}: {} -> {}.",
            change.workspace_id, change.from, change.to
        ));
    }
    if !changes.added.is_empty() {
        lines.push(format!("Added directories: {}.", changes.added.join(", ")));
    }
    if !changes.removed.is_empty() {
        lines.push(format!("Removed directories: {}.", changes.removed.join(", ")));
    }
    let primary_change = changes
        .primary
        .iter()
        .find(|change| change.workspace_id == current.workspace_id);
    if current.follows_primary
        && let Some(change) = primary_change
    {
        lines.push(format!(
            "Default working directory: {} -> {}.",
            change.from, change.to
        ));
    } else if current.explicit_cwd {
        lines.push(format!("Explicit session cwd remains {}.", current.work_dir));
    } else {
        lines.push(format!("Default working directory: {}.", current.work_dir));
    }
    lines.push(format!(
        "Effective directories: {}.",
        std::iter::once(current.work_dir.clone())
            .chain(current.effective_additional_dirs.iter().cloned())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    lines.push(
        "The updated directory set is already effective for subsequent tool calls and supersedes earlier directory information."
            .to_string(),
    );
    lines.join("\n")
}
