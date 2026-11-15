use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;

use futures_util::future::BoxFuture;

use super::cascade_engine::{CascadeEngine, UnitActivation};
use super::descriptors::{DynamicValue, SyncDescriptor, SyncDescriptor0};
use super::lifecycle::{DisposableStore, IDisposable};
use super::service_collection::ServiceCollection;
use crate::_base::event::Event;

pub const DI_TARGET: &str = "$di$target";
pub const DI_DEPENDENCIES: &str = "$di$dependencies";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DependencyKind {
    Instance,
    Collection,
    Ref,
}

#[derive(Clone)]
pub struct ServiceDependency {
    pub id: ServiceIdentifier<DynamicValue>,
    pub index: usize,
    pub kind: DependencyKind,
}

#[derive(Debug)]
pub struct DiTargetObj {
    pub identity: usize,
    pub di_target: usize,
    pub dependencies: Vec<ServiceDependency>,
}

impl DiTargetObj {
    pub fn new(identity: usize) -> Self {
        Self {
            identity,
            di_target: identity,
            dependencies: Vec::new(),
        }
    }
}

pub struct ServiceIdentifier<T = DynamicValue> {
    name: Arc<str>,
    marker: PhantomData<fn() -> T>,
}

impl<T> ServiceIdentifier<T> {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn decorate(&self, target: &mut DiTargetObj, index: usize) {
        store_service_dependency(self, target, index, DependencyKind::Instance);
    }
}

impl<T> Clone for ServiceIdentifier<T> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> fmt::Debug for ServiceIdentifier<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)
    }
}

impl<T> fmt::Display for ServiceIdentifier<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name)
    }
}

impl<T> PartialEq for ServiceIdentifier<T> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<T> Eq for ServiceIdentifier<T> {}

impl<T> Hash for ServiceIdentifier<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

pub static SERVICE_IDS: Lazy<Mutex<HashMap<String, ServiceIdentifier<DynamicValue>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn get_service_dependencies(target: &DiTargetObj) -> Vec<ServiceDependency> {
    target.dependencies.clone()
}

pub fn get_instance_dependencies(target: &DiTargetObj) -> Vec<ServiceDependency> {
    get_service_dependencies(target)
        .into_iter()
        .filter(|dependency| dependency.kind == DependencyKind::Instance)
        .collect()
}

pub fn store_custom_dependency<T>(
    id: &ServiceIdentifier<T>,
    kind: DependencyKind,
    target: &mut DiTargetObj,
    index: usize,
) {
    store_service_dependency(id, target, index, kind);
}

fn store_service_dependency<T>(
    id: &ServiceIdentifier<T>,
    target: &mut DiTargetObj,
    index: usize,
    kind: DependencyKind,
) {
    let dependency = ServiceDependency {
        id: ServiceIdentifier {
            name: id.name.clone(),
            marker: PhantomData,
        },
        index,
        kind,
    };
    if target.di_target == target.identity {
        target.dependencies.push(dependency);
    } else {
        target.dependencies = vec![dependency];
        target.di_target = target.identity;
    }
}

pub fn create_decorator<T>(name: impl Into<String>) -> ServiceIdentifier<T> {
    let name = name.into();
    let mut ids = SERVICE_IDS.lock();
    if let Some(existing) = ids.get(&name) {
        return ServiceIdentifier {
            name: existing.name.clone(),
            marker: PhantomData,
        };
    }
    let id = ServiceIdentifier {
        name: Arc::<str>::from(name.as_str()),
        marker: PhantomData,
    };
    ids.insert(
        name,
        ServiceIdentifier {
            name: id.name.clone(),
            marker: PhantomData,
        },
    );
    id
}

pub fn is_service_identifier(thing: &dyn std::any::Any) -> bool {
    thing.is::<ServiceIdentifier<DynamicValue>>()
}

pub fn refine_service_decorator<T1, T>(id: &ServiceIdentifier<T1>) -> ServiceIdentifier<T> {
    ServiceIdentifier {
        name: id.name.clone(),
        marker: PhantomData,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ScopeActivation {
    OnScopeCreated = 0,
    OnDemand = 1,
}

pub trait BrandedService {}

pub type IConstructorSignature<T> = Arc<dyn Fn(Vec<DynamicValue>) -> T + Send + Sync>;
pub type GetLeadingNonServiceArgs<TArgs> = TArgs;

#[derive(Clone)]
pub struct LiveRef<T> {
    pub current: Option<T>,
    pub on_did_change: Event<()>,
}

impl<T> LiveRef<T>
where
    T: Clone,
{
    pub fn current(&self) -> Option<T> {
        self.current.clone()
    }
}

pub fn ref_dependency<T>(
    id: &ServiceIdentifier<T>,
) -> impl Fn(&mut DiTargetObj, usize) + '_ {
    move |target, index| {
        store_service_dependency(id, target, index, DependencyKind::Ref);
    }
}

pub trait ServicesAccessor {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue;
}

pub trait ServicesAccessorExt {
    fn get<T>(&self, id: &ServiceIdentifier<T>) -> T
    where
        T: Clone + Send + Sync + 'static;
}

impl<S> ServicesAccessorExt for S
where
    S: ServicesAccessor + ?Sized,
{
    fn get<T>(&self, id: &ServiceIdentifier<T>) -> T
    where
        T: Clone + Send + Sync + 'static,
    {
        let id = ServiceIdentifier {
            name: id.name.clone(),
            marker: PhantomData,
        };
        Arc::downcast::<T>(self.get_dynamic(&id))
            .expect("service type does not match its identifier")
            .as_ref()
            .clone()
    }
}

#[derive(Clone, Default)]
pub struct ProvideOptions {
    pub activation: Option<UnitActivation>,
    pub config: Option<DynamicValue>,
}

pub struct ProvideAllEntry<T = DynamicValue> {
    pub id: ServiceIdentifier<T>,
    pub descriptor: SyncDescriptor<T>,
    pub options: Option<ProvideOptions>,
}

pub trait ProvideHandle: IDisposable {
    fn uid(&self) -> u64;
}

pub trait IInstantiationService: Send + Sync {
    fn cascade(&self) -> Arc<CascadeEngine>;
    fn invoke_function_dyn(&self, callback: &mut dyn FnMut(&dyn ServicesAccessor));
    fn create_instance<T>(&self, descriptor: &SyncDescriptor0<T>) -> T
    where
        Self: Sized;
    fn create_child(
        &self,
        services: ServiceCollection,
        store: Option<DisposableStore>,
    ) -> Arc<dyn IInstantiationService>;
    fn provide<T>(
        &self,
        id: ServiceIdentifier<T>,
        instance_or_descriptor: DynamicValue,
        options: Option<ProvideOptions>,
    ) -> Arc<dyn ProvideHandle>
    where
        Self: Sized;
    fn provide_all(&self, entries: Vec<ProvideAllEntry>);
    fn unprovide<T>(&self, id: &ServiceIdentifier<T>)
    where
        Self: Sized;
    fn dispose(&self);
    fn dispose_async(&self) -> BoxFuture<'static, ()>;
}

pub trait IInstantiationServiceExt: IInstantiationService {
    fn invoke_function<R>(&self, callback: impl FnOnce(&dyn ServicesAccessor) -> R) -> R {
        let mut callback = Some(callback);
        let mut result = None;
        self.invoke_function_dyn(&mut |accessor| {
            if let Some(callback) = callback.take() {
                result = Some(callback(accessor));
            }
        });
        result.expect("invoke_function callback did not run")
    }
}

impl<T> IInstantiationServiceExt for T where T: IInstantiationService + ?Sized {}

pub static I_INSTANTIATION_SERVICE: Lazy<ServiceIdentifier<DynamicValue>> =
    Lazy::new(|| create_decorator("instantiationService"));

pub trait ServiceCollectionLike {
    fn set<T>(&mut self, id: ServiceIdentifier<T>, instance_or_descriptor: DynamicValue);
    fn get<T>(&self, id: &ServiceIdentifier<T>) -> Option<DynamicValue>;
    fn has<T>(&self, id: &ServiceIdentifier<T>) -> bool;
    fn for_each(&self, callback: &mut dyn FnMut(&ServiceIdentifier<DynamicValue>, &DynamicValue));
}
