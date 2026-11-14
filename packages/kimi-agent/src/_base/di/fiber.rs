use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::collection::{
    CollectionOptions, CollectionToken, CollectionViewImpl, collection, is_collection_token,
};
use super::dependency_graph::ObjectId;
use super::descriptors::{DynamicValue, SyncDescriptor};
use super::instantiation::{LiveRef, ScopeActivation, ServiceIdentifier};
use super::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::event::Emitter;
use crate::_base::lifecycle::disposer::{EffectBody, EffectResult};
use crate::_base::lifecycle::ledger::{Ledger, LedgerEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum FiberState {
    Pending = 0,
    Activating = 1,
    Active = 2,
    Unloading = 3,
    Failed = 4,
}

#[derive(Clone)]
pub struct ConfigIssue {
    pub message: String,
}

#[derive(Clone, Default)]
pub struct StandardValidation {
    pub value: Option<DynamicValue>,
    pub has_value: bool,
    pub issues: Vec<ConfigIssue>,
}

pub enum ConfigValidation {
    Sync(StandardValidation),
    Async(BoxFuture<'static, StandardValidation>),
}

pub trait ConfigSchema: Send + Sync {
    fn validate(&self, value: Option<DynamicValue>) -> ConfigValidation;
}

#[derive(Clone, Default)]
pub struct RecipeStatics {
    pub name: Option<String>,
    pub inject: Vec<ServiceIdentifier<DynamicValue>>,
    pub config: Option<Arc<dyn ConfigSchema>>,
    pub meta: HashMap<String, DynamicValue>,
}

#[derive(Clone)]
pub struct ServiceClassRecipe {
    pub statics: RecipeStatics,
    pub descriptor: SyncDescriptor<DynamicValue>,
    pub service: bool,
}

impl ServiceClassRecipe {
    pub fn ctor_id(&self) -> ObjectId {
        Arc::as_ptr(&self.descriptor.ctor) as *const () as ObjectId
    }
}

#[derive(Clone)]
pub struct ServiceFunctionRecipe {
    pub statics: RecipeStatics,
    pub apply: Arc<
        dyn Fn(Arc<FiberRuntime>, Option<DynamicValue>) -> Result<EffectResult, String>
            + Send
            + Sync,
    >,
}

pub trait ServiceObjectRecipe: Send + Sync {
    fn statics(&self) -> &RecipeStatics;
    fn apply(
        &self,
        fiber: Arc<FiberRuntime>,
        config: Option<DynamicValue>,
    ) -> Result<EffectResult, String>;
}

#[derive(Clone)]
pub enum ServiceRecipe {
    Class(ServiceClassRecipe),
    Function(ServiceFunctionRecipe),
    Object(Arc<dyn ServiceObjectRecipe>),
}

impl ServiceRecipe {
    pub fn statics(&self) -> &RecipeStatics {
        match self {
            Self::Class(recipe) => &recipe.statics,
            Self::Function(recipe) => &recipe.statics,
            Self::Object(recipe) => recipe.statics(),
        }
    }
}

#[derive(Clone, Default)]
pub struct FiberProvideOptions {
    pub config: Option<DynamicValue>,
    pub activation: Option<ScopeActivation>,
}

#[derive(Clone)]
pub enum ProvideRequest {
    TokenClass {
        id: ServiceIdentifier<DynamicValue>,
        recipe: ServiceClassRecipe,
        options: Option<FiberProvideOptions>,
    },
    TokenInstance {
        id: ServiceIdentifier<DynamicValue>,
        instance: DynamicValue,
    },
    Anonymous {
        recipe: ServiceRecipe,
        options: Option<FiberProvideOptions>,
    },
    Collection {
        token: CollectionToken<DynamicValue>,
        value: DynamicValue,
    },
}

#[derive(Clone)]
pub enum FiberEvent {
    Named(String),
    Emitter(Emitter<DynamicValue>),
}

pub type FiberHandler = Arc<dyn Fn(DynamicValue) + Send + Sync>;

pub trait Fiber: Send + Sync {
    fn name(&self) -> &str;
    fn state(&self) -> FiberState;
    fn config(&self) -> Option<DynamicValue>;
    fn provide(&self, request: ProvideRequest) -> Result<Arc<dyn FiberHandle>, String>;
    fn effect(&self, body: EffectBody, label: Option<&str>) -> Result<Arc<dyn FiberHandle>, String>;
    fn on(&self, event: FiberEvent, handler: FiberHandler) -> Result<Arc<dyn FiberHandle>, String>;
    fn get(&self, id: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String>;
    fn reference(&self, id: &ServiceIdentifier<DynamicValue>) -> LiveRef<DynamicValue>;
}

pub trait FiberHandle: Send + Sync {
    fn name(&self) -> String;
    fn state(&self) -> FiberState;
    fn uid(&self) -> u64;
    fn update(&self, config: Option<DynamicValue>) -> BoxFuture<'static, Result<(), String>>;
    fn dispose(&self) -> BoxFuture<'static, Result<(), String>>;
    fn when_active(&self) -> BoxFuture<'static, Result<(), String>>;

    fn settled(self: Arc<Self>) -> SettledHandleView
    where
        Self: Sized,
    {
        SettledHandleView { handle: self }
    }
}

#[derive(Clone, Debug)]
pub struct FiberProtocolError(pub String);

impl fmt::Display for FiberProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for FiberProtocolError {}

#[derive(Clone, Debug)]
pub struct ServiceRecipeError(pub String);

impl fmt::Display for ServiceRecipeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ServiceRecipeError {}

#[derive(Clone)]
pub struct ConstructionFrame {
    pub ctor: ObjectId,
    pub config: Option<DynamicValue>,
    pub token: Option<ServiceIdentifier<DynamicValue>>,
    pub host: Arc<dyn FiberHost>,
}

thread_local! {
    static CONSTRUCTION_STACK: RefCell<Vec<ConstructionFrame>> = const { RefCell::new(Vec::new()) };
}

pub fn push_construction_frame(frame: ConstructionFrame) {
    CONSTRUCTION_STACK.with(|stack| stack.borrow_mut().push(frame));
}

pub fn pop_construction_frame() {
    CONSTRUCTION_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
}

pub fn current_construction() -> Option<ConstructionFrame> {
    CONSTRUCTION_STACK.with(|stack| stack.borrow().last().cloned())
}

pub const SERVICE_MARK: &str = "serviceUnit";

pub fn is_service_recipe(recipe: &ServiceClassRecipe) -> bool {
    recipe.service
}

pub fn is_class_recipe(recipe: &ServiceRecipe) -> bool {
    matches!(recipe, ServiceRecipe::Class(_))
}

pub type BufferedOp = Box<dyn FnOnce(Arc<FiberRuntime>) + Send>;

pub trait UnitInternals: IDisposable {
    fn unit_book(&self) -> Ledger;
    fn take_unit_buffer(&self) -> Option<Vec<BufferedOp>>;
    fn set_unit_runtime(&self, runtime: Arc<FiberRuntime>);
    fn class_recipe(&self) -> ServiceClassRecipe;
    fn declared_dependencies(&self) -> Vec<ServiceIdentifier<DynamicValue>>;
}

pub struct ConstructedService {
    pub value: DynamicValue,
    pub disposable: Option<DisposableRef>,
}

pub trait FiberHost: Send + Sync {
    fn mint_uid(&self) -> u64;
    fn provide_token(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
        activation: &str,
        config: Option<DynamicValue>,
    ) -> Result<Arc<dyn TokenProvideCore>, String>;
    fn provide_token_instance(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        instance: DynamicValue,
    ) -> Result<Arc<dyn TokenProvideCore>, String>;
    fn token_state(&self, id: &ServiceIdentifier<DynamicValue>) -> Option<String>;
    fn update_token(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        config: Option<DynamicValue>,
        has_config: bool,
    ) -> BoxFuture<'static, Result<(), String>>;
    fn resolve_token_when_available(
        &self,
        id: ServiceIdentifier<DynamicValue>,
    ) -> BoxFuture<'static, Result<DynamicValue, String>>;
    fn resolve_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
    ) -> Result<DynamicValue, String>;
    fn materialized_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
    ) -> Option<DynamicValue>;
    fn live_ref(&self, id: &ServiceIdentifier<DynamicValue>) -> LiveRef<DynamicValue>;
    fn record_instance_edge(
        &self,
        node: Option<ObjectId>,
        id: &ServiceIdentifier<DynamicValue>,
    );
    fn collection_view(
        &self,
        token: CollectionToken<DynamicValue>,
    ) -> Arc<CollectionViewImpl<DynamicValue>>;
    fn add_collection_record(
        &self,
        token: CollectionToken<DynamicValue>,
        provider_name: String,
        provider_book: Ledger,
        value: DynamicValue,
    ) -> Result<Box<dyn FnOnce() + Send>, String>;
    fn construct_service(
        &self,
        recipe: ServiceClassRecipe,
        config: Option<DynamicValue>,
    ) -> Result<ConstructedService, String>;
}

pub trait TokenProvideCore: Send + Sync {
    fn uid(&self) -> u64;
    fn dispose(&self) -> BoxFuture<'static, Result<(), String>>;
    fn release(&self);
}

pub type FiberEventResolver = Arc<
    dyn Fn(Arc<dyn FiberHost>, String, FiberHandler) -> Result<DisposableRef, String>
        + Send
        + Sync,
>;

static FIBER_EVENT_RESOLVER: Lazy<Mutex<Option<FiberEventResolver>>> =
    Lazy::new(|| Mutex::new(None));

pub fn set_fiber_event_resolver(resolver: Option<FiberEventResolver>) {
    *FIBER_EVENT_RESOLVER.lock() = resolver;
}

pub fn bind_service_unit(
    instance: Arc<dyn UnitInternals>,
    frame: ConstructionFrame,
) -> Result<(), String> {
    let Some(buffer) = instance.take_unit_buffer() else {
        return Ok(());
    };
    let recipe = instance.class_recipe();
    let runtime = Arc::new(FiberRuntime::new(
        frame.host,
        instance.unit_book(),
        recipe_name(recipe.statics()),
        frame.config,
        frame.token,
        instance.declared_dependencies().into_iter().collect(),
        Some(Arc::as_ptr(&instance) as *const () as ObjectId),
    ));
    instance.set_unit_runtime(runtime.clone());
    for operation in buffer {
        if let Err(error) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation(runtime.clone());
        })) {
            instance.dispose();
            return Err(
                error
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| error.downcast_ref::<&str>().map(|value| (*value).to_string()))
                    .unwrap_or_else(|| "unknown unit operation error".to_string()),
            );
        }
    }
    Ok(())
}

fn recipe_name(recipe: &RecipeStatics) -> String {
    recipe.name.clone().unwrap_or_else(|| "anonymous".to_string())
}

fn validate_config(
    schema: Option<&Arc<dyn ConfigSchema>>,
    config: Option<DynamicValue>,
    name: &str,
) -> Result<Option<DynamicValue>, String> {
    let Some(schema) = schema else {
        return Ok(config);
    };
    let out = schema.validate(config.clone());
    let ConfigValidation::Sync(result) = out else {
        return Err(ServiceRecipeError(format!(
            "config schema of unit '{name}' must validate synchronously"
        ))
        .to_string());
    };
    if !result.issues.is_empty() {
        return Err(ServiceRecipeError(format!(
            "invalid config for unit '{name}': {}",
            result
                .issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ))
        .to_string());
    }
    if result.has_value {
        Ok(result.value)
    } else {
        Ok(config)
    }
}

fn map_unit_state(state: Option<&str>) -> FiberState {
    match state {
        Some("Pending") => FiberState::Pending,
        Some("Activating") => FiberState::Activating,
        Some("Unloading") => FiberState::Unloading,
        Some("Failed") => FiberState::Failed,
        _ => FiberState::Active,
    }
}

pub struct FiberRuntime {
    host: Arc<dyn FiberHost>,
    book: Ledger,
    pub name: String,
    pub config: Option<DynamicValue>,
    token: Option<ServiceIdentifier<DynamicValue>>,
    declared: HashSet<ServiceIdentifier<DynamicValue>>,
    edge_node: Option<ObjectId>,
}

impl FiberRuntime {
    pub fn new(
        host: Arc<dyn FiberHost>,
        book: Ledger,
        name: String,
        config: Option<DynamicValue>,
        token: Option<ServiceIdentifier<DynamicValue>>,
        declared: HashSet<ServiceIdentifier<DynamicValue>>,
        edge_node: Option<ObjectId>,
    ) -> Self {
        Self {
            host,
            book,
            name,
            config,
            token,
            declared,
            edge_node,
        }
    }

    fn provide_token(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        recipe: ServiceClassRecipe,
        options: Option<FiberProvideOptions>,
    ) -> Result<Arc<dyn FiberHandle>, String> {
        let name = recipe
            .statics
            .name
            .clone()
            .unwrap_or_else(|| id.to_string());
        let options = options.unwrap_or_default();
        let config = validate_config(
            recipe.statics.config.as_ref(),
            options.config,
            &name,
        )?;
        let activation = if options.activation == Some(ScopeActivation::OnDemand) {
            "ondemand"
        } else {
            "eager"
        };
        let core = self.host.provide_token(
            id.clone(),
            recipe.descriptor.clone(),
            activation,
            config,
        )?;
        let dispose_core = core.clone();
        let entry = self.book.register(
            move |_| dispose_core.dispose(),
            &format!("provide:{id}"),
        );
        let state_core = core.clone();
        let state_host = self.host.clone();
        let state_id = id.clone();
        let update_host = self.host.clone();
        let update_id = id.clone();
        let update_recipe = recipe.clone();
        let update_name = name.clone();
        let active_host = self.host.clone();
        let active_id = id.clone();
        let uid_core = core;
        Ok(Arc::new(BasicFiberHandle {
            name,
            uid: None,
            uid_of: Some(Arc::new(move || uid_core.uid())),
            state: Arc::new(move || {
                map_unit_state(state_host.token_state(&state_id).as_deref())
            }),
            update: Arc::new(move |next| {
                let host = update_host.clone();
                let id = update_id.clone();
                let recipe = update_recipe.clone();
                let name = update_name.clone();
                async move {
                    let has_config = next.is_some();
                    let config = if has_config {
                        validate_config(recipe.statics.config.as_ref(), next, &name)?
                    } else {
                        None
                    };
                    host.update_token(id, config, has_config).await
                }
                .boxed()
            }),
            dispose: Arc::new(move || {
                let entry = entry.clone();
                async move {
                    if let Some(future) = entry.dispose("scope-close") {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(move || {
                let future = active_host.resolve_token_when_available(active_id.clone());
                async move { future.await.map(|_| ()) }.boxed()
            }),
        }))
    }

    fn provide_token_instance(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        instance: DynamicValue,
    ) -> Result<Arc<dyn FiberHandle>, String> {
        let core = self.host.provide_token_instance(id.clone(), instance)?;
        let dispose_core = core.clone();
        let entry = self.book.register(
            move |_| dispose_core.dispose(),
            &format!("provide:{id}"),
        );
        let state_host = self.host.clone();
        let state_id = id.clone();
        let update_host = self.host.clone();
        let update_id = id.clone();
        let active_host = self.host.clone();
        let active_id = id.clone();
        Ok(Arc::new(BasicFiberHandle {
            name: id.to_string(),
            uid: None,
            uid_of: Some(Arc::new(move || core.uid())),
            state: Arc::new(move || {
                map_unit_state(state_host.token_state(&state_id).as_deref())
            }),
            update: Arc::new(move |_| update_host.update_token(update_id.clone(), None, false)),
            dispose: Arc::new(move || {
                let entry = entry.clone();
                async move {
                    if let Some(future) = entry.dispose("scope-close") {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(move || {
                let future = active_host.resolve_token_when_available(active_id.clone());
                async move { future.await.map(|_| ()) }.boxed()
            }),
        }))
    }

    fn provide_anonymous(
        &self,
        recipe: ServiceRecipe,
        options: Option<FiberProvideOptions>,
    ) -> Result<Arc<dyn FiberHandle>, String> {
        match recipe {
            ServiceRecipe::Class(recipe) => self.provide_anonymous_class(recipe, options),
            _ => self.provide_function(recipe, options),
        }
    }

    fn provide_anonymous_class(
        &self,
        recipe: ServiceClassRecipe,
        options: Option<FiberProvideOptions>,
    ) -> Result<Arc<dyn FiberHandle>, String> {
        let name = recipe_name(&recipe.statics);
        let initial_config = validate_config(
            recipe.statics.config.as_ref(),
            options.and_then(|options| options.config),
            &name,
        )?;
        let state = Arc::new(Mutex::new(FiberState::Activating));
        let failure = Arc::new(Mutex::new(None::<String>));
        let entry = Arc::new(Mutex::new(None::<LedgerEntry>));
        let construct = {
            let host = self.host.clone();
            let book = self.book.clone();
            let recipe = recipe.clone();
            let name = name.clone();
            let state = state.clone();
            let entry = entry.clone();
            Arc::new(move |config: Option<DynamicValue>| -> Result<(), String> {
                *state.lock() = FiberState::Activating;
                let constructed = host.construct_service(recipe.clone(), config)?;
                let dispose_state = state.clone();
                let disposable = constructed.disposable;
                let next_entry = book.register(
                    move |_| {
                        *dispose_state.lock() = FiberState::Unloading;
                        if let Some(disposable) = &disposable {
                            disposable.dispose();
                        }
                    },
                    &format!("provide:{name}"),
                );
                *entry.lock() = Some(next_entry);
                *state.lock() = FiberState::Active;
                Ok(())
            })
        };
        if let Err(error) = construct(initial_config.clone()) {
            *state.lock() = FiberState::Failed;
            *failure.lock() = Some(error.clone());
            return Err(error);
        }
        let update_construct = construct.clone();
        let update_config = Arc::new(Mutex::new(initial_config));
        let update_recipe = recipe.clone();
        let update_name = name.clone();
        let update_state = state.clone();
        let update_failure = failure.clone();
        let update_entry = entry.clone();
        let active_failure = failure.clone();
        Ok(Arc::new(BasicFiberHandle {
            name,
            uid: Some(self.host.mint_uid()),
            uid_of: None,
            state: Arc::new(move || *state.lock()),
            update: Arc::new(move |next| {
                let construct = update_construct.clone();
                let config = update_config.clone();
                let recipe = update_recipe.clone();
                let name = update_name.clone();
                let state = update_state.clone();
                let failure = update_failure.clone();
                let entry = update_entry.clone();
                async move {
                    if let Some(current) = entry.lock().take()
                        && let Some(future) = current.dispose("scope-close")
                    {
                        future.await;
                    }
                    let result = (|| {
                        if next.is_some() {
                            *config.lock() = validate_config(
                                recipe.statics.config.as_ref(),
                                next,
                                &name,
                            )?;
                        }
                        construct(config.lock().clone())
                    })();
                    if let Err(error) = result {
                        *state.lock() = FiberState::Failed;
                        *failure.lock() = Some(error.clone());
                        return Err(error);
                    }
                    Ok(())
                }
                .boxed()
            }),
            dispose: Arc::new(move || {
                let entry = entry.clone();
                async move {
                    if let Some(entry) = entry.lock().take()
                        && let Some(future) = entry.dispose("scope-close")
                    {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(move || {
                let failure = active_failure.lock().clone();
                async move {
                    if let Some(error) = failure {
                        Err(error)
                    } else {
                        Ok(())
                    }
                }
                .boxed()
            }),
        }))
    }

    fn provide_function(
        &self,
        recipe: ServiceRecipe,
        options: Option<FiberProvideOptions>,
    ) -> Result<Arc<dyn FiberHandle>, String> {
        let statics = recipe.statics().clone();
        let name = recipe_name(&statics);
        let config = validate_config(
            statics.config.as_ref(),
            options.and_then(|options| options.config),
            &name,
        )?;
        let book = Ledger::new(&format!("unit:{name}"));
        let anchor_book = book.clone();
        let anchor = self.book.register(
            move |reason| anchor_book.teardown(reason),
            &format!("provide:{name}"),
        );
        let facade = Arc::new(FiberRuntime::new(
            self.host.clone(),
            book.clone(),
            name.clone(),
            config.clone(),
            None,
            statics.inject.iter().cloned().collect(),
            self.edge_node,
        ));
        let run = {
            let recipe = recipe.clone();
            let facade = facade.clone();
            let book = book.clone();
            let name = name.clone();
            Arc::new(move || -> Result<(), String> {
                let out = match &recipe {
                    ServiceRecipe::Function(recipe) => {
                        (recipe.apply)(facade.clone(), config.clone())?
                    }
                    ServiceRecipe::Object(recipe) => {
                        recipe.apply(facade.clone(), config.clone())?
                    }
                    ServiceRecipe::Class(_) => unreachable!(),
                };
                let out = Arc::new(Mutex::new(Some(out)));
                book.effect(
                    Arc::new(move || out.lock().take().unwrap_or(EffectResult::None)),
                    &format!("effect:{name}"),
                );
                Ok(())
            })
        };
        if let Err(error) = run() {
            if let Some(future) = book.teardown("unload") {
                tokio::spawn(future);
            }
            anchor.release();
            return Err(error);
        }
        let update_run = run.clone();
        let update_book = book.clone();
        let update_anchor = anchor.clone();
        let state_anchor = anchor.clone();
        Ok(Arc::new(BasicFiberHandle {
            name,
            uid: Some(self.host.mint_uid()),
            uid_of: None,
            state: Arc::new(move || {
                if state_anchor.disposed() {
                    FiberState::Unloading
                } else {
                    FiberState::Active
                }
            }),
            update: Arc::new(move |_| {
                let run = update_run.clone();
                let book = update_book.clone();
                let anchor = update_anchor.clone();
                async move {
                    if let Some(future) = book.clear("unload") {
                        future.await;
                    }
                    if let Err(error) = run() {
                        if let Some(future) = book.teardown("unload") {
                            future.await;
                        }
                        anchor.release();
                        return Err(error);
                    }
                    Ok(())
                }
                .boxed()
            }),
            dispose: Arc::new(move || {
                let anchor = anchor.clone();
                async move {
                    if let Some(future) = anchor.dispose("scope-close") {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(|| async { Ok(()) }.boxed()),
        }))
    }

    fn provide_record(
        &self,
        token: CollectionToken<DynamicValue>,
        value: DynamicValue,
    ) -> Result<Arc<dyn FiberHandle>, String> {
        let remove = self.host.add_collection_record(
            token.clone(),
            self.name.clone(),
            self.book.clone(),
            value,
        )?;
        let remove = Arc::new(Mutex::new(Some(remove)));
        let entry = self.book.register(
            move |_| {
                if let Some(remove) = remove.lock().take() {
                    remove();
                }
            },
            &format!("provide:{}", token.name),
        );
        let name = format!("{}→{}", self.name, token.name);
        let state_entry = entry.clone();
        Ok(Arc::new(BasicFiberHandle {
            name,
            uid: Some(self.host.mint_uid()),
            uid_of: None,
            state: Arc::new(move || {
                if state_entry.disposed() {
                    FiberState::Unloading
                } else {
                    FiberState::Active
                }
            }),
            update: Arc::new(|_| async { Ok(()) }.boxed()),
            dispose: Arc::new(move || {
                let entry = entry.clone();
                async move {
                    if let Some(future) = entry.dispose("scope-close") {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(|| async { Ok(()) }.boxed()),
        }))
    }
}

impl Fiber for FiberRuntime {
    fn name(&self) -> &str {
        &self.name
    }

    fn state(&self) -> FiberState {
        self.token
            .as_ref()
            .map(|token| map_unit_state(self.host.token_state(token).as_deref()))
            .unwrap_or(FiberState::Active)
    }

    fn config(&self) -> Option<DynamicValue> {
        self.config.clone()
    }

    fn provide(&self, request: ProvideRequest) -> Result<Arc<dyn FiberHandle>, String> {
        match request {
            ProvideRequest::Collection { token, value } if is_collection_token(&token) => {
                self.provide_record(token, value)
            }
            ProvideRequest::Collection { .. } => {
                Err(ServiceRecipeError("invalid collection token".to_string()).to_string())
            }
            ProvideRequest::TokenClass {
                id,
                recipe,
                options,
            } => self.provide_token(id, recipe, options),
            ProvideRequest::TokenInstance { id, instance } => {
                self.provide_token_instance(id, instance)
            }
            ProvideRequest::Anonymous { recipe, options } => {
                self.provide_anonymous(recipe, options)
            }
        }
    }

    fn effect(&self, body: EffectBody, label: Option<&str>) -> Result<Arc<dyn FiberHandle>, String> {
        let label = label
            .map(str::to_owned)
            .unwrap_or_else(|| format!("effect:{}", self.name));
        let entry = self.book.effect(body.clone(), &label);
        let update_entry = entry.clone();
        let update_book = self.book.clone();
        let update_label = label.clone();
        let state_entry = entry.clone();
        Ok(Arc::new(BasicFiberHandle {
            name: label,
            uid: self.host.mint_uid(),
            uid_of: None,
            state: Arc::new(move || {
                if state_entry.disposed() {
                    FiberState::Unloading
                } else {
                    FiberState::Active
                }
            }),
            update: Arc::new(move |_| {
                let entry = update_entry.clone();
                let book = update_book.clone();
                let body = body.clone();
                let label = update_label.clone();
                async move {
                    if let Some(future) = entry.dispose("scope-close") {
                        future.await;
                    }
                    if book.is_active() {
                        book.effect(body, &label);
                    }
                    Ok(())
                }
                .boxed()
            }),
            dispose: Arc::new(move || {
                let entry = entry.clone();
                async move {
                    if let Some(future) = entry.dispose("scope-close") {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(|| async { Ok(()) }.boxed()),
        }))
    }

    fn on(&self, event: FiberEvent, handler: FiberHandler) -> Result<Arc<dyn FiberHandle>, String> {
        let (label, subscription) = match event {
            FiberEvent::Named(event) => {
                let resolver = FIBER_EVENT_RESOLVER.lock().clone().ok_or_else(|| {
                    FiberProtocolError(format!(
                        "no event resolver registered — cannot subscribe to '{event}'"
                    ))
                    .to_string()
                })?;
                let subscription = resolver(self.host.clone(), event.clone(), handler)?;
                (format!("on:{event}"), subscription)
            }
            FiberEvent::Emitter(emitter) => {
                let subscription = emitter.event().subscribe(move |value| handler(value));
                (
                    "on:emitter".to_string(),
                    to_disposable(move || subscription.dispose()),
                )
            }
        };
        let entry = self.book.register(
            move |_| {
                subscription.dispose();
            },
            &label,
        );
        let state_entry = entry.clone();
        Ok(Arc::new(BasicFiberHandle {
            name: label,
            uid: self.host.mint_uid(),
            uid_of: None,
            state: Arc::new(move || {
                if state_entry.disposed() {
                    FiberState::Unloading
                } else {
                    FiberState::Active
                }
            }),
            update: Arc::new(|_| async { Ok(()) }.boxed()),
            dispose: Arc::new(move || {
                let entry = entry.clone();
                async move {
                    if let Some(future) = entry.dispose("scope-close") {
                        future.await;
                    }
                    Ok(())
                }
                .boxed()
            }),
            when_active: Arc::new(|| async { Ok(()) }.boxed()),
        }))
    }

    fn get(&self, id: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String> {
        if !self.declared.contains(id) {
            return Err(FiberProtocolError(format!(
                "unit '{}' resolves undeclared dependency '{}' — declare it as a constructor parameter (class recipe) or in the inject static (function recipe)",
                self.name, id
            ))
            .to_string());
        }
        self.host.record_instance_edge(self.edge_node, id);
        self.host.resolve_instance(id)
    }

    fn reference(&self, id: &ServiceIdentifier<DynamicValue>) -> LiveRef<DynamicValue> {
        self.host.live_ref(id)
    }
}

type StateCallback = Arc<dyn Fn() -> FiberState + Send + Sync>;
type UpdateCallback =
    Arc<dyn Fn(Option<DynamicValue>) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;
type DisposeCallback = Arc<dyn Fn() -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

pub struct BasicFiberHandle {
    name: String,
    uid: Option<u64>,
    uid_of: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
    state: StateCallback,
    update: UpdateCallback,
    dispose: DisposeCallback,
    when_active: DisposeCallback,
}

impl FiberHandle for BasicFiberHandle {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn state(&self) -> FiberState {
        (self.state)()
    }

    fn uid(&self) -> u64 {
        if let Some(uid_of) = &self.uid_of {
            uid_of()
        } else {
            self.uid.expect("handle uid is not available")
        }
    }

    fn update(&self, config: Option<DynamicValue>) -> BoxFuture<'static, Result<(), String>> {
        (self.update)(config)
    }

    fn dispose(&self) -> BoxFuture<'static, Result<(), String>> {
        (self.dispose)()
    }

    fn when_active(&self) -> BoxFuture<'static, Result<(), String>> {
        (self.when_active)()
    }
}

pub struct SettledHandleView {
    handle: Arc<dyn FiberHandle>,
}

impl SettledHandleView {
    pub fn name(&self) -> String {
        self.handle.name()
    }

    pub fn state(&self) -> FiberState {
        self.handle.state()
    }

    pub fn uid(&self) -> u64 {
        self.handle.uid()
    }

    pub fn update(&self, config: Option<DynamicValue>) -> BoxFuture<'static, Result<(), String>> {
        self.handle.update(config)
    }

    pub fn dispose(&self) -> BoxFuture<'static, Result<(), String>> {
        self.handle.dispose()
    }
}

pub struct PendingFiberHandle {
    pending_name: String,
    real: Mutex<Option<Arc<dyn FiberHandle>>>,
    disposed: Mutex<bool>,
}

impl PendingFiberHandle {
    pub fn new(pending_name: impl Into<String>) -> Self {
        Self {
            pending_name: pending_name.into(),
            real: Mutex::new(None),
            disposed: Mutex::new(false),
        }
    }

    pub fn attach(&self, real: Arc<dyn FiberHandle>) {
        if *self.disposed.lock() {
            tokio::spawn(async move {
                let _ = real.dispose().await;
            });
            return;
        }
        *self.real.lock() = Some(real);
    }
}

impl FiberHandle for PendingFiberHandle {
    fn name(&self) -> String {
        self.real
            .lock()
            .as_ref()
            .map(|real| real.name())
            .unwrap_or_else(|| self.pending_name.clone())
    }

    fn state(&self) -> FiberState {
        self.real
            .lock()
            .as_ref()
            .map(|real| real.state())
            .unwrap_or(FiberState::Activating)
    }

    fn uid(&self) -> u64 {
        self.real
            .lock()
            .as_ref()
            .unwrap_or_else(|| {
                panic!(
                    "handle '{}' has not been flushed yet (construction protocol)",
                    self.pending_name
                )
            })
            .uid()
    }

    fn update(&self, config: Option<DynamicValue>) -> BoxFuture<'static, Result<(), String>> {
        if let Some(real) = self.real.lock().clone() {
            return real.update(config);
        }
        let name = self.pending_name.clone();
        async move {
            Err(FiberProtocolError(format!(
                "handle '{name}' has not been flushed yet"
            ))
            .to_string())
        }
        .boxed()
    }

    fn dispose(&self) -> BoxFuture<'static, Result<(), String>> {
        *self.disposed.lock() = true;
        let real = self.real.lock().clone();
        async move {
            if let Some(real) = real {
                real.dispose().await?;
            }
            Ok(())
        }
        .boxed()
    }

    fn when_active(&self) -> BoxFuture<'static, Result<(), String>> {
        if let Some(real) = self.real.lock().clone() {
            return real.when_active();
        }
        async { Ok(()) }.boxed()
    }
}

static SCOPE_UNITS_TOKENS: Lazy<Mutex<HashMap<String, CollectionToken<ServiceRecipe>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn scope_units(kind: &str) -> CollectionToken<ServiceRecipe> {
    let mut tokens = SCOPE_UNITS_TOKENS.lock();
    if let Some(token) = tokens.get(kind) {
        return token.clone();
    }
    let token = collection::<ServiceRecipe>(
        format!("scope-units:{kind}"),
        CollectionOptions::default(),
    );
    tokens.insert(kind.to_string(), token.clone());
    token
}
