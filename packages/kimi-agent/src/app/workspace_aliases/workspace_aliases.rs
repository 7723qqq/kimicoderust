use async_trait::async_trait;

use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait IWorkspaceAliases: Send + Sync {
    async fn resolve_alias_ids(&self, id: &str) -> Result<Vec<String>, WireAsyncError>;
}
