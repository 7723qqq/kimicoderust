use async_trait::async_trait;

use crate::_base::event::Event;

use super::types::{SkillDefinition, SkippedSkill};

pub use crate::skills::*;

#[derive(Debug, Clone, Default)]
pub struct SkillContribution {
    pub skills: Vec<SkillDefinition>,
    pub variants: Option<Vec<SkillDefinition>>,
    pub skipped: Option<Vec<SkippedSkill>>,
    pub scanned_roots: Option<Vec<String>>,
}

pub mod skill_source_priority {
    pub const BUILTIN: i32 = 0;
    pub const PLUGIN: i32 = 5;
    pub const EXTRA: i32 = 10;
    pub const USER: i32 = 20;
    pub const WORKSPACE: i32 = 30;
}

pub const PLUGIN_SKILL_SOURCE_ID: &str = "plugin";
pub const BUILTIN_SKILL_SOURCE_ID: &str = "builtin";

#[async_trait]
pub trait ISkillSource: Send + Sync {
    fn id(&self) -> &str;
    fn priority(&self) -> i32;
    fn on_did_change(&self) -> Option<Event<()>> {
        None
    }
    async fn load(&self) -> Result<SkillContribution, String>;
}
