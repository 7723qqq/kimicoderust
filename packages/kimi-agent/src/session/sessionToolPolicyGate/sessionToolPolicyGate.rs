use std::sync::Arc;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::instantiation::create_decorator;
use crate::_base::di::scope::{ScopeSeed, seed_instance};

pub trait ISessionToolPolicyGate: Send + Sync {
    fn disabled_tools(&self) -> Vec<String>;
    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef;
}

pub fn session_tool_policy_gate_seed(gate: Arc<dyn ISessionToolPolicyGate>) -> ScopeSeed {
    let id = create_decorator::<Arc<dyn ISessionToolPolicyGate>>("sessionToolPolicyGate");
    seed_instance(&id, gate)
}
