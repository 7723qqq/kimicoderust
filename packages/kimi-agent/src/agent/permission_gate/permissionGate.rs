use std::sync::Arc;

use once_cell::sync::Lazy;
use futures_util::future::BoxFuture;

use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
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

pub static I_AGENT_PERMISSION_GATE: Lazy<ServiceIdentifier<Arc<dyn IAgentPermissionGate>>> =
    Lazy::new(|| create_decorator("agentPermissionGate"));
