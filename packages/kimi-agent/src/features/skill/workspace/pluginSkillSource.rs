use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::event::{Event, Subscription};
use crate::app::plugin::plugin::IPluginService;

use super::super::catalog::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::super::catalog::skillSource::{ISkillSource, SkillContribution, skill_source_priority};

pub use crate::skills::{PluginSkillDir, SkillDescriptor};
pub use super::super::catalog::skillSource::PLUGIN_SKILL_SOURCE_ID;

pub struct PluginSkillSource {
    discovery: Arc<dyn ISkillDiscovery>,
    plugins: Arc<dyn IPluginService>,
}

impl PluginSkillSource {
    pub fn new(discovery: Arc<dyn ISkillDiscovery>, plugins: Arc<dyn IPluginService>) -> Self {
        Self { discovery, plugins }
    }
}

#[async_trait]
pub trait IPluginSkillSource: ISkillSource {}

impl IPluginSkillSource for PluginSkillSource {}

impl ISkillSource for PluginSkillSource {
    fn id(&self) -> &str {
        PLUGIN_SKILL_SOURCE_ID
    }

    fn priority(&self) -> i32 {
        skill_source_priority::PLUGIN
    }

    fn on_did_change(&self) -> Option<Event<()>> {
        let plugins = self.plugins.clone();
        Some(Event::new(move |listener| {
            let disposable = plugins.on_did_reload(Arc::new(move |_| listener(())));
            Subscription::new(None, move || disposable.dispose())
        }))
    }

    async fn load(&self) -> Result<SkillContribution, String> {
        let roots = self
            .plugins
            .plugin_skill_roots()
            .await
            .map_err(|error| error.to_string())?;
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
