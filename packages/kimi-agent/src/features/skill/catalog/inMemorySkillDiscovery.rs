use async_trait::async_trait;
use parking_lot::RwLock;

use super::skillDiscovery::{ISkillDiscovery, SkillDiscoveryResult};
use super::types::{SkillDefinition, SkillRoot};

pub use crate::skills::*;

#[derive(Default)]
pub struct InMemorySkillDiscovery {
    project_skills: RwLock<Vec<SkillDefinition>>,
    user_skills: RwLock<Vec<SkillDefinition>>,
    plugin_skills: RwLock<Vec<SkillDefinition>>,
    extra_skills: RwLock<Vec<SkillDefinition>>,
}

impl InMemorySkillDiscovery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_project_skills(&self, skills: Vec<SkillDefinition>) {
        *self.project_skills.write() = skills;
    }

    pub fn set_user_skills(&self, skills: Vec<SkillDefinition>) {
        *self.user_skills.write() = skills;
    }

    pub fn set_plugin_skills(&self, skills: Vec<SkillDefinition>) {
        *self.plugin_skills.write() = skills;
    }

    pub fn set_extra_skills(&self, skills: Vec<SkillDefinition>) {
        *self.extra_skills.write() = skills;
    }
}

#[async_trait]
impl ISkillDiscovery for InMemorySkillDiscovery {
    async fn discover(&self, roots: &[SkillRoot]) -> Result<SkillDiscoveryResult, String> {
        let mut skills = Vec::new();
        if roots.is_empty() {
            skills.extend(self.user_skills.read().iter().cloned());
            skills.extend(self.project_skills.read().iter().cloned());
        } else {
            if roots.iter().any(|root| root.plugin.is_some()) {
                skills.extend(self.plugin_skills.read().iter().cloned());
            }
            if roots.iter().any(|root| root.source == "extra") {
                skills.extend(self.extra_skills.read().iter().cloned());
            }
            if roots.iter().any(|root| root.source == "user") {
                skills.extend(self.user_skills.read().iter().cloned());
            }
            if roots.iter().any(|root| root.source == "project") {
                skills.extend(self.project_skills.read().iter().cloned());
            }
        }
        Ok(SkillDiscoveryResult {
            variants: skills.clone(),
            skills,
            skipped: Vec::new(),
            scanned_roots: Vec::new(),
            scanned_directories: Vec::new(),
        })
    }
}
