use async_trait::async_trait;

use super::types::{SkillDefinition, SkillRoot, SkippedSkill};

pub use crate::skills::*;

#[derive(Debug, Clone, Default)]
pub struct SkillDiscoveryResult {
    pub skills: Vec<SkillDefinition>,
    pub variants: Vec<SkillDefinition>,
    pub skipped: Vec<SkippedSkill>,
    pub scanned_roots: Vec<String>,
    pub scanned_directories: Vec<String>,
}

#[async_trait]
pub trait ISkillDiscovery: Send + Sync {
    async fn discover(&self, roots: &[SkillRoot]) -> Result<SkillDiscoveryResult, String>;
}
