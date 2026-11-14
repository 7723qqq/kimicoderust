use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::event::Event;
use crate::features::skill::catalog::registry::InMemorySkillCatalog;
use crate::features::skill::catalog::skillSource::SkillContribution;
use crate::features::skill::catalog::types::SkillSummary;

pub use crate::skills::*;

pub const SESSION_SKILL_CATALOG_SERVICE_ID: &str = "sessionSkillCatalog";

#[async_trait]
pub trait ISessionSkillCatalog: Send + Sync {
    fn catalog(&self) -> Arc<InMemorySkillCatalog>;
    fn on_did_change(&self) -> Event<String>;
    async fn ready(&self) -> Result<(), String>;
    async fn load(&self) -> Result<(), String>;
    async fn reload(&self) -> Result<(), String>;
    async fn list(&self) -> Result<Vec<SkillSummary>, String>;
}

pub trait ISkillCatalogSink: Send + Sync {
    fn set(&self, id: String, contribution: SkillContribution, priority: i32);
    fn remove(&self, id: &str);
}
