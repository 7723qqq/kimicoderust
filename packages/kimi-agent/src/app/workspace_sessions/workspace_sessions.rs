use async_trait::async_trait;

pub use crate::app::session_index::session_index::SessionSummary;
use crate::wire::record::WireAsyncError;

pub const RECENT_SESSIONS_LIMIT: usize = 20;

#[async_trait]
pub trait IWorkspaceSessions: Send + Sync {
    async fn list_recent(&self, workspace_id: &str) -> Result<Vec<SessionSummary>, WireAsyncError>;
    async fn count(&self, workspace_id: &str) -> Result<i64, WireAsyncError>;
}
