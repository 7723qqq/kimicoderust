use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::utils::paths::{SubtreeWatchFilterOptions, subtree_watch_filter};
use crate::human::utils::watch::{
    WatchHandle, WatchOptions, WatchSubscription, is_watch_enabled, watch_candidates_with_options,
    watch_with_options,
};

pub use crate::skills::*;

const WATCH_DEBOUNCE_MS: u64 = 200;
const MAX_SKILL_SCAN_DEPTH: usize = 8;

#[derive(Clone)]
pub struct ConfiguredSkillRootWatchOptions {
    pub dirs: Vec<String>,
    pub cwd: String,
    pub os_home_dir: String,
    pub source: String,
}

struct WatchEntry {
    handle: Box<WatchHandle>,
    _subscription: Box<WatchSubscription>,
}

pub struct ConfiguredSkillRootWatcher {
    options: ConfiguredSkillRootWatchOptions,
    on_change: Arc<dyn Fn() + Send + Sync>,
    watches: Mutex<HashMap<String, WatchEntry>>,
    watch_revision: AtomicU64,
    debounce_revision: AtomicU64,
    disposed: AtomicBool,
    registration_completed: Mutex<bool>,
}

impl ConfiguredSkillRootWatcher {
    pub fn new(
        options: ConfiguredSkillRootWatchOptions,
        on_change: impl Fn() + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            options,
            on_change: Arc::new(on_change),
            watches: Mutex::new(HashMap::new()),
            watch_revision: AtomicU64::new(0),
            debounce_revision: AtomicU64::new(0),
            disposed: AtomicBool::new(false),
            registration_completed: Mutex::new(false),
        })
    }

    pub async fn registered(self: &Arc<Self>) -> Result<(), String> {
        let mut completed = self.registration_completed.lock();
        if *completed {
            return Ok(());
        }
        if !self.options.dirs.is_empty() && is_watch_enabled() {
            self.update_watches(configured_candidates(&self.options));
        }
        *completed = true;
        Ok(())
    }

    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::Release);
        self.debounce_revision.fetch_add(1, Ordering::AcqRel);
        self.watches.lock().clear();
    }

    fn update_watches(self: &Arc<Self>, candidates: Vec<String>) {
        if self.disposed.load(Ordering::Acquire) {
            return;
        }
        let revision = self.watch_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let mut plan = IndexMap::new();
        for candidate in candidates {
            if let Some(target) = resolve(&candidate) {
                plan.insert(format!("target:{target}"), (target, None));
            }
            let mut entry = PathBuf::from(&candidate);
            while entry.parent().is_some_and(|parent| parent != entry) {
                let Some(parent) = entry.parent() else { break };
                let Some(parent_resolved) = resolve_path(parent) else {
                    entry = parent.to_path_buf();
                    continue;
                };
                let entry_path = parent_resolved.join(entry.file_name().unwrap_or_default());
                let resolved_entry = resolve_path(&entry);
                if entry == PathBuf::from(&candidate)
                    || resolved_entry.is_none()
                    || resolved_entry.as_ref() != Some(&entry_path)
                {
                    plan.insert(
                        format!("entry:{}", normalize(&entry_path)),
                        (normalize(&parent_resolved), Some(normalize(&entry_path))),
                    );
                }
                entry = parent.to_path_buf();
            }
        }
        if self.disposed.load(Ordering::Acquire) || revision != self.watch_revision.load(Ordering::Acquire) {
            return;
        }
        for (key, (path, entry)) in &plan {
            if self.watches.lock().contains_key(key) {
                continue;
            }
            let weak = Arc::downgrade(self);
            let handle = match entry {
                None => watch_target(path),
                Some(entry) => {
                    let changed_root = path.clone();
                    let watched_entry = entry.clone();
                    watch_with_options(
                        path,
                        WatchOptions {
                            recursive: Some(false),
                            listing: Some(false),
                            ignored: Some(Arc::new(move |changed| {
                                let changed = PathBuf::from(changed);
                                changed != Path::new(&changed_root) && changed != Path::new(&watched_entry)
                            })),
                            ..WatchOptions::default()
                        },
                    )
                }
            };
            let change_candidates = candidates.clone();
            let entry_is_some = entry.is_some();
            let subscription = handle.on_did_change(move |_| {
                let Some(this) = weak.upgrade() else { return };
                if entry_is_some {
                    this.update_watches(change_candidates.clone());
                }
                this.invalidate();
            });
            self.watches.lock().insert(
                key.clone(),
                WatchEntry {
                    handle,
                    _subscription: subscription,
                },
            );
        }
        let stale: Vec<_> = self
            .watches
            .lock()
            .keys()
            .filter(|key| !plan.contains_key(*key))
            .cloned()
            .collect();
        for key in stale {
            self.watches.lock().remove(&key);
            self.invalidate();
        }
    }

    fn invalidate(self: &Arc<Self>) {
        if self.disposed.load(Ordering::Acquire) {
            return;
        }
        let revision = self.debounce_revision.fetch_add(1, Ordering::AcqRel) + 1;
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(WATCH_DEBOUNCE_MS));
            let Some(this) = weak.upgrade() else { return };
            if !this.disposed.load(Ordering::Acquire)
                && this.debounce_revision.load(Ordering::Acquire) == revision
            {
                (this.on_change)();
            }
        });
    }
}

impl Drop for ConfiguredSkillRootWatcher {
    fn drop(&mut self) {
        self.dispose();
    }
}

fn watch_target(path: &str) -> Box<WatchHandle> {
    let root = Path::new(path)
        .ancestors()
        .last()
        .unwrap_or_else(|| Path::new(path))
        .to_string_lossy()
        .into_owned();
    let candidate = path.trim_end_matches(['\\', '/']).to_string();
    let filter = subtree_watch_filter(
        &root,
        &[candidate],
        Some(SubtreeWatchFilterOptions {
            max_depth: Some(MAX_SKILL_SCAN_DEPTH + 2),
            skip_entry: Some(Arc::new(|entry| entry == "node_modules" || entry.starts_with('.'))),
            keep_entry_file: Some("SKILL.md".to_string()),
            scanned_directories: None,
        }),
    );
    watch_candidates_with_options(
        &root,
        &[path.to_string()],
        WatchOptions {
            recursive: None,
            ignored: Some(Arc::from(filter)),
            depth: None,
            signal: Some(true),
            listing: None,
        },
    )
}

fn configured_candidates(options: &ConfiguredSkillRootWatchOptions) -> Vec<String> {
    let cwd = absolute_normalized(Path::new(&options.cwd));
    let project_root = cwd
        .ancestors()
        .find(|path| path.join(".git").exists())
        .unwrap_or(&cwd);
    options
        .dirs
        .iter()
        .map(|dir| {
            if dir == "~" {
                options.os_home_dir.clone()
            } else if let Some(rest) = dir.strip_prefix("~/") {
                normalize(&Path::new(&options.os_home_dir).join(rest))
            } else if Path::new(dir).is_absolute() {
                dir.clone()
            } else {
                normalize(&project_root.join(dir))
            }
        })
        .collect()
}

fn resolve(path: &str) -> Option<String> {
    resolve_path(Path::new(path)).map(|path| normalize(&path))
}

fn resolve_path(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

fn absolute_normalized(path: &Path) -> PathBuf {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut normalized = PathBuf::new();
    for component in resolved.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn normalize(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized.to_string_lossy().replace('\\', "/")
}

