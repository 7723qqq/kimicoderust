use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use regex::Regex;
use serde_json::json;

use crate::_base::log::log::ILogService;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::flag::flag::IFlagService;
use crate::app::memory::memory_repo::IMemoryRepo;
use crate::app::memory::memory_store::IMemoryStore;
use crate::features::reminder::reminderService::ReminderService;
use crate::features::reminder::types::{ContextInjectionContent, ContextInjectionResult};
use crate::human::llm::message::ContentPart;
use crate::i18n::LocalizedText;
use crate::os::interface::host_process::IHostProcessService;
use crate::session::sessionContext::session_context::ISessionContext;
use crate::state::event_dispatcher::IEventDispatcher;

use super::flag::GIT_MEMORY_FLAG_ID;
use super::gitMemoryEntries::GitMemoryEntries;
use super::gitMemoryIndexLoader::{GitMemoryIndexLoader, GitMemoryIndexLoaderDeps};
use super::gitMemoryRenderState::GitMemoryRenderState;
use super::gitReader::{GitUnavailableReason, read_repo_map};
use super::projection::{
    project_file_block, project_memory_block, project_repo_block, project_repo_tree,
    project_signal_block,
};
use super::types::{
    CommitRecord, GIT_MEMORY_DIR_COMMITS, GIT_MEMORY_FILE_COMMITS, GIT_MEMORY_REPO_COMMITS,
    GIT_MEMORY_SIGNAL_COMMITS, GIT_MEMORY_SIGNAL_PATHS, GIT_MEMORY_TREE_DEPTH, GitMemoryIndex,
};

pub const GIT_MEMORY_REPO_MAP_MAX_LINES: usize = 2000;

#[async_trait]
pub trait IGitMemoryService: Send + Sync {
    async fn repo_block(&self) -> String;
    async fn signal_block(&self, message: String) -> String;
    async fn file_block(&self, path: String) -> String;
    async fn repo_map(&self, path: Option<String>, depth: usize) -> String;
}

pub struct GitMemoryService {
    process: Arc<dyn IHostProcessService>,
    bootstrap: Arc<dyn IBootstrapService>,
    log: Arc<dyn ILogService>,
    flags: Arc<dyn IFlagService>,
    context_memory: Arc<dyn IAgentContextMemoryService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    scope_context: Arc<dyn IAgentScopeContext>,
    index_loader: Arc<GitMemoryIndexLoader>,
    render_state: Arc<parking_lot::Mutex<GitMemoryRenderState>>,
    entries: Arc<GitMemoryEntries>,
    unavailable_notified: AtomicBool,
}

impl GitMemoryService {
    pub fn new(
        process: Arc<dyn IHostProcessService>,
        bootstrap: Arc<dyn IBootstrapService>,
        log: Arc<dyn ILogService>,
        flags: Arc<dyn IFlagService>,
        reminders: Arc<dyn ReminderService>,
        context_memory: Arc<dyn IAgentContextMemoryService>,
        memory_store: Arc<dyn IMemoryStore>,
        memory_repo: Arc<dyn IMemoryRepo>,
        dispatcher: Arc<dyn IEventDispatcher>,
        scope_context: Arc<dyn IAgentScopeContext>,
        session: Arc<dyn ISessionContext>,
    ) -> Arc<Self> {
        let this = Arc::new_cyclic(|weak| {
            let render_state = Arc::new(parking_lot::Mutex::new(GitMemoryRenderState::default()));
            let index_loader = GitMemoryIndexLoader::new(GitMemoryIndexLoaderDeps {
                process: process.clone(),
                store_dir: bootstrap.store_dir().into(),
                cwd: bootstrap.cwd(),
                log: log.clone(),
                now: Arc::new(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64
                }),
                on_index_changed: {
                    let render_state = render_state.clone();
                    Arc::new(move || render_state.lock().clear())
                },
                on_head_unavailable: {
                    let weak = weak.clone();
                    Arc::new(move |reason| {
                        if let Some(this) = weak.upgrade() {
                            this.notify_unavailable(reason);
                        }
                    })
                },
            });
            let entries = Arc::new(GitMemoryEntries::new(
                memory_store,
                memory_repo,
                session,
                log.clone(),
                render_state.clone(),
            ));
            Self {
                process,
                bootstrap,
                log,
                flags,
                context_memory,
                dispatcher,
                scope_context,
                index_loader,
                render_state,
                entries,
                unavailable_notified: AtomicBool::new(false),
            }
        });
        reminders.register("git_memory_signal", {
            let weak = Arc::downgrade(&this);
            Arc::new(move |context| {
                let weak = weak.clone();
                Box::pin(async move {
                    if !context.is_new_turn {
                        return Ok(None);
                    }
                    let Some(this) = weak.upgrade() else {
                        return Ok(None);
                    };
                    let message = this.last_user_text();
                    let block = this.signal_block(message).await;
                    Ok(Some(ContextInjectionResult {
                        content: ContextInjectionContent::Text(block),
                        disclosure: None,
                    }))
                })
            })
        });
        this
    }

    pub fn create_index_loader(self: &Arc<Self>) -> Arc<GitMemoryIndexLoader> {
        GitMemoryIndexLoader::new(self.index_loader_deps())
    }

    pub fn index_loader_deps(self: &Arc<Self>) -> GitMemoryIndexLoaderDeps {
        GitMemoryIndexLoaderDeps {
            process: self.process.clone(),
            store_dir: self.bootstrap.store_dir().into(),
            cwd: self.bootstrap.cwd(),
            log: self.log.clone(),
            now: Arc::new(|| self.now()),
            on_index_changed: {
                let render_state = self.render_state.clone();
                Arc::new(move || render_state.lock().clear())
            },
            on_head_unavailable: {
                let weak = self.downgrade();
                Arc::new(move |reason| {
                    if let Some(this) = weak.upgrade() {
                        this.notify_unavailable(reason);
                    }
                })
            },
        }
    }

    pub async fn repo_block(&self) -> String {
        <Self as IGitMemoryService>::repo_block(self).await
    }

    pub async fn file_block(&self, path: &str) -> String {
        <Self as IGitMemoryService>::file_block(self, path.to_string()).await
    }

    pub async fn signal_block(&self, message: &str) -> String {
        <Self as IGitMemoryService>::signal_block(self, message.to_string()).await
    }

    pub async fn repo_map(&self, path: Option<&str>, depth: usize) -> String {
        <Self as IGitMemoryService>::repo_map(self, path.map(ToString::to_string), depth).await
    }

    fn enabled(&self) -> bool {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.flags.enabled(GIT_MEMORY_FLAG_ID)
        })) {
            Ok(enabled) => enabled,
            Err(error) => {
                self.log.warn(
                    "git memory flag lookup failed",
                    Some(json!({ "error": panic_message(error) })),
                );
                false
            }
        }
    }

    fn commits_for_path(&self, index: &GitMemoryIndex, path: &str) -> Vec<CommitRecord> {
        index
            .by_path
            .get(path)
            .into_iter()
            .flatten()
            .filter_map(|position| index.commits.get(*position).cloned())
            .take(GIT_MEMORY_FILE_COMMITS)
            .collect()
    }

    fn commits_for_directory(&self, index: &GitMemoryIndex, directory: &str) -> Vec<CommitRecord> {
        if let Some(cached) = self.render_state.lock().directory_commits(directory) {
            return cached.to_vec();
        }
        let prefix = format!("{directory}/");
        let commits = index
            .commits
            .iter()
            .filter(|commit| commit.files.iter().any(|file| file.starts_with(&prefix)))
            .take(GIT_MEMORY_DIR_COMMITS)
            .cloned()
            .collect::<Vec<_>>();
        self.render_state
            .lock()
            .set_directory_commits(directory.to_string(), commits.clone());
        commits
    }

    fn notify_unavailable(&self, reason: GitUnavailableReason) {
        if self.unavailable_notified.load(Ordering::Relaxed) || !self.enabled() {
            return;
        }
        if self.unavailable_notified.swap(true, Ordering::Relaxed) {
            return;
        }
        let cwd = self.bootstrap.cwd();
        let key = match reason {
            GitUnavailableReason::NotARepo => "gitMemory.unavailable.notARepo",
            GitUnavailableReason::SpawnFailed => "gitMemory.unavailable.spawnFailed",
            GitUnavailableReason::Permission => "gitMemory.unavailable.permission",
            GitUnavailableReason::Timeout => "gitMemory.unavailable.timeout",
        };
        let message = LocalizedText::with_params(
            key,
            HashMap::from([("cwd".to_string(), cwd)]),
        )
        .render();
        let event = crate::agent::profile::profile_ops::WarningIssued::new(
            crate::agent::profile::profile_ops::WarningIssuedPayload {
                agent_id: self.scope_context.agent_id(),
                code: Some("git-memory-unavailable".to_string()),
                message,
            },
        );
        let dispatcher = self.dispatcher.clone();
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
    }

    pub fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn to_repo_path(&self, path: &str) -> String {
        let normalized = path.replace('\\', "/");
        let repo_root = self.index_loader.repo_root();
        if !repo_root.is_empty() {
            let root = repo_root.replace('\\', "/");
            let root = root.strip_suffix('/').unwrap_or(&root);
            let prefix = format!("{root}/");
            if let Some(relative) = normalized.strip_prefix(&prefix) {
                return relative.to_string();
            }
        }
        let relative = normalized
            .strip_prefix("./")
            .unwrap_or(&normalized)
            .to_string();
        let path_prefix = self.index_loader.path_prefix();
        if path_prefix.is_empty() || relative.starts_with(&path_prefix) {
            return relative;
        }
        format!("{path_prefix}{relative}")
    }

    fn extract_paths(&self, message: &str) -> Vec<String> {
        let pattern = Regex::new(
            r"[A-Za-z0-9_./\\-]+\.(?:ts|tsx|js|jsx|mjs|cjs|rs|go|py|java|kt|rb|vue|json|jsonc|md|yaml|yml|toml)",
        )
        .unwrap();
        let mut unique = Vec::new();
        for item in pattern.find_iter(message) {
            let key = self.to_repo_path(item.as_str());
            if !unique.contains(&key) {
                unique.push(key);
            }
            if unique.len() >= GIT_MEMORY_SIGNAL_PATHS {
                break;
            }
        }
        unique
    }

    fn last_user_text(&self) -> String {
        let history = self.context_memory.get();
        for message in history.iter().rev() {
            if message.role != "user" {
                continue;
            }
            for part in &message.content {
                if let ContentPart::Text { text, .. } = part
                    && !text.is_empty()
                {
                    return text.clone();
                }
            }
        }
        String::new()
    }

    fn repo_path_candidates(&self, path: Option<&str>) -> Vec<Option<String>> {
        if path.is_none_or(|path| path.is_empty()) {
            return vec![None];
        }
        let path = path.unwrap();
        let normalized = path.replace('\\', "/");
        let normalized = normalized
            .strip_prefix("./")
            .unwrap_or(&normalized)
            .to_string();
        let mut candidates = vec![Some(normalized.clone())];
        let prefix = self.index_loader.path_prefix();
        if prefix.is_empty() {
            return candidates;
        }
        let prefixed = if normalized.starts_with(&prefix) {
            normalized.clone()
        } else {
            format!("{prefix}{normalized}")
        };
        if prefixed != normalized {
            candidates.push(Some(prefixed));
        }
        candidates
    }
}

#[async_trait]
impl IGitMemoryService for GitMemoryService {
    async fn repo_block(&self) -> String {
        if !self.enabled() {
            return String::new();
        }
        self.render_state.lock().trim();
        let Some(index) = self.index_loader.ensure_index().await else {
            return String::new();
        };
        let commits = index
            .commits
            .iter()
            .take(GIT_MEMORY_REPO_COMMITS)
            .cloned()
            .collect::<Vec<_>>();
        let unscoped = self.entries.unscoped().await;
        let mut parts = Vec::new();
        let repo = project_repo_block(&commits);
        if !repo.is_empty() {
            parts.push(repo);
        }
        let tree = project_repo_tree(&index.dirs, GIT_MEMORY_TREE_DEPTH);
        if !tree.is_empty() {
            parts.push(tree);
        }
        if !unscoped.is_empty() {
            parts.push(
                unscoped
                    .iter()
                    .map(project_memory_block)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        parts.join("\n")
    }

    async fn signal_block(&self, message: String) -> String {
        if !self.enabled() {
            return String::new();
        }
        self.render_state.lock().trim();
        let paths = self.extract_paths(&message);
        if paths.is_empty() {
            return String::new();
        }
        let Some(index) = self.index_loader.ensure_index().await else {
            return String::new();
        };
        let mut blocks = Vec::new();
        for path in paths {
            let commits = self
                .commits_for_path(&index, &path)
                .into_iter()
                .take(GIT_MEMORY_SIGNAL_COMMITS)
                .collect::<Vec<_>>();
            if commits.is_empty() {
                continue;
            }
            let seen = self.render_state.lock().rendered_commits().clone();
            let block = project_signal_block(&path, &commits, &seen);
            if block.is_empty() {
                continue;
            }
            blocks.push(block);
            let mut state = self.render_state.lock();
            for commit in commits {
                state.mark_commit(commit.sha);
            }
        }
        blocks.join("\n")
    }

    async fn file_block(&self, path: String) -> String {
        if !self.enabled() {
            return String::new();
        }
        self.render_state.lock().trim();
        let Some(index) = self.index_loader.ensure_index().await else {
            return String::new();
        };
        let key = self.to_repo_path(&path);
        if self.render_state.lock().has_path(&key) {
            return String::new();
        }
        let commits = self.commits_for_path(&index, &key);
        let memories = self.entries.for_path(&key).await;
        let directory = directory_of(&key);
        let dir_commits = if directory.is_empty() {
            Vec::new()
        } else {
            self.commits_for_directory(&index, &directory)
        };
        if commits.is_empty() && dir_commits.is_empty() && memories.is_empty() {
            return String::new();
        }
        self.render_state.lock().mark_path(key.clone());
        let seen = self.render_state.lock().rendered_commits().clone();
        let mut block = project_file_block(
            &key,
            &commits,
            &directory,
            &dir_commits,
            &seen,
        );
        {
            let mut state = self.render_state.lock();
            for commit in commits.iter().chain(&dir_commits) {
                state.mark_commit(commit.sha.clone());
            }
        }
        if !memories.is_empty() {
            let rendered = memories
                .iter()
                .map(project_memory_block)
                .collect::<Vec<_>>()
                .join("\n");
            block = if block.is_empty() {
                rendered
            } else {
                format!("{block}\n{rendered}")
            };
        }
        block
    }

    async fn repo_map(&self, path: Option<String>, depth: usize) -> String {
        if !self.enabled() {
            return String::new();
        }
        let Some(head) = self.index_loader.ensure_head().await else {
            return String::new();
        };
        for candidate in self.repo_path_candidates(path.as_deref()) {
            let Some(entries) = read_repo_map(
                &self.process,
                &head.repo_root,
                candidate.as_deref(),
                None,
            )
            .await
            else {
                return String::new();
            };
            let lines = entries
                .into_iter()
                .filter(|entry| is_within_depth(entry, candidate.as_deref(), depth))
                .collect::<Vec<_>>();
            if lines.is_empty() {
                continue;
            }
            let total = lines.len();
            let mut shown = lines
                .into_iter()
                .take(GIT_MEMORY_REPO_MAP_MAX_LINES)
                .collect::<Vec<_>>();
            if shown.len() < total {
                shown.push(format!(
                    "... and {} more paths; narrow the path or raise the depth",
                    total - shown.len()
                ));
            }
            return shown.join("\n");
        }
        String::new()
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

fn directory_of(path: &str) -> String {
    let Some(separator) = path.rfind('/') else {
        return String::new();
    };
    if separator == 0 {
        return String::new();
    }
    path[..separator].to_string()
}

fn is_within_depth(entry: &str, base: Option<&str>, depth: usize) -> bool {
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return entry.split('/').count() <= depth;
    };
    if entry == base {
        return false;
    }
    let prefix = format!("{base}/");
    entry
        .strip_prefix(&prefix)
        .is_some_and(|relative| relative.split('/').count() <= depth)
}

pub fn register_git_memory_service() {
    crate::_base::di::scope::register_scoped_service(
        crate::app::scopes::LifecycleScope::Agent,
        crate::_base::di::scope::ScopeActivation::OnScopeCreated,
        "gitMemory",
    );
}
