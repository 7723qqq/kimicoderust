use std::sync::Arc;

use crate::agent::agent_context::agent_space::AgentSpaceImpl;

#[derive(Clone)]
pub struct AgentContext {
    pub agent_id: String,
    pub generation: u32,
    pub space: Option<Arc<AgentSpaceImpl>>,
}

impl PartialEq for AgentContext {
    fn eq(&self, other: &Self) -> bool {
        self.agent_id == other.agent_id
            && self.generation == other.generation
            && match (&self.space, &other.space) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for AgentContext {}
