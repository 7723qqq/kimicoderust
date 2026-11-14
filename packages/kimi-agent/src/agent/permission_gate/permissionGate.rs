use futures_util::future::BoxFuture;

use crate::agent::permission_policy::types::PermissionData;
use crate::agent::tool_executor::tool_hooks::{BeforeExecuteDecision, ResolvedToolExecutionHookContext};
use crate::wire::record::WireAsyncError;

pub trait IAgentPermissionGate: Send + Sync {
    fn data(&self) -> PermissionData;

    fn authorize(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>>;
}
