use futures_util::future::BoxFuture;

use crate::agent::permission_policy::types::{
    ApprovalResponse, PermissionPolicyResolution, PermissionPolicyResult,
};
use crate::agent::tool_executor::tool_hooks::{BeforeExecuteDecision, ResolvedToolExecutionHookContext};
use crate::wire::record::WireAsyncError;

pub trait IAgentToolApprovalService: Send + Sync {
    fn resolve_permission_resolution(
        &self,
        result: PermissionPolicyResolution,
        context: ResolvedToolExecutionHookContext,
        origin: String,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>>;

    fn request_tool_approval(
        &self,
        context: ResolvedToolExecutionHookContext,
        result: PermissionPolicyResult,
        origin: String,
    ) -> BoxFuture<'static, Result<Option<BeforeExecuteDecision>, WireAsyncError>>;

    fn format_deny_message(&self, message: &str) -> String;

    fn format_approval_rejection_message(
        &self,
        tool_name: &str,
        result: &ApprovalResponse,
    ) -> String;
}
