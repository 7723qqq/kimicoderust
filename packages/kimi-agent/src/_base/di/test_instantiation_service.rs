use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;

use super::dependency_graph::ObjectId;
use super::descriptors::{DynamicValue, SyncDescriptor};
use super::fiber::FiberHost;
use super::instantiation::{ServiceIdentifier, ServicesAccessor};
use super::instantiation_service::{InstantiationService, descriptor_ctor_id};
use super::lifecycle::{DisposableStore, IDisposable, to_disposable};
use super::service_collection::{ServiceCollection, ServiceValue};

pub struct TestInstantiationService {
    inner: Arc<InstantiationService>,
    service_collection: Arc<ServiceCollection>,
    class_stubs: Arc<Mutex<HashMap<ObjectId, DynamicValue>>>,
    parent_test_service: Option<Arc<TestInstantiationService>>,
    proper_dispose: bool,
}

impl TestInstantiationService {
    pub fn new(
        service_collection: Arc<ServiceCollection>,
        strict: bool,
        parent: Option<Arc<InstantiationService>>,
        proper_dispose: bool,
    ) -> Arc<Self> {
        let inner = InstantiationService::new(service_collection.clone(), strict, parent, false);
        Arc::new(Self {
            inner,
            service_collection,
            class_stubs: Arc::new(Mutex::new(HashMap::new())),
            parent_test_service: None,
            proper_dispose,
        })
    }

    fn with_parent(
        service_collection: Arc<ServiceCollection>,
        parent: &Arc<TestInstantiationService>,
    ) -> Arc<Self> {
        let inner = InstantiationService::new(
            service_collection.clone(),
            false,
            Some(parent.inner.clone()),
            false,
        );
        Arc::new(Self {
            inner,
            service_collection,
            class_stubs: parent.class_stubs.clone(),
            parent_test_service: Some(parent.clone()),
            proper_dispose: true,
        })
    }

    pub fn inner(&self) -> Arc<InstantiationService> {
        self.inner.clone()
    }

    pub fn get(&self, id: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String> {
        self.inner.resolve_instance(id)
    }

    pub fn set(
        self: &Arc<Self>,
        id: ServiceIdentifier<DynamicValue>,
        value: ServiceValue,
    ) -> Result<Option<ServiceValue>, String> {
        let previous = self.service_collection.get_value(&id);
        self.inner.provide(
            id,
            value,
            Some(super::instantiation::ProvideOptions {
                activation: Some(super::cascade_engine::UnitActivation::OnDemand),
                config: None,
            }),
        )?;
        Ok(previous)
    }

    pub fn mock(&self, id: ServiceIdentifier<DynamicValue>) -> Result<Mock, String> {
        let object = self.create_service(id, None, SinonOptions { mock: true, stub: false }, false)?;
        Ok(Mock { object })
    }

    pub fn stub_instance(&self, descriptor: &SyncDescriptor<DynamicValue>, instance: DynamicValue) {
        self.class_stubs
            .lock()
            .insert(descriptor_ctor_id(descriptor), instance);
    }

    pub fn get_class_stub(&self, descriptor: &SyncDescriptor<DynamicValue>) -> Option<DynamicValue> {
        self.class_stubs
            .lock()
            .get(&descriptor_ctor_id(descriptor))
            .cloned()
            .or_else(|| {
                self.parent_test_service
                    .as_ref()?
                    .get_class_stub(descriptor)
            })
    }

    pub fn create_instance(
        &self,
        descriptor: &SyncDescriptor<DynamicValue>,
        rest: Vec<DynamicValue>,
    ) -> Result<DynamicValue, String> {
        if let Some(stub) = self.get_class_stub(descriptor) {
            return Ok(stub);
        }
        self.inner.create_instance(descriptor, rest)
    }

    pub fn stub_descriptor(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
    ) -> SyncDescriptor<DynamicValue> {
        self.service_collection.set_value(
            id,
            ServiceValue::Descriptor(descriptor.clone()),
            None,
        );
        descriptor
    }

    pub fn stub_service(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        source: Option<DynamicValue>,
    ) -> Result<DynamicValue, String> {
        self.create_service(id, source, SinonOptions { mock: false, stub: true }, false)
    }

    pub fn stub_property(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        service: Option<DynamicValue>,
        property: &str,
        value: DynamicValue,
    ) -> Result<Replacement, String> {
        let object = self.create_service(
            id,
            service,
            SinonOptions { mock: false, stub: true },
            false,
        )?;
        let stub = Arc::downcast::<StubObject>(object)
            .map_err(|_| "stub service is not a mutable StubObject".to_string())?;
        if let Some(current) = stub.properties.lock().insert(property.to_string(), value.clone()) {
            let _ = current;
        }
        Ok(Replacement::Value(value))
    }

    pub fn stub_promise(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        service: Option<DynamicValue>,
        property: &str,
        value: DynamicValue,
    ) -> Result<Replacement, String> {
        let promise = async move { value }.boxed().shared();
        self.stub_property(
            id,
            service,
            property,
            Arc::new(promise) as DynamicValue,
        )
    }

    pub fn spy(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        service: Option<DynamicValue>,
        property: &str,
    ) -> Result<Arc<Spy>, String> {
        let spy = Arc::new(Spy::new(None));
        self.stub_property(id, service, property, Arc::new(spy.clone()) as DynamicValue)?;
        Ok(spy)
    }

    fn create_service(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        source: Option<DynamicValue>,
        options: SinonOptions,
        reset: bool,
    ) -> Result<DynamicValue, String> {
        if !reset {
            if let Some(ServiceValue::Instance(service)) = self.service_collection.get_value(&id) {
                return Ok(service);
            }
        }
        let service = source.unwrap_or_else(|| {
            Arc::new(StubObject::new(options)) as DynamicValue
        });
        if !options.mock {
            self.service_collection.set_value(
                id,
                ServiceValue::Instance(service.clone()),
                None,
            );
        }
        Ok(service)
    }

    pub fn create_child(
        self: &Arc<Self>,
        services: Arc<ServiceCollection>,
    ) -> Arc<TestInstantiationService> {
        Self::with_parent(services, self)
    }

    pub fn dispose_async(&self) -> BoxFuture<'static, ()> {
        if self.proper_dispose {
            self.inner.dispose_async().boxed()
        } else {
            async {}.boxed()
        }
    }
}

impl ServicesAccessor for TestInstantiationService {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue {
        self.get(id).unwrap_or_else(|error| panic!("{error}"))
    }
}

impl IDisposable for TestInstantiationService {
    fn dispose(&self) {
        if self.proper_dispose {
            self.inner.dispose();
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct SinonOptions {
    pub mock: bool,
    pub stub: bool,
}

pub struct Mock {
    pub object: DynamicValue,
}

pub struct StubObject {
    pub properties: Mutex<HashMap<String, DynamicValue>>,
    pub options: SinonOptions,
}

impl StubObject {
    pub fn new(options: SinonOptions) -> Self {
        Self {
            properties: Mutex::new(HashMap::new()),
            options,
        }
    }
}

pub enum Replacement {
    Value(DynamicValue),
    Spy(Arc<Spy>),
    Stub(Arc<Stub>),
}

pub type SpyCallback = Arc<dyn Fn(Vec<DynamicValue>) -> DynamicValue + Send + Sync>;

pub struct Spy {
    callback: Option<SpyCallback>,
    call_count: AtomicUsize,
}

impl Spy {
    pub fn new(callback: Option<SpyCallback>) -> Self {
        Self {
            callback,
            call_count: AtomicUsize::new(0),
        }
    }

    pub fn call(&self, args: Vec<DynamicValue>) -> DynamicValue {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        self.callback
            .as_ref()
            .map(|callback| callback(args))
            .unwrap_or_else(|| Arc::new(()))
    }

    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

pub struct Stub {
    pub value: Mutex<Option<DynamicValue>>,
}

impl Stub {
    pub fn call(&self) -> DynamicValue {
        self.value.lock().clone().unwrap_or_else(|| Arc::new(()))
    }
}

pub trait ServiceRegistration {
    fn define(&mut self, id: ServiceIdentifier<DynamicValue>, descriptor: SyncDescriptor<DynamicValue>);
    fn define_instance(&mut self, id: ServiceIdentifier<DynamicValue>, instance: DynamicValue);
    fn define_partial_instance(&mut self, id: ServiceIdentifier<DynamicValue>, instance: DynamicValue) {
        self.define_instance(id, instance);
    }
}

pub type ServiceGroup = Arc<dyn Fn(&mut dyn ServiceRegistration) + Send + Sync>;

#[derive(Clone, Default)]
pub struct CreateServicesOptions {
    pub base: Vec<ServiceGroup>,
    pub additional_services: Option<ServiceGroup>,
    pub strict: bool,
}

struct Registration<'a> {
    collection: &'a ServiceCollection,
    instance_ids: &'a mut HashSet<ServiceIdentifier<DynamicValue>>,
    overwrite: bool,
}

impl Registration<'_> {
    fn register(&mut self, id: ServiceIdentifier<DynamicValue>, value: ServiceValue, instance: bool) {
        if self.overwrite || !self.collection.has(&id) {
            self.collection.set_value(id.clone(), value, None);
        }
        if instance {
            self.instance_ids.insert(id);
        }
    }
}

impl ServiceRegistration for Registration<'_> {
    fn define(&mut self, id: ServiceIdentifier<DynamicValue>, descriptor: SyncDescriptor<DynamicValue>) {
        self.register(id, ServiceValue::Descriptor(descriptor), false);
    }

    fn define_instance(&mut self, id: ServiceIdentifier<DynamicValue>, instance: DynamicValue) {
        self.register(id, ServiceValue::Instance(instance), true);
    }
}

pub fn create_services(
    disposables: &DisposableStore,
    options: CreateServicesOptions,
) -> Arc<TestInstantiationService> {
    let service_collection = Arc::new(ServiceCollection::default());
    let mut instance_ids = HashSet::new();
    for group in options.base {
        let mut registration = Registration {
            collection: &service_collection,
            instance_ids: &mut instance_ids,
            overwrite: false,
        };
        group(&mut registration);
    }
    if let Some(group) = options.additional_services {
        let mut registration = Registration {
            collection: &service_collection,
            instance_ids: &mut instance_ids,
            overwrite: true,
        };
        group(&mut registration);
    }
    let instantiation_service = TestInstantiationService::new(
        service_collection.clone(),
        options.strict,
        None,
        true,
    );
    disposables.add(instantiation_service.clone());
    disposables.add(to_disposable(move || {
        for id in instance_ids {
            if let Some(ServiceValue::Instance(instance)) = service_collection.get_value(&id)
                && let Ok(disposable) = Arc::downcast::<Arc<dyn IDisposable>>(instance)
            {
                disposable.dispose();
            }
        }
    }));
    instantiation_service
}

pub type PromiseValue = Shared<BoxFuture<'static, DynamicValue>>;
