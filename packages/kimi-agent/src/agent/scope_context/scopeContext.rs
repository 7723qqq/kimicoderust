use crate::_base::di::instantiation::ServicesAccessorExt;
use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use std::sync::Arc;

use crate::_base::di::scope::IAgentScopeHandle;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::agent_context::agent_space::AgentSpaceImpl;

pub trait IAgentScopeContext: Send + Sync {
    fn agent_id(&self) -> String;
    fn forked_from(&self) -> Option<String>;
    fn agent_context(&self) -> Arc<AgentContext>;
    fn scope(&self, sub_key: Option<&str>) -> String;
}

pub struct AgentScopeContext {
    pub agent_id: String,
    pub forked_from: Option<String>,
    pub agent_context: Arc<AgentContext>,
    agent_scope: String,
}

impl IAgentScopeContext for AgentScopeContext {
    fn agent_id(&self) -> String {
        self.agent_id.clone()
    }

    fn forked_from(&self) -> Option<String> {
        self.forked_from.clone()
    }

    fn agent_context(&self) -> Arc<AgentContext> {
        self.agent_context.clone()
    }

    fn scope(&self, sub_key: Option<&str>) -> String {
        match sub_key.filter(|sub_key| !sub_key.is_empty()) {
            None => self.agent_scope.clone(),
            Some(sub_key) if self.agent_scope.is_empty() => sub_key.to_string(),
            Some(sub_key) => format!("{}/{sub_key}", self.agent_scope),
        }
    }
}

pub fn make_agent_scope_context(
    agent_id: String,
    agent_scope: String,
    forked_from: Option<String>,
    generation: u32,
) -> Arc<AgentScopeContext> {
    let space = Arc::new(AgentSpaceImpl::new(agent_id.clone()));
    let agent_context = Arc::new(AgentContext {
        agent_id: agent_id.clone(),
        generation,
        space: Some(space.clone()),
    });
    space.bind_context((*agent_context).clone());
    Arc::new(AgentScopeContext {
        agent_id,
        forked_from,
        agent_context,
        agent_scope,
    })
}

pub fn agent_context_of_scope(scope: &Arc<dyn IAgentScopeContext>) -> Arc<AgentContext> {
    scope.agent_context()
}

pub fn agent_context_of(handle: &Arc<IAgentScopeHandle>) -> Arc<AgentContext> {
    agent_context_of_scope(&handle.accessor().get(&I_AGENT_SCOPE_CONTEXT))
}

pub fn try_agent_context_of(handle: &Arc<IAgentScopeHandle>) -> Option<Arc<AgentContext>> {
    handle
        .accessor()
        .try_get(&I_AGENT_SCOPE_CONTEXT)
        .map(|scope| scope.agent_context())
}


pub static I_AGENT_SCOPE_CONTEXT: LazyLock<ServiceIdentifier<Arc<dyn IAgentScopeContext>>> =
    LazyLock::new(|| create_decorator("agentScopeContext"));
