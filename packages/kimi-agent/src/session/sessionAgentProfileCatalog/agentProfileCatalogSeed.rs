use std::sync::Arc;

use crate::_base::di::instantiation::create_decorator;
use crate::_base::di::scope::{ScopeSeed, seed_instance};

pub trait ISessionAgentProfileCatalogSeed: Send + Sync {
    fn workspace_key(&self) -> &str;
    fn context_key(&self) -> Option<&str>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAgentProfileCatalogSeed {
    pub workspace_key: String,
    pub context_key: Option<String>,
}

impl ISessionAgentProfileCatalogSeed for SessionAgentProfileCatalogSeed {
    fn workspace_key(&self) -> &str {
        &self.workspace_key
    }

    fn context_key(&self) -> Option<&str> {
        self.context_key.as_deref()
    }
}

pub fn session_agent_profile_catalog_seed(
    seed: Arc<dyn ISessionAgentProfileCatalogSeed>,
) -> ScopeSeed {
    let id = create_decorator::<Arc<dyn ISessionAgentProfileCatalogSeed>>("sessionAgentProfileCatalogSeed");
    seed_instance(&id, seed)
}
