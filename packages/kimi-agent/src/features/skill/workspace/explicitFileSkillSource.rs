use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::event::{Emitter, Event};

use super::super::catalog::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::super::catalog::skillRoots::configured_roots;
use super::super::catalog::skillSource::{
    ISkillSource, SkillContribution, skill_source_priority,
};
use super::super::catalog::types::SkillSource;
use super::configuredSkillRootWatcher::{
    ConfiguredSkillRootWatchOptions, ConfiguredSkillRootWatcher,
};

pub use crate::skills::*;

pub struct ExplicitFileSkillSource {
    discovery: Arc<dyn ISkillDiscovery>,
    workspace_cwd: String,
    os_home_dir: String,
    explicit_dirs: Vec<String>,
    on_did_change_emitter: Arc<Emitter<()>>,
    watcher: Arc<ConfiguredSkillRootWatcher>,
}

impl ExplicitFileSkillSource {
    pub fn new(
        discovery: Arc<dyn ISkillDiscovery>,
        workspace_cwd: String,
        os_home_dir: String,
        explicit_dirs: Vec<String>,
    ) -> Arc<Self> {
        let on_did_change_emitter = Arc::new(Emitter::new());
        let emitter = on_did_change_emitter.clone();
        let watcher = ConfiguredSkillRootWatcher::new(
            ConfiguredSkillRootWatchOptions {
                dirs: explicit_dirs.clone(),
                cwd: workspace_cwd.clone(),
                os_home_dir: os_home_dir.clone(),
                source: "explicit".to_string(),
            },
            move || emitter.fire(()),
        );
        Arc::new(Self {
            discovery,
            workspace_cwd,
            os_home_dir,
            explicit_dirs,
            on_did_change_emitter,
            watcher,
        })
    }
}

#[async_trait]
pub trait IExplicitFileSkillSource: ISkillSource {}

impl IExplicitFileSkillSource for ExplicitFileSkillSource {}

impl ISkillSource for ExplicitFileSkillSource {
    fn id(&self) -> &str {
        "explicit"
    }

    fn priority(&self) -> i32 {
        skill_source_priority::USER
    }

    fn on_did_change(&self) -> Option<Event<()>> {
        Some(self.on_did_change_emitter.event())
    }

    async fn load(&self) -> Result<SkillContribution, String> {
        if self.explicit_dirs.is_empty() {
            return Ok(SkillContribution::default());
        }
        self.watcher.registered().await?;
        let roots = configured_roots(
            &self.explicit_dirs,
            &self.workspace_cwd,
            &self.os_home_dir,
            SkillSource::User,
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
