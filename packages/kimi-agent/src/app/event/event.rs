use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::event::Event;

use super::event2::Event2;

pub type EventPublishListener = Arc<dyn Fn(Event2) + Send + Sync>;

pub trait IEventService: Send + Sync {
    fn on_did_publish(&self) -> Event<Event2>;
    fn publish(&self, event: Event2);
    fn subscribe(&self, handler: EventPublishListener) -> DisposableRef;
}

pub static I_EVENT_SERVICE: Lazy<ServiceIdentifier<Arc<dyn IEventService>>> =
    Lazy::new(|| create_decorator("eventService"));
