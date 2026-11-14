use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::IDisposable;
use crate::agent::agent_context::agent_context::AgentContext;

use super::event2::{Event2, Event2Class};

pub type Event2Handler = Arc<dyn Fn(Event2) + Send + Sync>;

pub trait IEventBus: Send + Sync {
    fn publish(&self, event: Event2, agent: Option<Arc<AgentContext>>);
    fn subscribe(&self, handler: Event2Handler) -> Box<dyn IDisposable>;
    fn subscribe_class(
        &self,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable>;
    fn subscribe_type(&self, type_: &str, handler: Event2Handler) -> Box<dyn IDisposable>;
}

pub static I_EVENT_BUS: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("eventBus"));

pub trait ISessionEventBus: IEventBus {
    fn activate_agent(&self, agent: Arc<AgentContext>);
    fn deactivate_agent(&self, agent: Arc<AgentContext>);
    fn is_agent_active(&self, agent: &Arc<AgentContext>) -> bool;
    fn source_of(&self, event: &Event2) -> Option<Arc<AgentContext>>;
    fn subscribe_agent(
        &self,
        agent: Arc<AgentContext>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable>;
    fn subscribe_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable>;
    fn on_agent_class(
        &self,
        agent: Arc<AgentContext>,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable>;
    fn on_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable>;
}

pub static I_SESSION_EVENT_BUS: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("sessionEventBus"));
