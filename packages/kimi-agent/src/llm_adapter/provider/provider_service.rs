use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::llm_adapter::provider::provider::{
    DefaultProviderChangedEvent, IProviderService, ProviderConfig, ProvidersChangedEvent,
    ProvidersSection,
};
use crate::llm_adapter::record_diff::{diff_records, is_empty_diff};
use crate::wire::record::WireAsyncError;

struct ChangeListeners<T> {
    next_id: AtomicU64,
    listeners: Mutex<IndexMap<u64, Arc<dyn Fn(T) + Send + Sync>>>,
}

impl<T> Default for ChangeListeners<T> {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            listeners: Mutex::new(IndexMap::new()),
        }
    }
}

impl<T> ChangeListeners<T> {
    fn add(self: &Arc<Self>, listener: Arc<dyn Fn(T) + Send + Sync>) -> DisposableRef {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        Arc::new(ChangeSubscription {
            owner: self.clone(),
            id,
        })
    }

    fn fire(&self, event: T)
    where
        T: Clone,
    {
        let listeners = self
            .listeners
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(event.clone());
        }
    }
}

struct ChangeSubscription<T> {
    owner: Arc<ChangeListeners<T>>,
    id: u64,
}

impl<T> IDisposable for ChangeSubscription<T> {
    fn dispose(&self) {
        self.owner.listeners.lock().shift_remove(&self.id);
    }
}

#[derive(Default)]
struct ProviderState {
    providers: ProvidersSection,
    default_provider: Option<String>,
    hydrated: bool,
}

#[derive(Default)]
pub struct ProviderService {
    state: Mutex<ProviderState>,
    ready_notify: Notify,
    on_providers: Arc<ChangeListeners<ProvidersChangedEvent>>,
    on_default: Arc<ChangeListeners<DefaultProviderChangedEvent>>,
}

impl ProviderService {
    pub fn new() -> Self {
        Self::default()
    }

    fn apply_records(&self, next: ProvidersSection) {
        let diff = {
            let state = self.state.lock();
            diff_records(&state.providers, &next)
        };
        if is_empty_diff(&diff) {
            return;
        }
        self.state.lock().providers = next;
        self.on_providers.fire(ProvidersChangedEvent {
            added: diff.added,
            removed: diff.removed,
            changed: diff.changed,
        });
    }

    fn apply_default_provider(&self, id: Option<String>) {
        if self.state.lock().default_provider == id {
            return;
        }
        self.state.lock().default_provider = id.clone();
        self.on_default.fire(DefaultProviderChangedEvent { id });
    }
}

impl IProviderService for ProviderService {
    fn ready(&self) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async {
            loop {
                let notified = self.ready_notify.notified();
                if self.state.lock().hydrated {
                    return Ok(());
                }
                notified.await;
            }
        })
    }

    fn on_did_change_providers(
        &self,
        listener: Arc<dyn Fn(ProvidersChangedEvent) + Send + Sync>,
    ) -> DisposableRef {
        self.on_providers.add(listener)
    }

    fn on_did_change_default_provider(
        &self,
        listener: Arc<dyn Fn(DefaultProviderChangedEvent) + Send + Sync>,
    ) -> DisposableRef {
        self.on_default.add(listener)
    }

    fn get(&self, name: &str) -> Option<ProviderConfig> {
        self.state.lock().providers.get(name).cloned()
    }

    fn list(&self) -> ProvidersSection {
        self.state.lock().providers.clone()
    }

    fn get_default_provider(&self) -> Option<String> {
        self.state.lock().default_provider.clone()
    }

    fn set(
        &self,
        name: &str,
        config: ProviderConfig,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            if self.get(name).as_ref() == Some(&config) {
                return Ok(());
            }
            let mut providers = self.list();
            providers.insert(name.to_string(), config);
            self.apply_records(providers);
            Ok(())
        })
    }

    fn delete(&self, name: &str) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            if !self.state.lock().providers.contains_key(name) {
                return Ok(());
            }
            let mut providers = self.list();
            providers.shift_remove(name);
            self.apply_records(providers);
            if self.get_default_provider().as_deref() == Some(name) {
                self.apply_default_provider(None);
            }
            Ok(())
        })
    }

    fn load_all(&self, providers: ProvidersSection, default_provider: Option<String>) {
        self.apply_records(providers);
        self.apply_default_provider(default_provider);
        let mut state = self.state.lock();
        if !state.hydrated {
            state.hydrated = true;
            drop(state);
            self.ready_notify.notify_waiters();
        }
    }

    fn replace_all(
        &self,
        providers: ProvidersSection,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            self.apply_records(providers);
            Ok(())
        })
    }

    fn set_default_provider(
        &self,
        id: Option<String>,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            self.apply_default_provider(id);
            Ok(())
        })
    }
}
