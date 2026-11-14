use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::{Emitter, Event};
use crate::app::config::config_service::ConfigService;

use super::super::catalog::configSection::EXTRA_SKILL_DIRS_SECTION;
use super::super::catalog::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::super::catalog::skillRoots::configured_roots;
use super::super::catalog::skillSource::{ISkillSource, SkillContribution, skill_source_priority};
use super::super::catalog::types::SkillSource;
use super::configuredSkillRootWatcher::{ConfiguredSkillRootWatchOptions, ConfiguredSkillRootWatcher};

pub use crate::skills::*;

struct ExtraWatchState {
    watched_dirs: Vec<String>,
    watcher: Option<Arc<ConfiguredSkillRootWatcher>>,
}

pub struct ExtraFileSkillSource {
    discovery: Arc<dyn ISkillDiscovery>,
    config: Arc<ConfigService>,
    workspace_cwd: String,
    os_home_dir: String,
    on_did_change_emitter: Arc<Emitter<()>>,
    watch_state: Mutex<ExtraWatchState>,
    _config_subscription: Box<dyn IDisposable>,
}

impl ExtraFileSkillSource {
    pub fn new(
        discovery: Arc<dyn ISkillDiscovery>,
        config: Arc<ConfigService>,
        workspace_cwd: String,
        os_home_dir: String,
    ) -> Arc<Self> {
        let on_did_change_emitter = Arc::new(Emitter::new());
        let emitter = on_did_change_emitter.clone();
        let config_subscription = config.on_did_section_change(move |event| {
            if event.domain == EXTRA_SKILL_DIRS_SECTION {
                emitter.fire(());
            }
        });
        Arc::new(Self {
            discovery,
            config,
            workspace_cwd,
            os_home_dir,
            on_did_change_emitter,
            watch_state: Mutex::new(ExtraWatchState {
                watched_dirs: Vec::new(),
                watcher: None,
            }),
            _config_subscription: config_subscription,
        })
    }
}

#[async_trait]
pub trait IExtraFileSkillSource: ISkillSource {}

impl IExtraFileSkillSource for ExtraFileSkillSource {}

impl ISkillSource for ExtraFileSkillSource {
    fn id(&self) -> &str {
        "extra"
    }

    fn priority(&self) -> i32 {
        skill_source_priority::EXTRA
    }

    fn on_did_change(&self) -> Option<Event<()>> {
        Some(self.on_did_change_emitter.event())
    }

    async fn load(&self) -> Result<SkillContribution, String> {
        self.config.ready().await.map_err(|error| error.to_string())?;
        let extra_skill_dirs = self
            .config
            .get(EXTRA_SKILL_DIRS_SECTION)
            .and_then(|value| match value {
                Value::Array(values) => Some(
                    values
                        .into_iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .unwrap_or_default();
        let watcher = {
            let mut state = self.watch_state.lock().await;
            if state.watched_dirs != extra_skill_dirs {
                state.watched_dirs = extra_skill_dirs.clone();
                state.watcher = if extra_skill_dirs.is_empty() {
                    None
                } else {
                    let emitter = self.on_did_change_emitter.clone();
                    Some(ConfiguredSkillRootWatcher::new(
                        ConfiguredSkillRootWatchOptions {
                            dirs: extra_skill_dirs.clone(),
                            cwd: self.workspace_cwd.clone(),
                            os_home_dir: self.os_home_dir.clone(),
                            source: "extra".to_string(),
                        },
                        move || emitter.fire(()),
                    ))
                };
            }
            state.watcher.clone()
        };
        if let Some(watcher) = watcher {
            watcher.registered().await?;
        }
        let roots = configured_roots(
            &extra_skill_dirs,
            &self.workspace_cwd,
            &self.os_home_dir,
            SkillSource::Extra,
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
