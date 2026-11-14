use futures_util::future::BoxFuture;

use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;
use crate::wire::record::WireAsyncError;

use super::types::PermissionPolicyResult;

pub struct PermissionPolicyEvaluation {
    pub policy_name: String,
    pub result: PermissionPolicyResult,
}

pub trait IAgentPermissionPolicyService: Send + Sync {
    fn evaluate(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyEvaluation>, WireAsyncError>>;
}
