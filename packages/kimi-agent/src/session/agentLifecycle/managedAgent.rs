use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::Mutex;

use crate::_base::di::scope::IAgentScopeHandle;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::agent_context::agent_space::AgentSpaceImpl;
use crate::agent::r#loop::machine::engine::MachineEngineAttachBundle;
use crate::human::session::machine::AgentActorRef;

pub struct ManagedAgent {
    pub active: AtomicBool,
    pub closing: AtomicBool,
    pub ref_: Mutex<Option<AgentActorRef>>,
    pub bundle: Mutex<Option<MachineEngineAttachBundle>>,
    pub context: Arc<AgentContext>,
    pub handle: Arc<IAgentScopeHandle>,
}

impl ManagedAgent {
    pub fn new(context: Arc<AgentContext>, handle: Arc<IAgentScopeHandle>) -> Self {
        Self {
            active: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            ref_: Mutex::new(None),
            bundle: Mutex::new(None),
            context,
            handle,
        }
    }

    pub fn kill_space(&self) {
        if let Some(space) = self.context.space.clone() {
            tokio::spawn(async move {
                space.kill().await;
            });
        }
    }
}
