use std::sync::Arc;

use parking_lot::Mutex;

use super::descriptors::DynamicValue;
use super::fiber::{
    BufferedOp, ConstructionFrame, Fiber, FiberEvent, FiberHandle, FiberHandler, FiberProtocolError,
    FiberRuntime, FiberState, ProvideRequest, ServiceClassRecipe, UnitInternals,
    current_construction,
};
use super::instantiation::{LiveRef, ServiceIdentifier};
use super::lifecycle::{Disposable, IDisposable};
use crate::_base::lifecycle::disposer::EffectBody;
use crate::_base::lifecycle::ledger::Ledger;

pub struct Service {
    store: Disposable,
    unit_buffer: Mutex<Option<Vec<BufferedOp>>>,
    unit_runtime: Mutex<Option<Arc<FiberRuntime>>>,
    recipe: ServiceClassRecipe,
    pub name: String,
    pub config: Option<DynamicValue>,
}

impl Service {
    pub fn new(recipe: ServiceClassRecipe) -> Self {
        let frame = current_construction();
        let constructing = frame
            .as_ref()
            .is_some_and(|frame| frame.ctor == recipe.ctor_id());
        let config = if constructing {
            frame.and_then(|frame| frame.config)
        } else {
            None
        };
        let name = recipe
            .statics
            .name
            .clone()
            .unwrap_or_else(|| "anonymous".to_string());
        Self {
            store: Disposable::new(),
            unit_buffer: Mutex::new(if constructing { Some(Vec::new()) } else { None }),
            unit_runtime: Mutex::new(None),
            recipe,
            name,
            config,
        }
    }

    pub fn frame_matches(frame: &ConstructionFrame, recipe: &ServiceClassRecipe) -> bool {
        frame.ctor == recipe.ctor_id()
    }

    fn runtime(&self) -> Result<Arc<FiberRuntime>, String> {
        if let Some(runtime) = self.unit_runtime.lock().clone() {
            return Ok(runtime);
        }
        if self.unit_buffer.lock().is_some() {
            return Err(FiberProtocolError(format!(
                "unit '{}': get/ref/provide reads are not available during construction — declare dependencies as constructor parameters (构造期只写不读)",
                self.name
            ))
            .to_string());
        }
        Err(FiberProtocolError(format!(
            "unit '{}': no unit runtime is bound — a Service created by manual `new` has no capabilities; construct it through the container",
            self.name
        ))
        .to_string())
    }

    fn pending_name(&self, request: &ProvideRequest) -> String {
        match request {
            ProvideRequest::Anonymous { recipe, .. } => recipe
                .statics()
                .name
                .clone()
                .unwrap_or_else(|| self.name.clone()),
            ProvideRequest::TokenClass { recipe, .. } => recipe
                .statics
                .name
                .clone()
                .unwrap_or_else(|| self.name.clone()),
            _ => self.name.clone(),
        }
    }
}

impl Fiber for Service {
    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> FiberState {
        FiberState::Active
    }

    fn config(&self) -> Option<DynamicValue> {
        self.config.clone()
    }

    fn provide(&self, request: ProvideRequest) -> Result<Arc<dyn FiberHandle>, String> {
        if self.unit_buffer.lock().is_some() {
            let pending = Arc::new(super::fiber::PendingFiberHandle::new(
                self.pending_name(&request),
            ));
            self.unit_buffer.lock().as_mut().unwrap().push(Box::new({
                let pending = pending.clone();
                move |runtime| {
                    let real = runtime
                        .provide(request)
                        .unwrap_or_else(|error| panic!("{error}"));
                    pending.attach(real);
                }
            }));
            return Ok(pending);
        }
        self.runtime()?.provide(request)
    }

    fn effect(&self, body: EffectBody, label: Option<&str>) -> Result<Arc<dyn FiberHandle>, String> {
        let pending_label = label
            .map(str::to_owned)
            .unwrap_or_else(|| format!("effect:{}", self.name));
        if self.unit_buffer.lock().is_some() {
            let pending = Arc::new(super::fiber::PendingFiberHandle::new(
                pending_label.clone(),
            ));
            let label = label.map(str::to_owned);
            self.unit_buffer.lock().as_mut().unwrap().push(Box::new({
                let pending = pending.clone();
                move |runtime| {
                    let real = runtime
                        .effect(body, label.as_deref())
                        .unwrap_or_else(|error| panic!("{error}"));
                    pending.attach(real);
                }
            }));
            return Ok(pending);
        }
        self.runtime()?.effect(body, label)
    }

    fn on(&self, event: FiberEvent, handler: FiberHandler) -> Result<Arc<dyn FiberHandle>, String> {
        let label = match &event {
            FiberEvent::Named(event) => format!("on:{event}"),
            FiberEvent::Emitter(_) => "on:emitter".to_string(),
        };
        if self.unit_buffer.lock().is_some() {
            let pending = Arc::new(super::fiber::PendingFiberHandle::new(label));
            self.unit_buffer.lock().as_mut().unwrap().push(Box::new({
                let pending = pending.clone();
                move |runtime| {
                    let real = runtime
                        .on(event, handler)
                        .unwrap_or_else(|error| panic!("{error}"));
                    pending.attach(real);
                }
            }));
            return Ok(pending);
        }
        self.runtime()?.on(event, handler)
    }

    fn get(&self, id: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String> {
        self.runtime()?.get(id)
    }

    fn reference(&self, id: &ServiceIdentifier<DynamicValue>) -> LiveRef<DynamicValue> {
        self.runtime()
            .unwrap_or_else(|error| panic!("{error}"))
            .reference(id)
    }
}

impl UnitInternals for Service {
    fn unit_book(&self) -> Ledger {
        self.store.store.ledger()
    }

    fn take_unit_buffer(&self) -> Option<Vec<BufferedOp>> {
        self.unit_buffer.lock().take()
    }

    fn set_unit_runtime(&self, runtime: Arc<FiberRuntime>) {
        *self.unit_runtime.lock() = Some(runtime);
    }

    fn class_recipe(&self) -> ServiceClassRecipe {
        self.recipe.clone()
    }

    fn declared_dependencies(&self) -> Vec<ServiceIdentifier<DynamicValue>> {
        self.recipe.statics.inject.clone()
    }
}

impl IDisposable for Service {
    fn dispose(&self) {
        self.store.dispose();
    }
}
