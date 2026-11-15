use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::fiber::{FiberHandler, FiberHost, set_fiber_event_resolver};
use crate::_base::di::instantiation::refine_service_decorator;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};

use super::event2::Event2;
use super::event_bus::{I_EVENT_BUS, IEventBus};

fn resolve(
    host: Arc<dyn FiberHost>,
    event: String,
    handler: FiberHandler,
) -> Result<DisposableRef, String> {
    let event_bus_id = refine_service_decorator(&I_EVENT_BUS);
    let bus_ref = host.live_ref(&event_bus_id);
    let subscription = Arc::new(Mutex::new(None::<DisposableRef>));
    let attach = {
        let host = host.clone();
        let subscription = subscription.clone();
        Arc::new(move || {
            if subscription.lock().is_some() {
                return;
            }
            let Some(bus) = host.live_ref(&event_bus_id).current else {
                return;
            };
            let bus = Arc::downcast::<Arc<dyn IEventBus>>(bus)
                .unwrap_or_else(|_| panic!("fiber event bus has an incompatible type"))
                .as_ref()
                .clone();
            let handler = handler.clone();
            let next = bus.subscribe_type(
                &event,
                Arc::new(move |event: Event2| handler(Arc::new(event) as DynamicValue)),
            );
            *subscription.lock() = Some(next);
        })
    };
    attach();
    let on_change = bus_ref.on_did_change.subscribe({
        let attach = attach.clone();
        move |_| attach()
    });
    Ok(to_disposable(move || {
        on_change.dispose();
        if let Some(subscription) = subscription.lock().take() {
            subscription.dispose();
        }
    }))
}

static FIBER_EVENT_RESOLVER_REGISTRATION: LazyLock<()> = LazyLock::new(|| {
    set_fiber_event_resolver(Some(Arc::new(resolve)));
});

pub fn register_fiber_event_resolver() {
    LazyLock::force(&FIBER_EVENT_RESOLVER_REGISTRATION);
}
