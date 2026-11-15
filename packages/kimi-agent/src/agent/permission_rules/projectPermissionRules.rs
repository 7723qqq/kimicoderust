use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use async_trait::async_trait;

use crate::wire::record::WireAsyncError;

use super::permission_rules::PermissionRule;

#[async_trait]
pub trait IProjectPermissionRulesService: Send + Sync {
    async fn ready(&self) -> Result<(), WireAsyncError>;
    fn rules(&self) -> Vec<PermissionRule>;
    async fn append_rules(&self, rules: &[PermissionRule]) -> Result<(), WireAsyncError>;
}


pub static I_PROJECT_PERMISSION_RULES_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IProjectPermissionRulesService>>> =
    LazyLock::new(|| create_decorator("projectPermissionRulesService"));
