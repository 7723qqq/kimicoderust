use std::io::{Error, ErrorKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::collection::CollectionView;
use crate::_base::di::descriptors::DynamicValue;
use crate::_base::di::fiber::{
    Fiber, FiberHandle, FiberProvideOptions, FiberState, ServiceClassRecipe, ServiceRecipe,
};
use crate::_base::di::instantiation::ServiceIdentifier;
use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::app::scopes::LifecycleScope;
use crate::wire::record::WireAsyncError;

use super::feature_manager::{IFeatureManager, ManagedUnitInfo};
use super::feature_service_contribution::ContributedFeatureService;

struct ManagedUnit {
    handle: Arc<dyn FiberHandle>,
    meta: Map<String, Value>,
}

pub struct FeatureManagerService {
    runtime: Arc<dyn Fiber>,
    contributed_services: CollectionView<ContributedFeatureService>,
    units: Arc<Mutex<IndexMap<String, ManagedUnit>>>,
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn() + Send + Sync>)>>>,
    next_listener_id: AtomicU64,
}

impl FeatureManagerService {
    pub fn new(
        runtime: Arc<dyn Fiber>,
        contributed_services: CollectionView<ContributedFeatureService>,
    ) -> Self {
        Self {
            runtime,
            contributed_services,
            units: Arc::new(Mutex::new(IndexMap::new())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(0),
        }
    }

    pub fn provide_unit(
        &self,
        recipe: ServiceRecipe,
        opts: Option<FiberProvideOptions>,
    ) -> Arc<dyn FiberHandle> {
        let meta = meta_to_values(&recipe.statics().meta);
        let handle = self
            .runtime
            .provide(crate::_base::di::fiber::ProvideRequest::Anonymous {
                recipe,
                options: opts,
            })
            .unwrap_or_else(|error| panic!("{error}"));
        self.track(handle, meta)
    }

    pub fn provide_unit_for_id(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        recipe: ServiceClassRecipe,
        opts: Option<FiberProvideOptions>,
    ) -> Arc<dyn FiberHandle> {
        let meta = meta_to_values(&recipe.statics.meta);
        let handle = self
            .runtime
            .provide(crate::_base::di::fiber::ProvideRequest::TokenClass {
                id,
                recipe,
                options: opts,
            })
            .unwrap_or_else(|error| panic!("{error}"));
        self.track(handle, meta)
    }

    fn track(
        &self,
        handle: Arc<dyn FiberHandle>,
        meta: Map<String, Value>,
    ) -> Arc<dyn FiberHandle> {
        let name = handle.name();
        let previous = self.units.lock().insert(
            name,
            ManagedUnit {
                handle: handle.clone(),
                meta,
            },
        );
        if let Some(previous) = previous {
            if !Arc::ptr_eq(&previous.handle, &handle) {
                tokio::spawn(async move {
                    let _ = previous.handle.dispose().await;
                });
            }
        }
        self.fire_units_changed();
        handle
    }

    pub async fn unprovide_unit(&self, name: &str) -> Result<(), WireAsyncError> {
        let entry = self.units.lock().shift_remove(name);
        let Some(entry) = entry else {
            return Ok(());
        };
        let result = entry
            .handle
            .dispose()
            .await
            .map_err(|error| -> WireAsyncError { Box::new(std::io::Error::other(error)) });
        self.fire_units_changed();
        result
    }

    pub async fn update_unit(
        &self,
        name: &str,
        config: Option<Value>,
    ) -> Result<(), WireAsyncError> {
        let handle = self
            .units
            .lock()
            .get(name)
            .map(|entry| entry.handle.clone())
            .ok_or_else(|| -> WireAsyncError {
                Box::new(Error::new(
                    ErrorKind::NotFound,
                    format!("feature unit '{name}' is not managed by this FeatureManager"),
                ))
            })?;
        handle.update(config.map(|value| Arc::new(value) as crate::_base::di::descriptors::DynamicValue)).await.map_err(|error| -> crate::wire::record::WireAsyncError { Box::new(std::io::Error::other(error)) })?;
        self.fire_units_changed();
        Ok(())
    }

    pub fn units(&self) -> Vec<ManagedUnitInfo> {
        self.units
            .lock()
            .iter()
            .map(|(name, entry)| ManagedUnitInfo {
                name: name.clone(),
                state: entry.handle.state(),
                uid: Some(entry.handle.uid()),
                meta: entry.meta.clone(),
            })
            .collect()
    }

    pub fn contributed_services(&self) -> Vec<ContributedFeatureService> {
        self.contributed_services.items()
    }

    pub fn on_did_change_units(
        &self,
        listener: Arc<dyn Fn() + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    fn fire_units_changed(&self) {
        let listeners = self
            .listeners
            .lock()
            .iter()
            .map(|(_, listener)| listener.clone())
            .collect::<Vec<_>>();
        for listener in listeners {
            listener();
        }
    }
}

#[async_trait]
impl IFeatureManager for FeatureManagerService {
    fn provide_unit(
        &self,
        recipe: ServiceRecipe,
        opts: Option<FiberProvideOptions>,
    ) -> Arc<dyn FiberHandle> {
        FeatureManagerService::provide_unit(self, recipe, opts)
    }

    fn provide_unit_for_id(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        recipe: ServiceClassRecipe,
        opts: Option<FiberProvideOptions>,
    ) -> Arc<dyn FiberHandle> {
        FeatureManagerService::provide_unit_for_id(self, id, recipe, opts)
    }

    async fn unprovide_unit(&self, name: &str) -> Result<(), WireAsyncError> {
        FeatureManagerService::unprovide_unit(self, name).await
    }

    async fn update_unit(&self, name: &str, config: Option<Value>) -> Result<(), WireAsyncError> {
        FeatureManagerService::update_unit(self, name, config).await
    }

    fn units(&self) -> Vec<ManagedUnitInfo> {
        FeatureManagerService::units(self)
    }

    fn contributed_services(&self) -> Vec<ContributedFeatureService> {
        FeatureManagerService::contributed_services(self)
    }

    fn on_did_change_units(&self, listener: Arc<dyn Fn() + Send + Sync>) -> DisposableRef {
        FeatureManagerService::on_did_change_units(self, listener)
    }
}

fn meta_to_values(meta: &HashMap<String, crate::_base::di::descriptors::DynamicValue>) -> Map<String, Value> {
    let mut out = Map::new();
    for (key, value) in meta {
        let json = if let Some(value) = value.downcast_ref::<Value>() {
            Some(value.clone())
        } else if let Some(value) = value.downcast_ref::<String>() {
            Some(Value::String(value.clone()))
        } else if let Some(value) = value.downcast_ref::<u64>() {
            Some(serde_json::json!(value))
        } else if let Some(value) = value.downcast_ref::<bool>() {
            Some(Value::Bool(*value))
        } else {
            None
        };
        if let Some(json) = json {
            out.insert(key.clone(), json);
        }
    }
    out
}
