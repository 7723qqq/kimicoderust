use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde_json::Value;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ScopeActivation, ServiceDependency, create_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable};
use crate::_base::di::scope::register_scoped_service;
use crate::_base::event::{Emitter, Subscription};
use crate::agent::agent_context::agent_context::AgentContext;
use crate::agent::scope_context::scope_context::IAgentScopeContext;
use crate::app::scopes::LifecycleScope;

use super::event2::{Event2, Event2Class};
use super::event_bus::{
    Event2Handler, I_EVENT_BUS, I_SESSION_EVENT_BUS, IEventBus, ISessionEventBus,
};

static EVENT_CLASS_DOMAINS: Lazy<Mutex<HashMap<String, bool>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn note_event_class(cls: &Event2Class) {
    EVENT_CLASS_DOMAINS
        .lock()
        .insert(cls.type_.clone(), cls.agent_domain);
}

fn event_agent_id(event: &Event2) -> Option<&str> {
    event.payload.get("agentId").and_then(Value::as_str)
}

fn event_is_agent_domain(event: &Event2) -> bool {
    EVENT_CLASS_DOMAINS
        .lock()
        .get(&event.type_)
        .copied()
        .unwrap_or_else(|| event_agent_id(event).is_some())
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct EventKey {
    type_: String,
    time: i64,
    payload: String,
}

fn event_key(event: &Event2) -> EventKey {
    EventKey {
        type_: event.type_.clone(),
        time: event.time,
        payload: serde_json::to_string(&event.payload).unwrap(),
    }
}

struct AgentChannel {
    all: Arc<Emitter<Event2>>,
}

impl AgentChannel {
    fn new(agent_id: &str) -> Self {
        Self {
            all: Arc::new(Emitter::with_debug_name(Some(&format!("agent:{agent_id}")))),
        }
    }

    fn dispose(&self) {
        self.all.dispose();
    }
}

struct EventBusInner {
    all_emitter: Arc<Emitter<Event2>>,
    per_type: Mutex<IndexMap<String, Arc<Emitter<Event2>>>>,
    per_agent: Mutex<IndexMap<String, Arc<AgentChannel>>>,
    agents: Mutex<HashMap<String, Arc<AgentContext>>>,
    sources: Mutex<HashMap<EventKey, Arc<AgentContext>>>,
    disposed_bus: AtomicBool,
    subscriptions: DisposableStore,
}

#[derive(Clone)]
pub struct EventBusService {
    inner: Arc<EventBusInner>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventBusListenerCounts {
    pub all: usize,
    pub per_type: IndexMap<String, usize>,
    pub per_agent: IndexMap<String, usize>,
}

impl EventBusService {
    pub fn new() -> Self {
        let all_emitter = Arc::new(Emitter::with_debug_name(Some("*")));
        let subscriptions = DisposableStore::new();
        subscriptions.add(all_emitter.clone() as DisposableRef);
        Self {
            inner: Arc::new(EventBusInner {
                all_emitter,
                per_type: Mutex::new(IndexMap::new()),
                per_agent: Mutex::new(IndexMap::new()),
                agents: Mutex::new(HashMap::new()),
                sources: Mutex::new(HashMap::new()),
                disposed_bus: AtomicBool::new(false),
                subscriptions,
            }),
        }
    }

    pub fn dispose(&self) {
        if self.inner.disposed_bus.swap(true, Ordering::SeqCst) {
            return;
        }
        let channels = self
            .inner
            .per_agent
            .lock()
            .drain(..)
            .map(|(_, channel)| channel)
            .collect::<Vec<_>>();
        for channel in channels {
            channel.dispose();
        }
        self.inner.subscriptions.dispose();
    }

    pub fn listener_counts(&self) -> EventBusListenerCounts {
        let per_type = self
            .inner
            .per_type
            .lock()
            .iter()
            .map(|(type_, emitter)| (type_.clone(), emitter.listener_count()))
            .collect();
        let per_agent = self
            .inner
            .per_agent
            .lock()
            .iter()
            .map(|(agent_id, channel)| (agent_id.clone(), channel.all.listener_count()))
            .collect();
        EventBusListenerCounts {
            all: self.inner.all_emitter.listener_count(),
            per_type,
            per_agent,
        }
    }

    fn is_active(&self, agent: &Arc<AgentContext>) -> bool {
        self.inner
            .agents
            .lock()
            .get(&agent.agent_id)
            .is_some_and(|current| Arc::ptr_eq(current, agent))
    }

    fn assert_active(&self, agent: &Arc<AgentContext>) {
        if !self.is_active(agent) {
            panic!(
                "Agent {}:{} is not the active lifecycle context",
                agent.agent_id, agent.generation
            );
        }
    }

    fn channel_for(&self, agent_id: &str) -> Arc<AgentChannel> {
        let mut channels = self.inner.per_agent.lock();
        if let Some(channel) = channels.get(agent_id) {
            return channel.clone();
        }
        let channel = Arc::new(AgentChannel::new(agent_id));
        channels.insert(agent_id.to_string(), channel.clone());
        channel
    }

    fn source(&self, event: &Event2) -> Option<Arc<AgentContext>> {
        self.inner.sources.lock().get(&event_key(event)).cloned()
    }

    fn matches_agent(&self, agent: &Arc<AgentContext>, event: &Event2) -> bool {
        if event_is_agent_domain(event) {
            return self.is_active(agent) && event_agent_id(event) == Some(agent.agent_id.as_str());
        }
        self.source(event)
            .is_some_and(|source| Arc::ptr_eq(&source, agent))
    }

    fn publish_event(&self, event: Event2, agent: Option<Arc<AgentContext>>) {
        if event_is_agent_domain(&event)
            && (agent.is_none()
                || !self.is_active(agent.as_ref().unwrap())
                || event_agent_id(&event)
                    != agent.as_ref().map(|agent| agent.agent_id.as_str()))
        {
            panic!("Agent event '{}' has no active lifecycle context", event.type_);
        }
        if let Some(agent) = &agent {
            self.inner
                .sources
                .lock()
                .insert(event_key(&event), agent.clone());
        }
        self.inner.all_emitter.fire(event.clone());
        let channel = agent
            .as_ref()
            .filter(|agent| self.is_active(agent))
            .and_then(|agent| self.inner.per_agent.lock().get(&agent.agent_id).cloned());
        if let Some(channel) = channel {
            channel.all.fire(event.clone());
        }
        let emitter = self.inner.per_type.lock().get(&event.type_).cloned();
        if let Some(emitter) = emitter {
            emitter.fire(event);
        }
    }

    fn subscribe_all(&self, handler: Event2Handler) -> Box<dyn IDisposable> {
        Box::new(self.inner.all_emitter.event().subscribe_arc(handler))
    }

    fn subscribe_type_inner(
        &self,
        type_: &str,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        let emitter = {
            let mut per_type = self.inner.per_type.lock();
            if let Some(emitter) = per_type.get(type_) {
                emitter.clone()
            } else {
                let emitter = Arc::new(Emitter::with_debug_name(Some(type_)));
                self.inner
                    .subscriptions
                    .add(emitter.clone() as DisposableRef);
                per_type.insert(type_.to_string(), emitter.clone());
                emitter
            }
        };
        Box::new(emitter.event().subscribe_arc(handler))
    }
}

impl Default for EventBusService {
    fn default() -> Self {
        Self::new()
    }
}

impl IEventBus for EventBusService {
    fn publish(&self, event: Event2, agent: Option<Arc<AgentContext>>) {
        self.publish_event(event, agent);
    }

    fn subscribe(&self, handler: Event2Handler) -> Box<dyn IDisposable> {
        self.subscribe_all(handler)
    }

    fn subscribe_class(
        &self,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        note_event_class(&cls);
        self.subscribe_type_inner(&cls.type_, handler)
    }

    fn subscribe_type(&self, type_: &str, handler: Event2Handler) -> Box<dyn IDisposable> {
        self.subscribe_type_inner(type_, handler)
    }
}

impl ISessionEventBus for EventBusService {
    fn activate_agent(&self, agent: Arc<AgentContext>) {
        let previous = self.inner.agents.lock().get(&agent.agent_id).cloned();
        if previous
            .as_ref()
            .is_some_and(|previous| !Arc::ptr_eq(previous, &agent))
        {
            let channel = self.inner.per_agent.lock().shift_remove(&agent.agent_id);
            if let Some(channel) = channel {
                channel.dispose();
            }
        }
        self.inner
            .agents
            .lock()
            .insert(agent.agent_id.clone(), agent);
    }

    fn deactivate_agent(&self, agent: Arc<AgentContext>) {
        if !self.is_active(&agent) {
            return;
        }
        self.inner.agents.lock().remove(&agent.agent_id);
        let channel = self.inner.per_agent.lock().shift_remove(&agent.agent_id);
        if let Some(channel) = channel {
            channel.dispose();
        }
    }

    fn is_agent_active(&self, agent: &Arc<AgentContext>) -> bool {
        self.is_active(agent)
    }

    fn source_of(&self, event: &Event2) -> Option<Arc<AgentContext>> {
        self.source(event)
    }

    fn subscribe_agent(
        &self,
        agent: Arc<AgentContext>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        if self.inner.disposed_bus.load(Ordering::SeqCst) {
            return Box::new(Subscription::none());
        }
        self.assert_active(&agent);
        Box::new(
            self.channel_for(&agent.agent_id)
                .all
                .event()
                .subscribe_arc(handler),
        )
    }

    fn subscribe_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        if self.inner.disposed_bus.load(Ordering::SeqCst) {
            return Box::new(Subscription::none());
        }
        self.assert_active(&agent);
        let this = self.clone();
        self.subscribe_type_inner(
            type_,
            Arc::new(move |event| {
                if this.matches_agent(&agent, &event) {
                    handler(event);
                }
            }),
        )
    }

    fn on_agent_class(
        &self,
        agent: Arc<AgentContext>,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        note_event_class(&cls);
        self.on_agent_type(agent, &cls.type_, handler)
    }

    fn on_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        self.assert_active(&agent);
        let this = self.clone();
        self.subscribe_type_inner(
            type_,
            Arc::new(move |event| {
                if this.is_active(&agent)
                    && event_agent_id(&event) == Some(agent.agent_id.as_str())
                {
                    handler(event);
                }
            }),
        )
    }
}

#[derive(Clone)]
pub struct AgentEventBusView {
    bus: Option<Arc<dyn ISessionEventBus>>,
    agent: Arc<AgentContext>,
}

impl AgentEventBusView {
    pub fn new(
        bus: Option<Arc<dyn ISessionEventBus>>,
        scope: Arc<dyn IAgentScopeContext>,
    ) -> Self {
        Self {
            bus,
            agent: scope.agent_context(),
        }
    }

    pub fn activate_agent(&self, agent: Arc<AgentContext>) {
        self.bus().activate_agent(agent);
    }

    pub fn deactivate_agent(&self, agent: Arc<AgentContext>) {
        self.bus().deactivate_agent(agent);
    }

    pub fn on_agent_class(
        &self,
        agent: Arc<AgentContext>,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        self.assert_own(&agent);
        self.bus().on_agent_class(agent, cls, handler)
    }

    pub fn on_agent_type(
        &self,
        agent: Arc<AgentContext>,
        type_: &str,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        self.assert_own(&agent);
        self.bus().on_agent_type(agent, type_, handler)
    }

    fn bus(&self) -> &Arc<dyn ISessionEventBus> {
        self.bus.as_ref().expect("agent event bus view has no session bus")
    }

    fn assert_own(&self, agent: &Arc<AgentContext>) {
        if !Arc::ptr_eq(agent, &self.agent) {
            panic!("Agent event bus view received a foreign context");
        }
    }

    fn matches(&self, event: &Event2) -> bool {
        if event_is_agent_domain(event) {
            return self.bus().is_agent_active(&self.agent)
                && event_agent_id(event) == Some(self.agent.agent_id.as_str());
        }
        self.bus()
            .source_of(event)
            .is_some_and(|source| Arc::ptr_eq(&source, &self.agent))
    }
}

impl IEventBus for AgentEventBusView {
    fn publish(&self, event: Event2, agent: Option<Arc<AgentContext>>) {
        let agent = agent.unwrap_or_else(|| self.agent.clone());
        self.assert_own(&agent);
        self.bus().publish(event, Some(self.agent.clone()));
    }

    fn subscribe(&self, handler: Event2Handler) -> Box<dyn IDisposable> {
        let Some(bus) = &self.bus else {
            return Box::new(Subscription::none());
        };
        bus.subscribe_agent(self.agent.clone(), handler)
    }

    fn subscribe_class(
        &self,
        cls: Arc<Event2Class>,
        handler: Event2Handler,
    ) -> Box<dyn IDisposable> {
        if self.bus.is_none() {
            return Box::new(Subscription::none());
        }
        let this = self.clone();
        self.bus().subscribe_class(
            cls,
            Arc::new(move |event| {
                if this.matches(&event) {
                    handler(event);
                }
            }),
        )
    }

    fn subscribe_type(&self, type_: &str, handler: Event2Handler) -> Box<dyn IDisposable> {
        if self.bus.is_none() {
            return Box::new(Subscription::none());
        }
        let this = self.clone();
        self.bus().subscribe_type(
            type_,
            Arc::new(move |event| {
                if this.matches(&event) {
                    handler(event);
                }
            }),
        )
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("event bus dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static EVENT_BUS_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let session_ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn ISessionEventBus> = Arc::new(EventBusService::new());
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::Session.as_str().to_string(),
        I_SESSION_EVENT_BUS.clone(),
        SyncDescriptor::new(session_ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "event",
    )
    .map_err(|error| error.to_string())?;
    let view_ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IEventBus> = Arc::new(AgentEventBusView::new(
            Some(dependency::<dyn ISessionEventBus>(&args, 0)),
            dependency::<dyn IAgentScopeContext>(&args, 1),
        ));
        Arc::new(service) as DynamicValue
    });
    let view_descriptor = SyncDescriptor::new(view_ctor, Vec::new());
    register_descriptor_dependencies(
        &view_descriptor,
        vec![
            ServiceDependency {
                id: I_SESSION_EVENT_BUS.clone(),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: create_decorator("agentScopeContext"),
                index: 1,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::Agent.as_str().to_string(),
        I_EVENT_BUS.clone(),
        view_descriptor,
        ScopeActivation::OnDemand,
        "eventView",
    )
    .map_err(|error| error.to_string())?;
    Ok(())
});

pub fn ensure_event_bus_services_registered() -> Result<(), String> {
    Lazy::force(&EVENT_BUS_REGISTRATION).clone()
}
