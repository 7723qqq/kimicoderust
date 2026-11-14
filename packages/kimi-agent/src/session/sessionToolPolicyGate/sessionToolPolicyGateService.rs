use std::sync::Arc;

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;

use super::session_tool_policy_gate::ISessionToolPolicyGate;

pub struct NoopSessionToolPolicyGate;

impl ISessionToolPolicyGate for NoopSessionToolPolicyGate {
    fn disabled_tools(&self) -> Vec<String> {
        Vec::new()
    }

    fn on_did_change(&self, _listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn IDisposable> {
        to_disposable(|| {})
    }
}

pub fn register_noop_session_tool_policy_gate() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "sessionToolPolicyGate",
    );
}
