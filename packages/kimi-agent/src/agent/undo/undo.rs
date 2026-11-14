use async_trait::async_trait;

use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoAvailability {
    pub max_turns: u64,
    pub stopped_at_compaction: bool,
}

#[async_trait]
pub trait IAgentConversationUndoService: Send + Sync {
    fn availability(&self) -> UndoAvailability;
    async fn undo(&self, turns: u64) -> Result<u64, WireAsyncError>;
}
