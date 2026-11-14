use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::ScopeActivation;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable};
use crate::_base::di::scope::register_scoped_service;
use crate::_base::event::{Emitter, Event};
use crate::app::scopes::LifecycleScope;

use super::event::{EventPublishListener, I_EVENT_SERVICE, IEventService};
use super::event2::Event2;

pub struct EventService {
    emitter: Arc<Emitter<Event2>>,
    subscriptions: DisposableStore,
}

impl EventService {
    pub fn new() -> Self {
        let emitter = Arc::new(Emitter::with_debug_name(Some("publish")));
        let subscriptions = DisposableStore::new();
        subscriptions.add(emitter.clone() as DisposableRef);
        Self {
            emitter,
            subscriptions,
        }
    }

    pub fn listener_count(&self) -> usize {
        self.emitter.listener_count()
    }

    pub fn dispose(&self) {
        self.subscriptions.dispose();
    }
}

impl Default for EventService {
    fn default() -> Self {
        Self::new()
    }
}

impl IEventService for EventService {
    fn on_did_publish(&self) -> Event<Event2> {
        self.emitter.event()
    }

    fn publish(&self, event: Event2) {
        self.emitter.fire(event);
    }

    fn subscribe(&self, handler: EventPublishListener) -> Box<dyn IDisposable> {
        Box::new(self.emitter.event().subscribe_arc(handler))
    }
}

static EVENT_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|_| {
        let service: Arc<dyn IEventService> = Arc::new(EventService::new());
        Arc::new(service) as DynamicValue
    });
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_EVENT_SERVICE.clone(),
        SyncDescriptor::new(ctor, Vec::new()),
        ScopeActivation::OnScopeCreated,
        "event",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_event_service_registered() -> Result<(), String> {
    Lazy::force(&EVENT_SERVICE_REGISTRATION).clone()
}
