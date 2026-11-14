use async_trait::async_trait;

use crate::wire::record::WireAsyncError;

#[async_trait]
pub trait IAgentPluginService: Send + Sync {
    async fn refresh_session_start(&self) -> Result<(), WireAsyncError>;
}
