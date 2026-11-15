use std::sync::Arc;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::agent::tool_executor::before_tool_execute_event::deny_tool_execution;
use crate::agent::tool_executor::tool_executor::IAgentToolExecutorService;
use crate::app::config::config::IConfigService;
use crate::session::session_context::session_context::ISessionContext;

use super::sandbox::{SandboxMode, resolve_sandbox_policy, sandbox_tool_veto};

pub trait ISandboxPolicyService: Send + Sync {}

pub struct SandboxPolicyService {
    _subscription: Option<DisposableRef>,
}

impl SandboxPolicyService {
    pub fn new(
        config: Arc<dyn IConfigService>,
        session_ctx: Arc<dyn ISessionContext>,
        tool_executor: Arc<dyn IAgentToolExecutorService>,
    ) -> Self {
        let policy = resolve_sandbox_policy(config.as_ref(), &session_ctx.cwd());
        if policy.mode == SandboxMode::Off {
            return Self {
                _subscription: None,
            };
        }
        let subscription = tool_executor.on_before_execute_tool(move |event| {
            if let Some(message) = sandbox_tool_veto(
                &policy,
                &event.tool_call.name,
                event.tool.as_ref().and_then(|tool| tool.side_effect),
                event.execution.accesses.as_ref(),
            ) {
                event.veto(deny_tool_execution(message));
            }
        });
        Self {
            _subscription: Some(subscription),
        }
    }
}

impl ISandboxPolicyService for SandboxPolicyService {}
