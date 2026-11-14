use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::cascade_engine::{
    CascadeAction, CascadeChange, CascadeEngine, CascadeEngineOptions, CascadeHost,
    CascadeScopeHandle, CascadeTree, UnitActivation,
};
use super::collection::{CollectionStore, CollectionToken, CollectionViewImpl, StoredRecord};
use super::dependency_graph::{DependencyEdgeKind, DependencyGraph, ObjectId, ScopedToken};
use super::descriptors::{DynamicValue, SyncDescriptor, SyncDescriptor0};
use super::errors::{CascadeConflictError, CyclicDependencyError};
use super::fiber::{
    ConstructionFrame, ConstructedService, FiberHost, TokenProvideCore, UnitInternals,
    bind_service_unit, is_service_recipe, pop_construction_frame, push_construction_frame,
};
use super::graph::Graph;
use super::instantiation::{
    DependencyKind, IInstantiationService, I_INSTANTIATION_SERVICE, LiveRef, ProvideAllEntry,
    ProvideHandle, ProvideOptions, ServiceCollectionLike, ServiceDependency, ServiceIdentifier,
    ServicesAccessor,
};
use super::lifecycle::{
    DisposableRef, DisposableStore, IDisposable, to_disposable,
};
use super::service_collection::{ServiceCollection, ServiceCollectionOptions, ServiceValue};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::event::{Emitter, Subscription};
use crate::_base::lifecycle::disposer::Disposer;
use crate::_base::lifecycle::ledger::{Ledger, LedgerEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TraceType {
    None,
    Creation,
    Invocation,
    Branch,
}

#[derive(Clone)]
pub struct Trace {
    inner: Arc<TraceInner>,
}

struct TraceInner {
    kind: TraceType,
    name: Option<String>,
    start: std::time::Instant,
    dependencies: Mutex<Vec<(ServiceIdentifier<DynamicValue>, bool, Trace)>>,
}

static TRACE_ALL: Lazy<Mutex<HashSet<String>>> = Lazy::new(|| Mutex::new(HashSet::new()));
static TRACE_TOTALS: AtomicU64 = AtomicU64::new(0);

impl Trace {
    pub fn all() -> HashSet<String> {
        TRACE_ALL.lock().clone()
    }

    fn none() -> Self {
        Self::new(TraceType::None, None)
    }

    fn new(kind: TraceType, name: Option<String>) -> Self {
        Self {
            inner: Arc::new(TraceInner {
                kind,
                name,
                start: std::time::Instant::now(),
                dependencies: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn trace_invocation(enable: bool, name: impl Into<String>) -> Self {
        if enable {
            Self::new(TraceType::Invocation, Some(name.into()))
        } else {
            Self::none()
        }
    }

    pub fn trace_creation(enable: bool, name: impl Into<String>) -> Self {
        if enable {
            Self::new(TraceType::Creation, Some(name.into()))
        } else {
            Self::none()
        }
    }

    pub fn branch(&self, id: &ServiceIdentifier<DynamicValue>, first: bool) -> Self {
        if self.inner.kind == TraceType::None {
            return self.clone();
        }
        let child = Self::new(TraceType::Branch, Some(id.to_string()));
        self.inner
            .dependencies
            .lock()
            .push((id.clone(), first, child.clone()));
        child
    }

    pub fn stop(&self) {
        if self.inner.kind == TraceType::None {
            return;
        }
        let duration = self.inner.start.elapsed().as_millis() as u64;
        let total = TRACE_TOTALS.fetch_add(duration, Ordering::Relaxed) + duration;
        fn print_child(
            depth: usize,
            trace: &Trace,
            caused_creation: &mut bool,
        ) -> String {
            let prefix = "\t".repeat(depth);
            let mut lines = Vec::new();
            for (id, first, child) in trace.inner.dependencies.lock().iter() {
                if *first {
                    *caused_creation = true;
                    lines.push(format!("{prefix}CREATES -> {id}"));
                    let nested = print_child(depth + 1, child, caused_creation);
                    if !nested.is_empty() {
                        lines.push(nested);
                    }
                } else {
                    lines.push(format!("{prefix}uses -> {id}"));
                }
            }
            lines.join("\n")
        }
        let mut caused_creation = false;
        let children = print_child(1, self, &mut caused_creation);
        let lines = vec![
            format!(
                "{} {}",
                if self.inner.kind == TraceType::Creation {
                    "CREATE"
                } else {
                    "CALL"
                },
                self.inner.name.clone().unwrap_or_default()
            ),
            children,
            format!("DONE, took {duration}ms (grand total {total}ms)"),
        ];
        if duration > 2 || caused_creation {
            TRACE_ALL.lock().insert(lines.join("\n"));
        }
    }
}

#[derive(Clone, Debug)]
pub struct InstantiationServiceDisposedError;

impl InstantiationServiceDisposedError {
    pub const CODE: &'static str = "instantiation_service_disposed";
}

impl fmt::Display for InstantiationServiceDisposedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("InstantiationService has been disposed")
    }
}

impl std::error::Error for InstantiationServiceDisposedError {}

pub fn is_instantiation_service_disposed_error(error: &dyn std::error::Error) -> bool {
    error.is::<InstantiationServiceDisposedError>()
}

#[derive(Default)]
pub struct GlobalGraph {
    edges: Mutex<Vec<(String, String)>>,
}

impl GlobalGraph {
    pub fn insert_edge(&self, from: String, to: String) {
        self.edges.lock().push((from, to));
    }

    pub fn edges(&self) -> Vec<(String, String)> {
        self.edges.lock().clone()
    }
}

static NEXT_CONTAINER_ID: AtomicUsize = AtomicUsize::new(1);
static CONTAINERS: Lazy<Mutex<HashMap<ObjectId, Weak<InstantiationService>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static DESCRIPTOR_DEPENDENCIES: Lazy<Mutex<HashMap<ObjectId, Vec<ServiceDependency>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static DESCRIPTOR_RECIPES: Lazy<Mutex<HashMap<ObjectId, super::fiber::ServiceClassRecipe>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn descriptor_ctor_id(descriptor: &SyncDescriptor<DynamicValue>) -> ObjectId {
    Arc::as_ptr(&descriptor.ctor) as *const () as ObjectId
}

pub fn register_descriptor_dependencies(
    descriptor: &SyncDescriptor<DynamicValue>,
    dependencies: Vec<ServiceDependency>,
) {
    DESCRIPTOR_DEPENDENCIES
        .lock()
        .insert(descriptor_ctor_id(descriptor), dependencies);
}

pub fn register_service_recipe(recipe: &super::fiber::ServiceClassRecipe) {
    register_descriptor_dependencies(&recipe.descriptor, recipe.statics.inject.iter().enumerate().map(|(index, id)| ServiceDependency {
        id: id.clone(),
        index,
        kind: DependencyKind::Instance,
    }).collect());
    DESCRIPTOR_RECIPES
        .lock()
        .insert(recipe.ctor_id(), recipe.clone());
}

pub struct InstantiationService {
    id: ObjectId,
    this_ref: once_cell::sync::OnceCell<Weak<InstantiationService>>,
    services: Arc<ServiceCollection>,
    strict: bool,
    parent: Option<Arc<InstantiationService>>,
    enable_tracing: bool,
    pub global_graph: Option<Arc<GlobalGraph>>,
    ledger: Ledger,
    tree: Arc<CascadeTree>,
    cascade: once_cell::sync::OnceCell<Arc<CascadeEngine>>,
    parent_ledger_entry: Mutex<Option<LedgerEntry>>,
    instance_entries: Mutex<HashMap<ObjectId, LedgerEntry>>,
    provide_entries: Mutex<HashMap<ServiceIdentifier<DynamicValue>, ProvideEntry>>,
    cascade_resolving: AtomicBool,
    children: Mutex<IndexMap<ObjectId, Arc<InstantiationService>>>,
    in_progress: Mutex<Vec<ServiceIdentifier<DynamicValue>>>,
    active_instantiations: Mutex<HashSet<ServiceIdentifier<DynamicValue>>>,
    collection_store: Arc<CollectionStore>,
    collection_views: Mutex<HashMap<String, Arc<CollectionViewImpl<DynamicValue>>>>,
    edge_nodes: Mutex<HashSet<ObjectId>>,
    debug_label: Mutex<Option<String>>,
    disposed: AtomicBool,
    next_unit_uid: AtomicU64,
    dispose_promise: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
}

struct ProvideEntry {
    entry: LedgerEntry,
    core: Arc<dyn TokenProvideCore>,
}

impl InstantiationService {
    pub fn new(
        services: Arc<ServiceCollection>,
        strict: bool,
        parent: Option<Arc<InstantiationService>>,
        enable_tracing: bool,
    ) -> Arc<Self> {
        let id = NEXT_CONTAINER_ID.fetch_add(1, Ordering::Relaxed);
        let tree = parent
            .as_ref()
            .map(|parent| parent.tree.clone())
            .unwrap_or_else(|| CascadeTree::new(DependencyGraph::new()));
        let collection_store = parent
            .as_ref()
            .map(|parent| parent.collection_store.clone())
            .unwrap_or_else(|| {
                Arc::new(CollectionStore::new(|container| {
                    CONTAINERS
                        .lock()
                        .get(&container)
                        .and_then(Weak::upgrade)
                        .and_then(|container| container.parent.as_ref().map(|parent| parent.id))
                }))
            });
        let global_graph = if enable_tracing {
            Some(
                parent
                    .as_ref()
                    .and_then(|parent| parent.global_graph.clone())
                    .unwrap_or_else(|| Arc::new(GlobalGraph::default())),
            )
        } else {
            None
        };
        let service = Arc::new(Self {
            id,
            this_ref: once_cell::sync::OnceCell::new(),
            services: services.clone(),
            strict,
            parent,
            enable_tracing,
            global_graph,
            ledger: Ledger::new("InstantiationService"),
            tree: tree.clone(),
            cascade: once_cell::sync::OnceCell::new(),
            parent_ledger_entry: Mutex::new(None),
            instance_entries: Mutex::new(HashMap::new()),
            provide_entries: Mutex::new(HashMap::new()),
            cascade_resolving: AtomicBool::new(false),
            children: Mutex::new(IndexMap::new()),
            in_progress: Mutex::new(Vec::new()),
            active_instantiations: Mutex::new(HashSet::new()),
            collection_store,
            collection_views: Mutex::new(HashMap::new()),
            edge_nodes: Mutex::new(HashSet::new()),
            debug_label: Mutex::new(None),
            disposed: AtomicBool::new(false),
            next_unit_uid: AtomicU64::new(0),
            dispose_promise: Mutex::new(None),
        });
        let _ = service.this_ref.set(Arc::downgrade(&service));
        CONTAINERS.lock().insert(id, Arc::downgrade(&service));
        let host = Arc::new(ServiceCascadeHost {
            service: service.clone(),
        });
        let cascade = CascadeEngine::new(
            host,
            service.clone(),
            tree,
            CascadeEngineOptions::default(),
        );
        let _ = service.cascade.set(cascade);
        services.set_value(
            I_INSTANTIATION_SERVICE.clone(),
            ServiceValue::Instance(Arc::new(service.clone())),
            None,
        );
        service
    }

    fn arc(&self) -> Arc<Self> {
        self.this_ref
            .get()
            .and_then(Weak::upgrade)
            .expect("instantiation service is not registered")
    }

    pub fn cascade(&self) -> Arc<CascadeEngine> {
        self.cascade.get().expect("cascade is not initialized").clone()
    }

    pub fn dependency_graph(&self) -> Arc<Mutex<DependencyGraph>> {
        self.tree.graph.clone()
    }

    pub fn cascade_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    pub fn cascade_depth(&self) -> usize {
        self.parent.as_ref().map_or(0, |parent| parent.cascade_depth() + 1)
    }

    pub fn debug_label(&self) -> Option<String> {
        self.debug_label.lock().clone()
    }

    pub fn set_debug_label(&self, label: impl Into<String>) {
        *self.debug_label.lock() = Some(label.into());
    }

    fn owner_of(&self, id: &ServiceIdentifier<DynamicValue>) -> Option<Arc<Self>> {
        if self.services.has(id) {
            Some(self.arc())
        } else {
            self.parent.as_ref()?.owner_of(id)
        }
    }

    pub fn invoke_function<R>(&self, callback: impl FnOnce(&dyn ServicesAccessor) -> R) -> R {
        self.assert_not_disposed().unwrap_or_else(|error| panic!("{error}"));
        let trace = Trace::trace_invocation(self.enable_tracing, "invokeFunction");
        let done = Arc::new(AtomicBool::new(false));
        let accessor = ServiceAccessor {
            service: self.arc(),
            done: done.clone(),
            trace: trace.clone(),
        };
        let guard = InvocationGuard { done, trace };
        let result = callback(&accessor);
        drop(guard);
        result
    }

    pub fn provide(
        self: &Arc<Self>,
        id: ServiceIdentifier<DynamicValue>,
        value: ServiceValue,
        options: Option<ProvideOptions>,
    ) -> Result<Arc<dyn ProvideHandle>, String> {
        self.assert_not_disposed()?;
        let core = self.provide_core(id.clone(), value, options)?;
        let dispose_core = core.clone();
        let entry = self.ledger.register(
            move |_| {
                let future = dispose_core.dispose();
                tokio::spawn(async move {
                    let _ = future.await;
                });
            },
            &format!("provide:{id}"),
        );
        self.provide_entries.lock().insert(
            id,
            ProvideEntry {
                entry: entry.clone(),
                core: core.clone(),
            },
        );
        Ok(Arc::new(ServiceProvideHandle { entry, core }))
    }

    pub fn provide_all(
        self: &Arc<Self>,
        entries: Vec<ProvideAllEntry<DynamicValue>>,
    ) -> Result<(), String> {
        self.assert_not_disposed()?;
        let mut changes = Vec::new();
        for entry in &entries {
            self.release_provide_entry(&entry.id);
            changes.push(CascadeChange {
                action: CascadeAction::Provide,
                token: entry.id.clone(),
                descriptor: Some(entry.descriptor.clone()),
                instance: None,
                activation: entry.options.as_ref().and_then(|options| options.activation),
                config: entry.options.as_ref().and_then(|options| options.config.clone()),
                reason: format!("provide {}", entry.id),
            });
        }
        let cascade = self.cascade();
        tokio::spawn(async move {
            if let Err(error) = cascade.submit_all(changes).await {
                on_unexpected_error(error);
            }
        });
        for entry in entries {
            let Some(uid) = self.services.uid_of(&entry.id) else {
                continue;
            };
            let service = self.clone();
            let id = entry.id.clone();
            let ledger_entry = self.ledger.register(
                move |_| {
                    if service.services.uid_of(&id) == Some(uid) {
                        let future = service.unprovide_core(id.clone());
                        tokio::spawn(async move {
                            let _ = future.await;
                        });
                    }
                },
                &format!("provide:{}", entry.id),
            );
            let core = Arc::new(FixedProvideCore {
                service: self.clone(),
                id: entry.id.clone(),
                uid,
            });
            self.provide_entries.lock().insert(
                entry.id,
                ProvideEntry {
                    entry: ledger_entry,
                    core,
                },
            );
        }
        Ok(())
    }

    fn provide_core(
        self: &Arc<Self>,
        id: ServiceIdentifier<DynamicValue>,
        value: ServiceValue,
        options: Option<ProvideOptions>,
    ) -> Result<Arc<dyn TokenProvideCore>, String> {
        self.release_provide_entry(&id);
        if let ServiceValue::Instance(instance) = &value
            && self
                .services
                .get_value(&id)
                .is_some_and(|current| match current {
                    ServiceValue::Instance(current) => Arc::ptr_eq(&current, instance),
                    _ => false,
                })
        {
            self.services.set_value(id.clone(), value, None);
            let uid = self.services.uid_of(&id).unwrap();
            return Ok(Arc::new(FixedProvideCore {
                service: self.clone(),
                id,
                uid,
            }));
        }
        let before_uid = self.services.uid_of(&id);
        let applied_uid = Arc::new(Mutex::new(None));
        let subscription = self.services.on_did_change(&id, {
            let applied_uid = applied_uid.clone();
            move |change| {
                if change.new_uid.is_some() && change.new_uid != before_uid {
                    let mut applied = applied_uid.lock();
                    if applied.is_none() {
                        *applied = change.new_uid;
                    }
                }
            }
        });
        let options = options.unwrap_or_default();
        let change = CascadeChange {
            action: CascadeAction::Provide,
            token: id.clone(),
            descriptor: match &value {
                ServiceValue::Descriptor(descriptor) => Some(descriptor.clone()),
                _ => None,
            },
            instance: match value {
                ServiceValue::Instance(instance) => Some(instance),
                _ => None,
            },
            activation: options.activation,
            config: options.config,
            reason: format!("provide {id}"),
        };
        let cascade = self.cascade();
        tokio::spawn(async move {
            if let Err(error) = cascade.submit(change).await {
                on_unexpected_error(error);
            }
        });
        Ok(Arc::new(CascadeProvideCore {
            service: self.clone(),
            id,
            applied_uid,
            subscription: Mutex::new(Some(subscription)),
        }))
    }

    fn unprovide_core(
        self: &Arc<Self>,
        id: ServiceIdentifier<DynamicValue>,
    ) -> BoxFuture<'static, Result<(), String>> {
        if self.disposed.load(Ordering::SeqCst) {
            return async { Ok(()) }.boxed();
        }
        self.release_provide_entry(&id);
        if self.services.get_value(&id).is_none() {
            return async { Ok(()) }.boxed();
        }
        let cascade = self.cascade();
        async move {
            cascade
                .submit(CascadeChange {
                    action: CascadeAction::Unprovide,
                    token: id.clone(),
                    descriptor: None,
                    instance: None,
                    activation: None,
                    config: None,
                    reason: format!("unprovide {id}"),
                })
                .await
                .or_else(|error| {
                    on_unexpected_error(error);
                    Ok(())
                })
        }
        .boxed()
    }

    pub fn unprovide(self: &Arc<Self>, id: ServiceIdentifier<DynamicValue>) {
        let future = self.unprovide_core(id);
        tokio::spawn(async move {
            let _ = future.await;
        });
    }

    fn release_provide_entry(&self, id: &ServiceIdentifier<DynamicValue>) {
        if let Some(previous) = self.provide_entries.lock().remove(id) {
            previous.entry.release();
            previous.core.release();
        }
    }

    fn retire_unit(
        self: &Arc<Self>,
        id: ServiceIdentifier<DynamicValue>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let Some(ServiceValue::Instance(instance)) = self.services.get_value(&id) else {
            return async { Ok(()) }.boxed();
        };
        self.services.unmaterialize(&id);
        let instance_id = dynamic_id(&instance);
        let Some(entry) = self.instance_entries.lock().remove(&instance_id) else {
            return async { Ok(()) }.boxed();
        };
        self.edge_nodes.lock().remove(&instance_id);
        self.tree.graph.lock().remove_instance(instance_id);
        async move {
            if let Some(future) = entry.dispose("cascade") {
                future.await;
            }
            Ok(())
        }
        .boxed()
    }

    pub fn fiber_host(self: &Arc<Self>) -> Arc<dyn FiberHost> {
        self.clone()
    }

    pub fn collection_store(&self) -> Arc<CollectionStore> {
        self.collection_store.clone()
    }

    pub fn ledger(&self) -> Ledger {
        self.ledger.clone()
    }

    pub fn cascade_tree(&self) -> Arc<CascadeTree> {
        self.tree.clone()
    }

    pub fn children(&self) -> Vec<Arc<InstantiationService>> {
        self.children.lock().values().cloned().collect()
    }

    pub fn services_snapshot(&self) -> Vec<(String, u64)> {
        let mut snapshot = Vec::new();
        self.services.for_each_value(|id, _| {
            if let Some(uid) = self.services.uid_of(&id) {
                snapshot.push((id.to_string(), uid));
            }
        });
        snapshot
    }

    pub fn find_identifier(&self, token: &str) -> Option<ServiceIdentifier<DynamicValue>> {
        let mut found = None;
        self.services.for_each_value(|id, _| {
            if found.is_none() && id.to_string() == token {
                found = Some(id);
            }
        });
        found
    }

    pub fn anchor_kernel_entry(&self, disposer: Disposer, label: &str) -> LedgerEntry {
        self.ledger.register(disposer, label)
    }

    pub fn anchor_kernel_finalizer(&self, disposer: Disposer, label: &str) -> LedgerEntry {
        self.ledger.register_finalizer(disposer, label)
    }

    fn materialized_instance_of(&self, id: &ServiceIdentifier<DynamicValue>) -> Option<DynamicValue> {
        match self.services.get_value(id) {
            Some(ServiceValue::Instance(instance)) => Some(instance),
            Some(ServiceValue::Descriptor(_)) => None,
            None => self.parent.as_ref()?.materialized_instance_of(id),
        }
    }

    fn live_ref(self: &Arc<Self>, id: &ServiceIdentifier<DynamicValue>) -> LiveRef<DynamicValue> {
        let change = Arc::new(Emitter::<()>::new());
        let mut chain = vec![self.clone()];
        let mut parent = self.parent.clone();
        while let Some(container) = parent {
            parent = container.parent.clone();
            chain.push(container);
        }
        let mut subscriptions = Vec::new();
        for container in chain {
            subscriptions.push(container.services.on_did_change(id, {
                let change = change.clone();
                move |_| change.fire(())
            }));
        }
        let dispose_change = change.clone();
        self.ledger.register(
            move |_| {
                for subscription in subscriptions {
                    subscription.dispose();
                }
                dispose_change.dispose();
            },
            &format!("ref:{id}"),
        );
        LiveRef {
            current: self.materialized_instance_of(id),
            on_did_change: change.event(),
        }
    }

    fn collection_view(
        self: &Arc<Self>,
        token: CollectionToken<DynamicValue>,
    ) -> Arc<CollectionViewImpl<DynamicValue>> {
        if let Some(view) = self.collection_views.lock().get(&token.name) {
            return view.clone();
        }
        let view = self.collection_store.create_view(token.clone(), self.id);
        self.collection_views
            .lock()
            .insert(token.name, view.clone());
        view
    }

    fn scope_path(&self) -> String {
        let mut labels = vec![
            self.debug_label()
                .unwrap_or_else(|| format!("#{}", self.tree.seq_of(self.id))),
        ];
        let mut parent = self.parent.clone();
        while let Some(container) = parent {
            labels.insert(
                0,
                container
                    .debug_label()
                    .unwrap_or_else(|| format!("#{}", self.tree.seq_of(container.id))),
            );
            parent = container.parent.clone();
        }
        labels.join("/")
    }

    pub fn create_instance(
        &self,
        descriptor: &SyncDescriptor<DynamicValue>,
        rest: Vec<DynamicValue>,
    ) -> Result<DynamicValue, String> {
        self.assert_not_disposed()?;
        let trace = Trace::trace_creation(self.enable_tracing, "createInstance");
        let mut args = descriptor.static_arguments.clone();
        args.extend(rest);
        let result = self.create_instance_inner(descriptor, args, trace.clone(), None);
        trace.stop();
        result
    }

    pub fn create_child(
        self: &Arc<Self>,
        services: Arc<ServiceCollection>,
        store: Option<DisposableStore>,
    ) -> Result<Arc<Self>, String> {
        self.assert_not_disposed()?;
        let child = Self::new(
            services,
            self.strict,
            Some(self.clone()),
            self.enable_tracing,
        );
        self.children.lock().insert(child.id, child.clone());
        let dispose_child = child.clone();
        *child.parent_ledger_entry.lock() = Some(self.ledger.register(
            move |_| dispose_child.dispose(),
            "child-instantiation",
        ));
        if let Some(store) = store {
            store.add(child.clone());
        }
        Ok(child)
    }

    pub fn dispose_async(self: &Arc<Self>) -> Shared<BoxFuture<'static, ()>> {
        let mut slot = self.dispose_promise.lock();
        if let Some(promise) = slot.as_ref() {
            return promise.clone();
        }
        let service = self.clone();
        let task = tokio::spawn(async move { service.dispose_core().await });
        let promise = async move {
            match task.await {
                Ok(()) => {}
                Err(_) => std::future::pending::<()>().await,
            }
        }
        .boxed()
        .shared();
        *slot = Some(promise.clone());
        promise
    }

    async fn dispose_core(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let children = self.children();
        let child_teardowns = children
            .into_iter()
            .map(|child| child.dispose_async())
            .collect::<Vec<_>>();
        self.children.lock().clear();
        let teardown = self.ledger.teardown("scope-close");
        self.services.dispose();
        self.cascade().dispose();
        let views = self
            .collection_views
            .lock()
            .drain()
            .map(|(_, view)| view)
            .collect::<Vec<_>>();
        for view in views {
            view.dispose();
        }
        for node in self.edge_nodes.lock().drain() {
            self.tree.graph.lock().remove_instance(node);
        }
        self.tree.graph.lock().remove_scope(self.id);
        self.children.lock().clear();
        if let Some(entry) = self.parent_ledger_entry.lock().take() {
            entry.release();
        }
        if let Some(parent) = &self.parent {
            parent.children.lock().shift_remove(&self.id);
        }
        CONTAINERS.lock().remove(&self.id);
        join_all(child_teardowns).await;
        if let Some(teardown) = teardown {
            teardown.await;
        }
    }

    fn create_instance_inner(
        &self,
        descriptor: &SyncDescriptor<DynamicValue>,
        mut args: Vec<DynamicValue>,
        trace: Trace,
        unit: Option<(Option<ServiceIdentifier<DynamicValue>>, Option<DynamicValue>)>,
    ) -> Result<DynamicValue, String> {
        let mut dependencies = DESCRIPTOR_DEPENDENCIES
            .lock()
            .get(&descriptor_ctor_id(descriptor))
            .cloned()
            .unwrap_or_default();
        dependencies.sort_by_key(|dependency| dependency.index);
        let mut service_args = Vec::new();
        for dependency in &dependencies {
            match dependency.kind {
                DependencyKind::Collection => {
                    let token = super::collection::collection::<DynamicValue>(
                        dependency.id.to_string().trim_start_matches("collection:"),
                        super::collection::CollectionOptions::default(),
                    );
                    service_args.push(Arc::new(self.arc().collection_view(token)) as DynamicValue);
                }
                DependencyKind::Ref => {
                    service_args.push(Arc::new(self.arc().live_ref(&dependency.id)) as DynamicValue);
                }
                DependencyKind::Instance => {
                    let service = self.get_or_create_service_instance(&dependency.id, trace.clone())?;
                    service_args.push(service.unwrap_or_else(|| Arc::new(()) as DynamicValue));
                }
            }
        }
        let first_service_arg_pos = dependencies.first().map_or(args.len(), |dependency| dependency.index);
        if args.len() != first_service_arg_pos {
            tracing::warn!(
                "[createInstance] First service dependency at position {} conflicts with {} static arguments",
                first_service_arg_pos + 1,
                args.len()
            );
            if args.len() < first_service_arg_pos {
                args.resize_with(first_service_arg_pos, || Arc::new(()) as DynamicValue);
            } else {
                args.truncate(first_service_arg_pos);
            }
        }
        args.extend(service_args);
        let recipe = DESCRIPTOR_RECIPES
            .lock()
            .get(&descriptor_ctor_id(descriptor))
            .cloned();
        let Some(recipe) = recipe.filter(is_service_recipe) else {
            return Ok((descriptor.ctor)(args));
        };
        let frame = ConstructionFrame {
            ctor: recipe.ctor_id(),
            config: unit.as_ref().and_then(|(_, config)| config.clone()),
            token: unit.and_then(|(id, _)| id),
            host: self.arc(),
        };
        push_construction_frame(frame.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (descriptor.ctor)(args)
        }));
        pop_construction_frame();
        let instance = match result {
            Ok(instance) => instance,
            Err(error) => std::panic::resume_unwind(error),
        };
        let unit_instance = Arc::downcast::<Arc<dyn UnitInternals>>(instance.clone())
            .map_err(|_| "service constructor did not return UnitInternals".to_string())?
            .as_ref()
            .clone();
        bind_service_unit(unit_instance, frame)?;
        Ok(instance)
    }

    fn get_or_create_service_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
        trace: Trace,
    ) -> Result<Option<DynamicValue>, String> {
        if !self.cascade_resolving.load(Ordering::SeqCst) {
            if self.cascade().is_in_flight(id) {
                return Err(CascadeConflictError::new(
                    id.to_string(),
                    "token is inside an in-flight cascade transaction",
                )
                .to_string());
            }
            if let Some(failure) = self.cascade().failure_of(id) {
                return Err(failure);
            }
        }
        let entry = self.get_service_instance_or_descriptor(id);
        if let Some(ServiceValue::Descriptor(descriptor)) = entry {
            let root = self.root();
            if root.in_progress.lock().contains(id) {
                let mut path = root.in_progress.lock().clone();
                path.push(id.clone());
                return Err(CyclicDependencyError::from_path(
                    path.iter().map(ToString::to_string).collect(),
                )
                .to_string());
            }
            return self
                .safe_create_and_cache_service_instance(id, descriptor, trace.branch(id, true))
                .map(Some);
        }
        trace.branch(id, false);
        Ok(entry.and_then(|entry| match entry {
            ServiceValue::Instance(instance) => Some(instance),
            _ => None,
        }))
    }

    fn safe_create_and_cache_service_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
        trace: Trace,
    ) -> Result<DynamicValue, String> {
        if !self.active_instantiations.lock().insert(id.clone()) {
            return Err(format!(
                "illegal state - RECURSIVELY instantiating service '{id}'"
            ));
        }
        let _guard = ActiveInstantiationGuard {
            service: self.arc(),
            id: id.clone(),
        };
        self.create_and_cache_service_instance(id, descriptor, trace)
    }

    fn create_and_cache_service_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
        trace: Trace,
    ) -> Result<DynamicValue, String> {
        #[derive(Clone)]
        struct Triple {
            id: ServiceIdentifier<DynamicValue>,
            descriptor: SyncDescriptor<DynamicValue>,
            trace: Trace,
        }
        let mut graph = Graph::<Triple>::new(|data| data.id.to_string());
        let mut cycle_count = 0;
        let mut stack = vec![Triple {
            id: id.clone(),
            descriptor,
            trace,
        }];
        let mut seen = HashSet::new();
        while let Some(item) = stack.pop() {
            if !seen.insert(item.id.to_string()) {
                continue;
            }
            graph.lookup_or_insert_node(item.clone());
            cycle_count += 1;
            if cycle_count > 1001 {
                return Err(CyclicDependencyError::from_graph(&graph).to_string());
            }
            for dependency in descriptor_dependencies(&item.descriptor)
                .into_iter()
                .filter(|dependency| dependency.kind == DependencyKind::Instance)
            {
                let instance_or_desc = self.get_service_instance_or_descriptor(&dependency.id);
                if instance_or_desc.is_none() {
                    self.throw_if_strict(
                        &format!(
                            "[createInstance] {} depends on {} which is NOT registered.",
                            item.id, dependency.id
                        ),
                        true,
                    )?;
                }
                if let Some(global) = &self.global_graph {
                    global.insert_edge(item.id.to_string(), dependency.id.to_string());
                }
                if let Some(ServiceValue::Descriptor(descriptor)) = instance_or_desc {
                    let next = Triple {
                        id: dependency.id.clone(),
                        descriptor,
                        trace: item.trace.branch(&dependency.id, true),
                    };
                    graph.insert_edge(item.clone(), next.clone());
                    stack.push(next);
                }
            }
        }
        loop {
            let roots = graph.roots();
            if roots.is_empty() {
                if !graph.is_empty() {
                    return Err(CyclicDependencyError::from_graph(&graph).to_string());
                }
                break;
            }
            for root in roots {
                let data = root.borrow().data.clone();
                if let Some(ServiceValue::Descriptor(_)) = self.get_service_instance_or_descriptor(&data.id) {
                    let instance = self.create_service_instance_with_owner(
                        &data.id,
                        &data.descriptor,
                        data.trace,
                    )?;
                    self.set_created_service_instance(&data.id, instance)?;
                }
                graph.remove_node(&data);
            }
        }
        Ok(self
            .get_service_instance_or_descriptor(id)
            .and_then(|value| match value {
                ServiceValue::Instance(instance) => Some(instance),
                _ => None,
            })
            .unwrap_or_else(|| Arc::new(()) as DynamicValue))
    }

    fn create_service_instance_with_owner(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
        descriptor: &SyncDescriptor<DynamicValue>,
        trace: Trace,
    ) -> Result<DynamicValue, String> {
        if matches!(self.services.get_value(id), Some(ServiceValue::Descriptor(_))) {
            self.create_service_instance(id, descriptor, trace)
        } else if let Some(parent) = &self.parent {
            parent.create_service_instance_with_owner(id, descriptor, trace)
        } else {
            Err("illegalState - creating UNKNOWN service instance".to_string())
        }
    }

    fn create_service_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
        descriptor: &SyncDescriptor<DynamicValue>,
        trace: Trace,
    ) -> Result<DynamicValue, String> {
        let root = self.root();
        root.in_progress.lock().push(id.clone());
        let _guard = InProgressGuard {
            service: root.clone(),
            id: id.clone(),
        };
        let result = self.create_instance_inner(
            descriptor,
            descriptor.static_arguments.clone(),
            trace,
            Some((Some(id.clone()), self.services.config_of(id))),
        );
        let instance = match result {
            Ok(instance) => instance,
            Err(error) => return Err(error),
        };
        let instance_id = dynamic_id(&instance);
        self.tree.graph.lock().add_instance(instance_id, self.id, id.clone());
        for dependency in descriptor_dependencies(descriptor) {
            match dependency.kind {
                DependencyKind::Collection => self.tree.graph.lock().add_edge(
                    instance_id,
                    ScopedToken {
                        scope: self.id,
                        token: dependency.id,
                    },
                    DependencyEdgeKind::Collection,
                ),
                DependencyKind::Ref => {}
                DependencyKind::Instance => {
                    if let Some(owner) = self.owner_of(&dependency.id) {
                        self.tree.graph.lock().add_edge(
                            instance_id,
                            ScopedToken {
                                scope: owner.id,
                                token: dependency.id,
                            },
                            DependencyEdgeKind::Instance,
                        );
                    }
                }
            }
        }
        let service = self.arc();
        let callback_instance = instance.clone();
        let callback_id = id.clone();
        let entry = self.ledger.register(
            move |_| {
                service.instance_entries.lock().remove(&instance_id);
                service.tree.graph.lock().remove_instance(instance_id);
                if let Ok(disposable) = Arc::downcast::<Arc<dyn IDisposable>>(callback_instance.clone()) {
                    disposable.dispose();
                }
            },
            &format!("service:{callback_id}"),
        );
        self.instance_entries.lock().insert(instance_id, entry);
        self.cascade().observed_materialization(id);
        Ok(instance)
    }

    fn set_created_service_instance(
        &self,
        id: &ServiceIdentifier<DynamicValue>,
        instance: DynamicValue,
    ) -> Result<(), String> {
        if matches!(self.services.get_value(id), Some(ServiceValue::Descriptor(_))) {
            self.services.materialize(id, instance);
            Ok(())
        } else if let Some(parent) = &self.parent {
            parent.set_created_service_instance(id, instance)
        } else {
            Err(format!(
                "illegal state - setting UNKNOWN service instance '{id}'"
            ))
        }
    }

    fn get_service_instance_or_descriptor(&self, id: &ServiceIdentifier<DynamicValue>) -> Option<ServiceValue> {
        self.services
            .get_value(id)
            .or_else(|| self.parent.as_ref()?.get_service_instance_or_descriptor(id))
    }

    fn throw_if_strict(&self, message: &str, print_warning: bool) -> Result<(), String> {
        if print_warning {
            tracing::warn!("{message}");
        }
        if self.strict {
            Err(message.to_string())
        } else {
            Ok(())
        }
    }

    fn root(&self) -> Arc<Self> {
        self.parent.as_ref().map_or_else(|| self.arc(), |parent| parent.root())
    }

    fn assert_not_disposed(&self) -> Result<(), String> {
        if self.disposed.load(Ordering::SeqCst) {
            Err(InstantiationServiceDisposedError.to_string())
        } else {
            Ok(())
        }
    }
}

fn descriptor_dependencies(descriptor: &SyncDescriptor<DynamicValue>) -> Vec<ServiceDependency> {
    DESCRIPTOR_DEPENDENCIES
        .lock()
        .get(&descriptor_ctor_id(descriptor))
        .cloned()
        .unwrap_or_default()
}

fn dynamic_id(value: &DynamicValue) -> ObjectId {
    Arc::as_ptr(value) as *const () as ObjectId
}

fn remove_in_progress(service: &InstantiationService, id: &ServiceIdentifier<DynamicValue>) {
    let mut in_progress = service.in_progress.lock();
    if let Some(index) = in_progress.iter().rposition(|current| current == id) {
        in_progress.remove(index);
    }
}

struct ActiveInstantiationGuard {
    service: Arc<InstantiationService>,
    id: ServiceIdentifier<DynamicValue>,
}

impl Drop for ActiveInstantiationGuard {
    fn drop(&mut self) {
        self.service.active_instantiations.lock().remove(&self.id);
    }
}

struct InProgressGuard {
    service: Arc<InstantiationService>,
    id: ServiceIdentifier<DynamicValue>,
}

impl Drop for InProgressGuard {
    fn drop(&mut self) {
        remove_in_progress(&self.service, &self.id);
    }
}

struct InvocationGuard {
    done: Arc<AtomicBool>,
    trace: Trace,
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        self.trace.stop();
    }
}

struct ServiceAccessor {
    service: Arc<InstantiationService>,
    done: Arc<AtomicBool>,
    trace: Trace,
}

impl ServicesAccessor for ServiceAccessor {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue {
        assert!(
            !self.done.load(Ordering::SeqCst),
            "service accessor is only valid during the invocation of its target method"
        );
        let result = self
            .service
            .get_or_create_service_instance(id, self.trace.clone())
            .unwrap_or_else(|error| panic!("{error}"));
        if result.is_none() {
            self.service
                .throw_if_strict(&format!("[invokeFunction] unknown service '{id}'"), false)
                .unwrap_or_else(|error| panic!("{error}"));
        }
        result.unwrap_or_else(|| Arc::new(()))
    }
}

struct ServiceProvideHandle {
    entry: LedgerEntry,
    core: Arc<dyn TokenProvideCore>,
}

impl ProvideHandle for ServiceProvideHandle {
    fn uid(&self) -> u64 {
        self.core.uid()
    }
}

impl IDisposable for ServiceProvideHandle {
    fn dispose(&self) {
        if let Some(future) = self.entry.dispose("scope-close") {
            tokio::spawn(future);
        }
    }
}

struct FixedProvideCore {
    service: Arc<InstantiationService>,
    id: ServiceIdentifier<DynamicValue>,
    uid: u64,
}

impl TokenProvideCore for FixedProvideCore {
    fn uid(&self) -> u64 {
        self.uid
    }

    fn dispose(&self) -> BoxFuture<'static, Result<(), String>> {
        if self.service.services.uid_of(&self.id) == Some(self.uid) {
            self.service.unprovide_core(self.id.clone())
        } else {
            async { Ok(()) }.boxed()
        }
    }

    fn release(&self) {}
}

struct CascadeProvideCore {
    service: Arc<InstantiationService>,
    id: ServiceIdentifier<DynamicValue>,
    applied_uid: Arc<Mutex<Option<u64>>>,
    subscription: Mutex<Option<Subscription>>,
}

impl CascadeProvideCore {
    fn release_subscription(&self) {
        if let Some(subscription) = self.subscription.lock().take() {
            subscription.dispose();
        }
    }
}

impl TokenProvideCore for CascadeProvideCore {
    fn uid(&self) -> u64 {
        self.applied_uid.lock().unwrap_or_else(|| {
            panic!(
                "provide of '{}' has not been applied yet (cascade in flight)",
                self.id
            )
        })
    }

    fn dispose(&self) -> BoxFuture<'static, Result<(), String>> {
        self.release_subscription();
        if self.applied_uid.lock().is_some()
            && self.service.services.uid_of(&self.id) == *self.applied_uid.lock()
        {
            self.service.unprovide_core(self.id.clone())
        } else {
            async { Ok(()) }.boxed()
        }
    }

    fn release(&self) {
        self.release_subscription();
    }
}

struct ServiceCascadeHost {
    service: Arc<InstantiationService>,
}

impl CascadeHost for ServiceCascadeHost {
    fn is_registered(&self, token: &ServiceIdentifier<DynamicValue>) -> bool {
        self.service.get_service_instance_or_descriptor(token).is_some()
    }

    fn owner_scope_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<ObjectId> {
        self.service.owner_of(token).map(|owner| owner.id)
    }

    fn is_materialized(&self, token: &ServiceIdentifier<DynamicValue>) -> bool {
        matches!(
            self.service.services.get_value(token),
            Some(ServiceValue::Instance(_))
        )
    }

    fn materialize(&self, token: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String> {
        self.service.cascade_resolving.store(true, Ordering::SeqCst);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.service.get_or_create_service_instance(
                token,
                Trace::trace_creation(false, "CascadeEngine"),
            )
        }));
        self.service.cascade_resolving.store(false, Ordering::SeqCst);
        match result {
            Ok(result) => result.map(|value| value.unwrap_or_else(|| Arc::new(()) as DynamicValue)),
            Err(error) => std::panic::resume_unwind(error),
        }
    }

    fn retire(&self, token: ServiceIdentifier<DynamicValue>) -> BoxFuture<'static, Result<(), String>> {
        self.service.retire_unit(token)
    }

    fn apply_provide(
        &self,
        token: ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
        config: Option<DynamicValue>,
    ) -> Result<u64, String> {
        self.service.services.set_value(
            token.clone(),
            ServiceValue::Descriptor(descriptor),
            Some(ServiceCollectionOptions { config }),
        );
        self.service
            .services
            .uid_of(&token)
            .ok_or_else(|| format!("provide of '{token}' did not create an entry"))
    }

    fn apply_provide_instance(
        &self,
        token: ServiceIdentifier<DynamicValue>,
        instance: Option<DynamicValue>,
        config: Option<DynamicValue>,
    ) -> Result<u64, String> {
        self.service.services.set_value(
            token.clone(),
            ServiceValue::Instance(instance.unwrap_or_else(|| Arc::new(()))),
            Some(ServiceCollectionOptions { config }),
        );
        self.service
            .services
            .uid_of(&token)
            .ok_or_else(|| format!("provide of '{token}' did not create an entry"))
    }

    fn apply_unprovide(&self, token: &ServiceIdentifier<DynamicValue>) -> Result<(), String> {
        self.service.services.delete(token);
        Ok(())
    }

    fn recipe_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<SyncDescriptor<DynamicValue>> {
        let entry = self.service.services.entry(token)?;
        match entry.value {
            ServiceValue::Descriptor(descriptor) => Some(descriptor),
            _ => entry.recipe,
        }
    }

    fn dependencies_of(
        &self,
        recipe: &SyncDescriptor<DynamicValue>,
    ) -> Vec<ServiceIdentifier<DynamicValue>> {
        descriptor_dependencies(recipe)
            .into_iter()
            .filter(|dependency| dependency.kind == DependencyKind::Instance)
            .map(|dependency| dependency.id)
            .collect()
    }
}

impl CascadeScopeHandle for InstantiationService {
    fn cascade(&self) -> Arc<CascadeEngine> {
        InstantiationService::cascade(self)
    }

    fn cascade_disposed(&self) -> bool {
        InstantiationService::cascade_disposed(self)
    }

    fn cascade_depth(&self) -> usize {
        InstantiationService::cascade_depth(self)
    }

    fn scope_id(&self) -> ObjectId {
        self.id
    }
}

impl FiberHost for InstantiationService {
    fn mint_uid(&self) -> u64 {
        self.root().next_unit_uid.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn provide_token(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
        activation: &str,
        config: Option<DynamicValue>,
    ) -> Result<Arc<dyn TokenProvideCore>, String> {
        self.arc().provide_core(
            id,
            ServiceValue::Descriptor(descriptor),
            Some(ProvideOptions {
                activation: Some(if activation == "ondemand" {
                    UnitActivation::OnDemand
                } else {
                    UnitActivation::Eager
                }),
                config,
            }),
        )
    }

    fn provide_token_instance(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        instance: DynamicValue,
    ) -> Result<Arc<dyn TokenProvideCore>, String> {
        self.arc()
            .provide_core(id, ServiceValue::Instance(instance), None)
    }

    fn token_state(&self, id: &ServiceIdentifier<DynamicValue>) -> Option<String> {
        let owner = self.owner_of(id).unwrap_or_else(|| self.arc());
        owner
            .cascade()
            .state_of(id)
            .map(|state| state.to_string())
    }

    fn update_token(
        &self,
        id: ServiceIdentifier<DynamicValue>,
        config: Option<DynamicValue>,
        has_config: bool,
    ) -> BoxFuture<'static, Result<(), String>> {
        let Some(owner) = self.owner_of(&id) else {
            return async move {
                Err(format!("update of unregistered token '{id}'"))
            }
            .boxed();
        };
        if has_config {
            owner.services.set_config(&id, config.unwrap_or_else(|| Arc::new(())));
        }
        owner.cascade().update(id, None)
    }

    fn resolve_token_when_available(
        &self,
        id: ServiceIdentifier<DynamicValue>,
    ) -> BoxFuture<'static, Result<DynamicValue, String>> {
        self.cascade().resolve_when_available(id, None)
    }

    fn resolve_instance(&self, id: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String> {
        self.get_or_create_service_instance(
            id,
            Trace::trace_invocation(self.enable_tracing, "resolveInstance"),
        )
        .map(|value| value.unwrap_or_else(|| Arc::new(()) as DynamicValue))
    }

    fn materialized_instance(&self, id: &ServiceIdentifier<DynamicValue>) -> Option<DynamicValue> {
        self.materialized_instance_of(id)
    }

    fn live_ref(&self, id: &ServiceIdentifier<DynamicValue>) -> LiveRef<DynamicValue> {
        self.arc().live_ref(id)
    }

    fn record_instance_edge(&self, node: Option<ObjectId>, id: &ServiceIdentifier<DynamicValue>) {
        let Some(node) = node else {
            return;
        };
        if let Some(owner) = self.owner_of(id) {
            self.edge_nodes.lock().insert(node);
            self.tree.graph.lock().add_edge(
                node,
                ScopedToken {
                    scope: owner.id,
                    token: id.clone(),
                },
                DependencyEdgeKind::Instance,
            );
        }
    }

    fn collection_view(
        &self,
        token: CollectionToken<DynamicValue>,
    ) -> Arc<CollectionViewImpl<DynamicValue>> {
        self.arc().collection_view(token)
    }

    fn add_collection_record(
        &self,
        token: CollectionToken<DynamicValue>,
        provider_name: String,
        provider_book: Ledger,
        value: DynamicValue,
    ) -> Result<Box<dyn FnOnce() + Send>, String> {
        self.collection_store.add_record(
            &token,
            self.id,
            provider_name,
            self.scope_path(),
            provider_book,
            value,
        )
    }

    fn construct_service(
        &self,
        recipe: super::fiber::ServiceClassRecipe,
        config: Option<DynamicValue>,
    ) -> Result<ConstructedService, String> {
        register_service_recipe(&recipe);
        let value = self.create_instance_inner(
            &recipe.descriptor,
            Vec::new(),
            Trace::trace_creation(self.enable_tracing, "constructService"),
            Some((None, config)),
        )?;
        let disposable = Arc::downcast::<Arc<dyn UnitInternals>>(value.clone())
            .ok()
            .map(|instance| instance.as_ref().clone() as DisposableRef);
        Ok(ConstructedService { value, disposable })
    }
}

impl IInstantiationService for InstantiationService {
    fn cascade(&self) -> Arc<CascadeEngine> {
        InstantiationService::cascade(self)
    }

    fn invoke_function<R>(&self, callback: impl FnOnce(&dyn ServicesAccessor) -> R) -> R {
        InstantiationService::invoke_function(self, callback)
    }

    fn create_instance<T>(&self, descriptor: &SyncDescriptor0<T>) -> T {
        (descriptor.ctor)()
    }

    fn create_child(
        &self,
        services: ServiceCollection,
        store: Option<DisposableStore>,
    ) -> Arc<dyn IInstantiationService> {
        InstantiationService::create_child(self, Arc::new(services), store)
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn provide<T>(
        &self,
        id: ServiceIdentifier<T>,
        instance_or_descriptor: DynamicValue,
        options: Option<ProvideOptions>,
    ) -> Arc<dyn ProvideHandle> {
        self.arc()
            .provide(
                super::instantiation::refine_service_decorator(&id),
                ServiceValue::Instance(instance_or_descriptor),
                options,
            )
            .unwrap_or_else(|error| panic!("{error}"))
    }

    fn provide_all(&self, entries: Vec<ProvideAllEntry>) {
        InstantiationService::provide_all(self, entries)
            .unwrap_or_else(|error| panic!("{error}"));
    }

    fn unprovide<T>(&self, id: &ServiceIdentifier<T>) {
        InstantiationService::unprovide(self, super::instantiation::refine_service_decorator(id));
    }

    fn dispose(&self) {
        IDisposable::dispose(self);
    }

    fn dispose_async(&self) -> impl Future<Output = ()> + Send {
        self.arc().dispose_async()
    }
}

impl IDisposable for InstantiationService {
    fn dispose(&self) {
        let future = self.arc().dispose_async();
        tokio::spawn(future);
    }
}
