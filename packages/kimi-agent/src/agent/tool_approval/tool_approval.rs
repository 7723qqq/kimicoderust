use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
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


pub static I_AGENT_TOOL_APPROVAL_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentToolApprovalService>>> =
    LazyLock::new(|| create_decorator("agentToolApprovalService"));
