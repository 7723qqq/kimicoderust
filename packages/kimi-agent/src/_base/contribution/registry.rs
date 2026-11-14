use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable, to_disposable};
use crate::_base::event::{Emitter, Event};

pub struct ContributionRegistration<T> {
    pub source_id: String,
    pub priority: f64,
    pub contribution: T,
}

#[derive(Clone, Default)]
pub struct RegisterContributionOptions {
    pub priority: Option<f64>,
}

pub struct ContributionRegistry<T> {
    registrations: Mutex<IndexMap<String, Arc<ContributionRegistration<T>>>>,
    on_did_change_emitter: Arc<Emitter<String>>,
    pub on_did_change: Event<String>,
    store: DisposableStore,
}

impl<T> ContributionRegistry<T>
where
    T: Send + Sync + 'static,
{
    pub fn new() -> Arc<Self> {
        let emitter = Arc::new(Emitter::new());
        let store = DisposableStore::new();
        let close_emitter = emitter.clone();
        store.add(to_disposable(move || close_emitter.dispose()));
        Arc::new(Self {
            registrations: Mutex::new(IndexMap::new()),
            on_did_change: emitter.event(),
            on_did_change_emitter: emitter,
            store,
        })
    }

    pub fn register(
        self: &Arc<Self>,
        source_id: impl Into<String>,
        contribution: T,
        options: Option<RegisterContributionOptions>,
    ) -> DisposableRef {
        let source_id = source_id.into();
        let registration = Arc::new(ContributionRegistration {
            source_id: source_id.clone(),
            priority: options.and_then(|options| options.priority).unwrap_or(0.0),
            contribution,
        });
        self.registrations
            .lock()
            .insert(source_id.clone(), registration.clone());
        self.on_did_change_emitter.fire(source_id.clone());
        let registry = self.clone();
        let active = AtomicBool::new(false);
        to_disposable(move || {
            if active.swap(true, Ordering::SeqCst) {
                return;
            }
            let removed = {
                let mut registrations = registry.registrations.lock();
                if registrations
                    .get(&source_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &registration))
                {
                    registrations.shift_remove(&source_id);
                    true
                } else {
                    false
                }
            };
            if removed {
                registry.on_did_change_emitter.fire(source_id);
            }
        })
    }

    pub fn unregister(&self, source_id: &str) {
        let removed = self.registrations.lock().shift_remove(source_id).is_some();
        if removed {
            self.on_did_change_emitter.fire(source_id.to_string());
        }
    }

    pub fn entries(&self) -> Vec<Arc<ContributionRegistration<T>>> {
        self.registrations.lock().values().cloned().collect()
    }

    pub fn get(&self, source_id: &str) -> Option<Arc<ContributionRegistration<T>>> {
        self.registrations.lock().get(source_id).cloned()
    }

    pub fn dispose(&self) {
        self.store.dispose();
    }
}

impl<T> IDisposable for ContributionRegistry<T>
where
    T: Send + Sync + 'static,
{
    fn dispose(&self) {
        ContributionRegistry::dispose(self);
    }
}
