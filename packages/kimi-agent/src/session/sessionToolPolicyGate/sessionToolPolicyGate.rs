use std::sync::Arc;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::ScopeSeed;

pub trait ISessionToolPolicyGate: Send + Sync {
    fn disabled_tools(&self) -> Vec<String>;
    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef;
}

pub fn session_tool_policy_gate_seed(gate: Arc<dyn ISessionToolPolicyGate>) -> Vec<ScopeSeed> {
    vec![ScopeSeed::new::<ISessionToolPolicyGate>(gate)]
}
