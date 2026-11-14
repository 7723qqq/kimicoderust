use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::event::Event;

use super::super::catalog::registry::InMemorySkillCatalog;
use super::super::session::skillCatalogData::ISessionSkillCatalogData;

pub use crate::skills::*;

#[async_trait]
pub trait IWorkspaceSkillCatalog: Send + Sync {
    fn catalog(&self) -> Arc<InMemorySkillCatalog>;
    fn on_did_change(&self) -> Event<String>;
    async fn ready(&self) -> Result<(), String>;
    async fn load(&self) -> Result<(), String>;
    async fn reload(&self) -> Result<(), String>;
    async fn reload_sources(&self, ids: &[String]) -> Result<(), String>;
    fn session_data(&self) -> Arc<dyn ISessionSkillCatalogData>;
}
