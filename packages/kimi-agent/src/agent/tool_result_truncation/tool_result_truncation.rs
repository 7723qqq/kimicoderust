use std::sync::{Arc, LazyLock};

use futures_util::future::BoxFuture;

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
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

pub static I_AGENT_TOOL_RESULT_TRUNCATION_SERVICE: LazyLock<
    ServiceIdentifier<Arc<dyn IAgentToolResultTruncationService>>,
> = LazyLock::new(|| create_decorator("agentToolResultTruncationService"));
