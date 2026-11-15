use std::sync::Arc;

use once_cell::sync::Lazy;
use futures_util::future::BoxFuture;

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
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

pub static I_AGENT_PERMISSION_POLICY_SERVICE: Lazy<
    ServiceIdentifier<Arc<dyn IAgentPermissionPolicyService>>,
> = Lazy::new(|| create_decorator("agentPermissionPolicyService"));
