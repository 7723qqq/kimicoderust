use indexmap::IndexMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::event::{Emitter, Event};
use crate::_base::utils::paths::{SubtreeWatchFilterOptions, subtree_watch_filter};
use crate::app::config::config::IConfigService;
use crate::human::utils::watch::{WatchHandle, WatchOptions, WatchSubscription, watch_candidates_with_options};
use crate::workspace::workspaceDirs::workspaceDirsService::WorkspaceDirsService;

use super::super::catalog::configSection::MERGE_ALL_AVAILABLE_SKILLS_SECTION;
use super::super::catalog::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::super::catalog::skillRoots::{
    SkillRootsOptions, project_roots_for_dirs, project_skill_root_candidates,
};
use super::super::catalog::skillSource::{ISkillSource, SkillContribution, skill_source_priority};
pub use crate::skills::*;

pub const WORKSPACE_ROOT_SKILL_SOURCE_ID: &str = "workspace";
const WATCH_DEBOUNCE_MS: u64 = 200;

struct WatchEntry {
    _handle: Box<WatchHandle>,
    _subscription: Box<WatchSubscription>,
}

#[derive(Default)]
struct WatchState {
    entries: Vec<WatchEntry>,
    signature: Option<String>,
}

pub struct WorkspaceRootSkillSource {
    discovery: Arc<dyn ISkillDiscovery>,
    workspace_id: String,
    workspace_cwd: String,
    config: Arc<dyn IConfigService>,
    explicit_dirs: Vec<String>,
    dirs: Arc<WorkspaceDirsService>,
    watch_enabled: bool,
    on_did_change_emitter: Arc<Emitter<()>>,
    watch_state: Mutex<WatchState>,
    debounce_revision: Arc<AtomicU64>,
    _config_subscription: DisposableRef,
    _dirs_subscription: DisposableRef,
}

impl WorkspaceRootSkillSource {
    pub fn new(
        discovery: Arc<dyn ISkillDiscovery>,
        workspace_id: String,
        workspace_cwd: String,
        config: Arc<dyn IConfigService>,
        explicit_dirs: Vec<String>,
        dirs: Arc<WorkspaceDirsService>,
        watch_enabled: bool,
    ) -> Arc<Self> {
        let on_did_change_emitter = Arc::new(Emitter::new());
        let config_emitter = on_did_change_emitter.clone();
        let config_subscription = config.on_did_section_change(Box::new(move |event| {
            if event.domain == MERGE_ALL_AVAILABLE_SKILLS_SECTION {
                config_emitter.fire(());
            }
        }));
        let dirs_emitter = on_did_change_emitter.clone();
        let dirs_subscription = dirs.on_did_change(Arc::new(move || dirs_emitter.fire(())));
        Arc::new(Self {
            discovery,
            workspace_id,
            workspace_cwd,
            config,
            explicit_dirs,
            dirs,
            watch_enabled,
            on_did_change_emitter,
            watch_state: Mutex::new(WatchState::default()),
            debounce_revision: Arc::new(AtomicU64::new(0)),
            _config_subscription: config_subscription,
            _dirs_subscription: dirs_subscription,
        })
    }

    fn work_dirs(&self) -> Vec<String> {
        let primary = self
            .dirs
            .primary_by_workspace()
            .get(&self.workspace_id)
            .cloned()
            .unwrap_or_else(|| self.workspace_cwd.clone());
        let mut dirs = vec![primary, self.workspace_cwd.clone()];
        dirs.extend(self.dirs.shared_additional_dirs());
        dirs
    }

    async fn discover(&self, merge_all_available_skills: bool) -> Result<SkillDiscoveryResult, String> {
        let roots = project_roots_for_dirs(
            &self.work_dirs(),
            SkillRootsOptions { merge_all_available_skills },
        )
        .await?;
        self.discovery.discover(&roots).await
    }

    async fn update_project_skill_root_watches(
        &self,
        scanned_directories: &[String],
    ) -> Result<bool, String> {
        if !self.watch_enabled {
            return Ok(false);
        }
        let mut watches = IndexMap::new();
        for work_dir in self.work_dirs() {
            let candidates = project_skill_root_candidates(&work_dir).await?;
            watches.entry(candidates.project_root).or_insert(candidates.candidates);
        }
        let mut sorted_scanned = scanned_directories.to_vec();
        sorted_scanned.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        let watch_entries: Vec<_> = watches
            .iter()
            .map(|(root, candidates)| (root.clone(), candidates.clone()))
            .collect();
        let signature = serde_json::json!({
            "watches": watch_entries,
            "scannedDirectories": sorted_scanned,
        })
        .to_string();
        let mut state = self.watch_state.lock().await;
        if state.signature.as_deref() == Some(&signature) {
            return Ok(false);
        }
        let mut entries = Vec::new();
        for (project_root, candidates) in watches {
            let filter = subtree_watch_filter(
                &project_root,
                &candidates,
                Some(SubtreeWatchFilterOptions {
                    max_depth: None,
                    skip_entry: None,
                    keep_entry_file: Some("SKILL.md".to_string()),
                    scanned_directories: Some(scanned_directories.to_vec()),
                }),
            );
            let handle = watch_candidates_with_options(
                &project_root,
                &candidates,
                WatchOptions {
                    recursive: None,
                    ignored: Some(Arc::from(filter)),
                    depth: None,
                    signal: Some(true),
                    listing: None,
                },
            );
            let subscription = handle.on_did_change({
                let emitter = Arc::downgrade(&self.on_did_change_emitter);
                let revision = self.debounce_revision.clone();
                move |_| schedule_change(emitter.clone(), revision.clone())
            });
            entries.push(WatchEntry {
                _handle: handle,
                _subscription: subscription,
            });
        }
        state.entries = entries;
        state.signature = Some(signature);
        Ok(true)
    }
}

#[async_trait]
impl ISkillSource for WorkspaceRootSkillSource {
    fn id(&self) -> &str {
        WORKSPACE_ROOT_SKILL_SOURCE_ID
    }

    fn priority(&self) -> i32 {
        skill_source_priority::WORKSPACE
    }

    fn on_did_change(&self) -> Option<Event<()>> {
        Some(self.on_did_change_emitter.event())
    }

    async fn load(&self) -> Result<SkillContribution, String> {
        if !self.explicit_dirs.is_empty() {
            return Ok(SkillContribution::default());
        }
        self.config.ready().await.map_err(|error| error.to_string())?;
        let merge_all_available_skills = self
            .config
            .get(MERGE_ALL_AVAILABLE_SKILLS_SECTION)
            .and_then(|value| match value {
                Value::Bool(value) => Some(value),
                _ => None,
            })
            .unwrap_or(true);
        self.dirs.ready().await.map_err(|error| error.to_string())?;
        let mut contribution = self.discover(merge_all_available_skills).await?;
        while self
            .update_project_skill_root_watches(&contribution.scanned_directories)
            .await?
        {
            contribution = self.discover(merge_all_available_skills).await?;
        }
        Ok(SkillContribution {
            skills: contribution.skills,
            variants: Some(contribution.variants),
            skipped: Some(contribution.skipped),
            scanned_roots: Some(contribution.scanned_roots),
        })
    }
}

fn schedule_change(emitter: Weak<Emitter<()>>, revision: Arc<AtomicU64>) {
    let current = revision.fetch_add(1, Ordering::AcqRel) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS));
        if revision.load(Ordering::Acquire) != current {
            return;
        }
        if let Some(emitter) = emitter.upgrade() {
            emitter.fire(());
        }
    });
}

