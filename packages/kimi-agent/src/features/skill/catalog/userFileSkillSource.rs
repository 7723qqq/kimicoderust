use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::event::{Emitter, Event};
use crate::_base::utils::paths::subtree_watch_filter;
use crate::app::config::config::ConfigSectionChangedEvent;
use crate::app::config::config_service::ConfigService;
use crate::human::utils::watch::{WatchHandle, WatchOptions, WatchSubscription, watch_candidates_with_options};

use super::configSection::MERGE_ALL_AVAILABLE_SKILLS_SECTION;
use super::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::skillRoots::{SkillRootsOptions, user_roots};
use super::skillSource::{ISkillSource, SkillContribution, skill_source_priority};

pub use crate::skills::*;

const WATCH_DEBOUNCE_MS: u64 = 200;

struct UserWatchEntry {
    _handle: Box<WatchHandle>,
    _subscription: Box<WatchSubscription>,
}

pub struct UserFileSkillSource {
    discovery: Arc<dyn ISkillDiscovery>,
    home_dir: String,
    os_home_dir: String,
    config: Arc<ConfigService>,
    explicit_dirs: Vec<String>,
    on_did_change_emitter: Arc<Emitter<()>>,
    _watches: Mutex<Vec<UserWatchEntry>>,
    debounce_revision: Arc<AtomicU64>,
    _config_subscription: DisposableRef,
}

impl UserFileSkillSource {
    pub fn new(
        discovery: Arc<dyn ISkillDiscovery>,
        home_dir: String,
        os_home_dir: String,
        config: Arc<ConfigService>,
        explicit_dirs: Vec<String>,
    ) -> Arc<Self> {
        let on_did_change_emitter = Arc::new(Emitter::new());
        let emitter = on_did_change_emitter.clone();
        let subscription = config.on_did_section_change(Box::new(move |event: ConfigSectionChangedEvent| {
            if event.domain == MERGE_ALL_AVAILABLE_SKILLS_SECTION {
                emitter.fire(());
            }
        }));
        let debounce_revision = Arc::new(AtomicU64::new(0));
        let mut watches = Vec::new();
        if explicit_dirs.is_empty() {
            for (base, candidate) in [
                (home_dir.clone(), format!("{}/skills", home_dir.trim_end_matches('/'))),
                (
                    os_home_dir.clone(),
                    format!("{}/.agents/skills", os_home_dir.trim_end_matches('/')),
                ),
            ] {
                let filter = subtree_watch_filter(&base, std::slice::from_ref(&candidate), None);
                let handle = watch_candidates_with_options(
                    &base,
                    &[candidate],
                    WatchOptions {
                        recursive: None,
                        ignored: Some(Arc::from(filter)),
                        depth: None,
                        signal: Some(true),
                        listing: None,
                    },
                );
                let subscription = handle.on_did_change({
                    let emitter = Arc::downgrade(&on_did_change_emitter);
                    let revision = debounce_revision.clone();
                    move |_| schedule_change(emitter.clone(), revision.clone())
                });
                watches.push(UserWatchEntry {
                    _handle: handle,
                    _subscription: subscription,
                });
            }
        }
        Arc::new(Self {
            discovery,
            home_dir,
            os_home_dir,
            config,
            explicit_dirs,
            on_did_change_emitter,
            _watches: Mutex::new(watches),
            debounce_revision,
            _config_subscription: subscription,
        })
    }
}

#[async_trait]
pub trait IUserFileSkillSource: ISkillSource {}

impl IUserFileSkillSource for UserFileSkillSource {}

#[async_trait]
impl ISkillSource for UserFileSkillSource {
    fn id(&self) -> &str {
        "user"
    }

    fn priority(&self) -> i32 {
        skill_source_priority::USER
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
        let roots = user_roots(
            &self.home_dir,
            &self.os_home_dir,
            SkillRootsOptions { merge_all_available_skills },
        )
        .await?;
        let SkillDiscoveryResult {
            skills,
            variants,
            skipped,
            scanned_roots,
            ..
        } = self.discovery.discover(&roots).await?;
        Ok(SkillContribution {
            skills,
            variants: Some(variants),
            skipped: Some(skipped),
            scanned_roots: Some(scanned_roots),
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
