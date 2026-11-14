use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::event::Event;
use crate::features::skill::catalog::registry::InMemorySkillCatalog;

pub use crate::skills::{SkillDescriptor, SkillScopes};

pub const SESSION_SKILL_CATALOG_DATA_SERVICE_ID: &str = "sessionSkillCatalogData";

#[async_trait]
pub trait ISessionSkillCatalogData: Send + Sync {
    fn catalog(&self) -> Arc<InMemorySkillCatalog>;
    fn on_did_change(&self) -> Event<String>;
    async fn ready(&self) -> Result<(), String>;
}
