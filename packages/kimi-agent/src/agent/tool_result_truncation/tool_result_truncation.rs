use futures_util::future::BoxFuture;

use crate::agent::tool_executor::tool_executor::{ToolResult, ToolTruncationInput};
use crate::wire::record::WireAsyncError;

pub type ToolResultTruncationInput = ToolTruncationInput;

pub trait IAgentToolResultTruncationService: Send + Sync {
    fn truncate_for_model(
        &self,
        input: ToolResultTruncationInput,
    ) -> BoxFuture<'static, Result<ToolResult, WireAsyncError>>;

    fn is_spill_file_path(&self, path: &str) -> bool;

    fn is_wire_journal_path(&self, path: &str) -> bool;
}
