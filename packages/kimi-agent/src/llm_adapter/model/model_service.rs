use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use indexmap::IndexMap;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::llm_adapter::model::model::{
    DefaultModelChangedEvent, IModelService, ModelRecord, ModelsChangedEvent, ModelsSection,
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
struct ModelState {
    models: ModelsSection,
    default_model: Option<String>,
    hydrated: bool,
}

#[derive(Default)]
pub struct ModelService {
    state: Mutex<ModelState>,
    ready_notify: Notify,
    on_models: Arc<ChangeListeners<ModelsChangedEvent>>,
    on_default: Arc<ChangeListeners<DefaultModelChangedEvent>>,
}

impl ModelService {
    pub fn new() -> Self {
        Self::default()
    }

    fn apply_records(&self, next: ModelsSection) {
        let diff = {
            let state = self.state.lock();
            diff_records(&state.models, &next)
        };
        if is_empty_diff(&diff) {
            return;
        }
        self.state.lock().models = next;
        self.on_models.fire(ModelsChangedEvent {
            added: diff.added,
            removed: diff.removed,
            changed: diff.changed,
        });
    }

    fn apply_default_model(&self, id: Option<String>) {
        if self.state.lock().default_model == id {
            return;
        }
        self.state.lock().default_model = id.clone();
        self.on_default.fire(DefaultModelChangedEvent { id });
    }
}

impl IModelService for ModelService {
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

    fn on_did_change_models(
        &self,
        listener: Arc<dyn Fn(ModelsChangedEvent) + Send + Sync>,
    ) -> DisposableRef {
        self.on_models.add(listener)
    }

    fn on_did_change_default_model(
        &self,
        listener: Arc<dyn Fn(DefaultModelChangedEvent) + Send + Sync>,
    ) -> DisposableRef {
        self.on_default.add(listener)
    }

    fn get(&self, id: &str) -> Option<ModelRecord> {
        self.state.lock().models.get(id).cloned()
    }

    fn list(&self) -> ModelsSection {
        self.state.lock().models.clone()
    }

    fn get_default_model(&self) -> Option<String> {
        self.state.lock().default_model.clone()
    }

    fn set(&self, id: &str, model: ModelRecord) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            if self.get(id).as_ref() == Some(&model) {
                return Ok(());
            }
            let mut models = self.list();
            models.insert(id.to_string(), model);
            self.apply_records(models);
            Ok(())
        })
    }

    fn delete(&self, id: &str) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            if !self.state.lock().models.contains_key(id) {
                return Ok(());
            }
            let mut models = self.list();
            models.shift_remove(id);
            self.apply_records(models);
            Ok(())
        })
    }

    fn load_all(&self, models: ModelsSection, default_model: Option<String>) {
        self.apply_records(models);
        self.apply_default_model(default_model);
        let mut state = self.state.lock();
        if !state.hydrated {
            state.hydrated = true;
            drop(state);
            self.ready_notify.notify_waiters();
        }
    }

    fn replace_all(&self, models: ModelsSection) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            self.apply_records(models);
            Ok(())
        })
    }

    fn set_default_model(
        &self,
        id: Option<String>,
    ) -> BoxFuture<'_, Result<(), WireAsyncError>> {
        Box::pin(async move {
            self.ready().await?;
            self.apply_default_model(id);
            Ok(())
        })
    }
}
