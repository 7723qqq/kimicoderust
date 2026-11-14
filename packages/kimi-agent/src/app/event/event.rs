use std::sync::Arc;

use once_cell::sync::Lazy;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::Event;

use super::event2::Event2;

pub type EventPublishListener = Arc<dyn Fn(Event2) + Send + Sync>;

pub trait IEventService: Send + Sync {
    fn on_did_publish(&self) -> Event<Event2>;
    fn publish(&self, event: Event2);
    fn subscribe(&self, handler: EventPublishListener) -> Box<dyn IDisposable>;
}

pub static I_EVENT_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("eventService"));
