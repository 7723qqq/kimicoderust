use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use futures_util::future::{BoxFuture, FutureExt};
use once_cell::sync::Lazy;
use parking_lot::Mutex;

use super::cascade_engine::UnitActivation;
use super::descriptors::{DynamicValue, SyncDescriptor};
use super::instantiation::{
    ProvideAllEntry, ProvideOptions, ServiceIdentifier, ServicesAccessor,
    refine_service_decorator,
};
pub use super::instantiation::ScopeActivation;
use super::instantiation_service::InstantiationService;
use super::lifecycle::{DisposableStore, IDisposable};
use super::scope_units::watch_scope_units;
use super::service_collection::{ServiceCollection, ServiceValue};
use crate::_base::errors::errors::BugIndicatingError;
use crate::_base::lifecycle::ledger::{Ledger, LedgerEntry};

pub type ScopeKind = String;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleScope {
    App,
    Session,
    Agent,
}

impl LifecycleScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Session => "session",
            Self::Agent => "agent",
        }
    }
}

static SCOPE_TOPOLOGY: Lazy<Mutex<Option<Vec<String>>>> = Lazy::new(|| Mutex::new(None));

pub fn set_scope_topology(kinds: Vec<String>) -> Result<(), BugIndicatingError> {
    let mut topology = SCOPE_TOPOLOGY.lock();
    if let Some(existing) = topology.as_ref() {
        if existing == &kinds {
            return Ok(());
        }
        return Err(BugIndicatingError::new(format!(
            "scope topology already declared as [{}]; cannot redeclare as [{}]",
            existing.join(", "),
            kinds.join(", ")
        )));
    }
    *topology = Some(kinds);
    Ok(())
}

#[derive(Clone)]
pub struct ScopedEntry {
    pub scope: ScopeKind,
    pub id: ServiceIdentifier<DynamicValue>,
    pub descriptor: SyncDescriptor<DynamicValue>,
    pub domain: String,
    pub activation: ScopeActivation,
}

static SCOPED_REGISTRY: Lazy<Mutex<Vec<ScopedEntry>>> = Lazy::new(|| Mutex::new(Vec::new()));

fn find_scoped_entry_index(
    registry: &[ScopedEntry],
    scope: &str,
    id: &ServiceIdentifier<DynamicValue>,
) -> Option<usize> {
    registry
        .iter()
        .position(|entry| entry.scope == scope && &entry.id == id)
}

pub fn register_scoped_service(
    scope: ScopeKind,
    id: ServiceIdentifier<DynamicValue>,
    descriptor: SyncDescriptor<DynamicValue>,
    activation: ScopeActivation,
    domain: impl Into<String>,
) -> Result<(), BugIndicatingError> {
    let domain = domain.into();
    let mut registry = SCOPED_REGISTRY.lock();
    if let Some(existing) = find_scoped_entry_index(&registry, &scope, &id) {
        return Err(BugIndicatingError::new(format!(
            "duplicate scoped service registration for '{}' in scope '{}' (registered domain '{}', attempted domain '{}'); use overrideScopedService for intentional replacement",
            id,
            scope,
            registry[existing].domain,
            domain
        )));
    }
    registry.push(ScopedEntry {
        scope,
        id,
        descriptor,
        domain,
        activation,
    });
    Ok(())
}

pub fn override_scoped_service(
    scope: ScopeKind,
    id: ServiceIdentifier<DynamicValue>,
    descriptor: SyncDescriptor<DynamicValue>,
    activation: ScopeActivation,
    domain: impl Into<String>,
) -> Result<(), BugIndicatingError> {
    let domain = domain.into();
    let mut registry = SCOPED_REGISTRY.lock();
    let Some(index) = find_scoped_entry_index(&registry, &scope, &id) else {
        return Err(BugIndicatingError::new(format!(
            "overrideScopedService found no registration for '{}' in scope '{}' (domain '{}'); use registerScopedService for the initial registration",
            id, scope, domain
        )));
    };
    registry[index] = ScopedEntry {
        scope,
        id,
        descriptor,
        domain,
        activation,
    };
    Ok(())
}

pub fn get_scoped_service_descriptors(scope: &str) -> Vec<ScopedEntry> {
    SCOPED_REGISTRY
        .lock()
        .iter()
        .filter(|entry| entry.scope == scope)
        .cloned()
        .collect()
}

pub fn clear_scoped_registry_for_tests() {
    SCOPED_REGISTRY.lock().clear();
}

pub type ScopeSeed = Vec<(ServiceIdentifier<DynamicValue>, ServiceValue)>;

/// Build a single-entry seed from an explicit service identifier and an
/// already-constructed instance. Mirrors the v2 `[[IX, value]]` seed tuple:
/// DI lookup matches on the identifier name, not the trait type. Callers
/// wrap the instance the same way a descriptor constructor produces it
/// (`Arc::new(instance) as DynamicValue`) so constructor dependencies can
/// downcast it with the shared `dependency::<dyn T>` helper.
pub fn seed_instance<U>(
    id: &ServiceIdentifier<U>,
    instance: DynamicValue,
) -> ScopeSeed {
    vec![(refine_service_decorator(id), ServiceValue::Instance(instance))]
}
pub type ConfigureContainer = Arc<
    dyn Fn(Arc<InstantiationService>) -> Result<(), String> + Send + Sync,
>;

#[derive(Clone, Default)]
pub struct ScopeOptions {
    pub id: Option<String>,
    pub seeds: Option<ScopeSeed>,
    pub configure_container: Option<ConfigureContainer>,
}

pub trait IScopeHandle: Send + Sync {
    fn id(&self) -> &str;
    fn kind(&self) -> &str;
    fn accessor(&self) -> Arc<dyn ServicesAccessor>;
    fn dispose(&self) -> BoxFuture<'static, ()>;
}

pub type IAppScopeHandle = dyn IScopeHandle;
pub type ISessionScopeHandle = dyn IScopeHandle;
pub type IAgentScopeHandle = dyn IScopeHandle;

fn build_collection(seeds: Option<ScopeSeed>) -> Arc<ServiceCollection> {
    let collection = Arc::new(ServiceCollection::default());
    if let Some(seeds) = seeds {
        for (id, value) in seeds {
            collection.set_value(id, value, None);
        }
    }
    collection
}

fn provide_scope_services(
    instantiation: Arc<InstantiationService>,
    kind: &str,
    collection: &ServiceCollection,
) -> Result<(), String> {
    let mut entries = Vec::new();
    for entry in SCOPED_REGISTRY.lock().iter() {
        if entry.scope != kind || collection.get_value(&entry.id).is_some() {
            continue;
        }
        entries.push(ProvideAllEntry {
            id: entry.id.clone(),
            descriptor: entry.descriptor.clone(),
            options: Some(ProvideOptions {
                activation: Some(
                    if entry.activation == ScopeActivation::OnDemand {
                        UnitActivation::OnDemand
                    } else {
                        UnitActivation::Eager
                    },
                ),
                config: None,
            }),
        });
    }
    instantiation.provide_all(entries)
}

struct ScopeAccessor {
    instantiation: Arc<InstantiationService>,
}

impl ServicesAccessor for ScopeAccessor {
    fn get_dynamic(&self, id: &ServiceIdentifier<DynamicValue>) -> DynamicValue {
        let id = id.clone();
        self.instantiation.invoke_function(|accessor| {
            accessor.get_dynamic(&id)
        })
    }
}

struct InstantiationScopeHandle {
    id: String,
    kind: ScopeKind,
    instantiation: Arc<InstantiationService>,
}

impl IScopeHandle for InstantiationScopeHandle {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> &str {
        &self.kind
    }

    fn accessor(&self) -> Arc<dyn ServicesAccessor> {
        Arc::new(ScopeAccessor {
            instantiation: self.instantiation.clone(),
        })
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        self.instantiation.dispose_async().boxed()
    }
}

pub fn create_scoped_child_handle(
    parent: Arc<InstantiationService>,
    kind: ScopeKind,
    id: String,
    options: ScopeOptions,
) -> Result<Arc<dyn IScopeHandle>, String> {
    let collection = build_collection(options.seeds);
    let child = parent.create_child(collection.clone(), None)?;
    child.set_debug_label(id.clone());
    let engine = child.cascade();
    engine.suspend_activation();
    let result = (|| {
        watch_scope_units(child.clone(), kind.clone());
        if let Some(configure) = options.configure_container {
            configure(child.clone())?;
        }
        provide_scope_services(child.clone(), &kind, &collection)
    })();
    if let Err(error) = result {
        child.dispose();
        engine.resume_activation();
        return Err(error);
    }
    engine.resume_activation();
    Ok(Arc::new(InstantiationScopeHandle {
        id,
        kind,
        instantiation: child,
    }))
}

pub struct Scope {
    this_ref: once_cell::sync::OnceCell<Weak<Scope>>,
    pub id: String,
    pub kind: ScopeKind,
    pub instantiation: Arc<InstantiationService>,
    parent: Option<Weak<Scope>>,
    children: Mutex<HashMap<String, Arc<Scope>>>,
    store: DisposableStore,
    ledger: Ledger,
    ledger_entry: Mutex<Option<LedgerEntry>>,
    disposed: AtomicBool,
}

impl Scope {
    fn new(
        id: String,
        kind: ScopeKind,
        instantiation: Arc<InstantiationService>,
        parent: Option<Weak<Scope>>,
    ) -> Arc<Self> {
        let ledger = Ledger::new(&format!("scope:{id}"));
        let dispose_instantiation = instantiation.clone();
        ledger.register(
            move |_| dispose_instantiation.dispose(),
            "instantiation",
        );
        let store = DisposableStore::new();
        let dispose_store = store.clone();
        ledger.register(move |_| dispose_store.dispose(), "store");
        let scope = Arc::new(Self {
            this_ref: once_cell::sync::OnceCell::new(),
            id,
            kind,
            instantiation,
            parent,
            children: Mutex::new(HashMap::new()),
            store,
            ledger,
            ledger_entry: Mutex::new(None),
            disposed: AtomicBool::new(false),
        });
        let _ = scope.this_ref.set(Arc::downgrade(&scope));
        scope
    }

    pub fn create_app(options: ScopeOptions) -> Result<Arc<Self>, String> {
        let kind = "app".to_string();
        let collection = build_collection(options.seeds.clone());
        let instantiation = InstantiationService::new(collection.clone(), true, None, false);
        instantiation.set_debug_label(options.id.clone().unwrap_or_else(|| "app".to_string()));
        let engine = instantiation.cascade();
        engine.suspend_activation();
        let result = (|| {
            watch_scope_units(instantiation.clone(), kind.clone());
            if let Some(configure) = options.configure_container.clone() {
                configure(instantiation.clone())?;
            }
            provide_scope_services(instantiation.clone(), &kind, &collection)
        })();
        if let Err(error) = result {
            instantiation.dispose();
            engine.resume_activation();
            return Err(error);
        }
        engine.resume_activation();
        Ok(Self::new(
            options.id.unwrap_or_else(|| "app".to_string()),
            kind,
            instantiation,
            None,
        ))
    }

    fn assert_not_disposed(&self) -> Result<(), String> {
        if self.disposed.load(Ordering::SeqCst) {
            Err(format!("Scope '{}' has been disposed", self.id))
        } else {
            Ok(())
        }
    }

    pub fn create_child(
        self: &Arc<Self>,
        kind: ScopeKind,
        id: String,
        options: ScopeOptions,
    ) -> Result<Arc<Self>, String> {
        self.assert_not_disposed()?;
        if let Some(topology) = SCOPE_TOPOLOGY.lock().as_ref() {
            let parent_index = topology.iter().position(|kind| kind == &self.kind);
            let child_index = topology.iter().position(|candidate| candidate == &kind);
            if parent_index.is_none()
                || child_index.is_none()
                || child_index <= parent_index
            {
                return Err(format!(
                    "child scope kind '{}' must be greater than parent kind '{}' in the declared scope topology",
                    kind, self.kind
                ));
            }
        }
        if self.children.lock().contains_key(&id) {
            return Err(format!(
                "Scope '{}' already has a child with id '{}'",
                self.id, id
            ));
        }
        let collection = build_collection(options.seeds.clone());
        let child_instantiation = self
            .instantiation
            .create_child(collection.clone(), None)?;
        child_instantiation.set_debug_label(id.clone());
        let engine = child_instantiation.cascade();
        engine.suspend_activation();
        let result = (|| {
            watch_scope_units(child_instantiation.clone(), kind.clone());
            if let Some(configure) = options.configure_container {
                configure(child_instantiation.clone())?;
            }
            provide_scope_services(child_instantiation.clone(), &kind, &collection)
        })();
        if let Err(error) = result {
            child_instantiation.dispose();
            engine.resume_activation();
            return Err(error);
        }
        engine.resume_activation();
        let child = Self::new(
            id.clone(),
            kind,
            child_instantiation,
            Some(Arc::downgrade(self)),
        );
        self.children.lock().insert(id.clone(), child.clone());
        let dispose_child = child.clone();
        *child.ledger_entry.lock() = Some(self.ledger.register(
            move |_| dispose_child.dispose(),
            &format!("scope:{id}"),
        ));
        Ok(child)
    }

    pub fn ledger(&self) -> Ledger {
        self.ledger.clone()
    }

    pub fn accessor(&self) -> Arc<dyn ServicesAccessor> {
        Arc::new(ScopeAccessor {
            instantiation: self.instantiation.clone(),
        })
    }

    pub fn to_handle(self: &Arc<Self>) -> Arc<dyn IScopeHandle> {
        Arc::new(OwnedScopeHandle { scope: self.clone() })
    }

    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(entry) = self.ledger_entry.lock().take() {
            entry.release();
        }
        if let Some(future) = self.ledger.teardown("scope-close") {
            tokio::spawn(future);
        }
        self.children.lock().clear();
        if let Some(parent) = self.parent.as_ref().and_then(Weak::upgrade) {
            parent.children.lock().remove(&self.id);
        }
    }
}

struct OwnedScopeHandle {
    scope: Arc<Scope>,
}

impl IScopeHandle for OwnedScopeHandle {
    fn id(&self) -> &str {
        &self.scope.id
    }

    fn kind(&self) -> &str {
        &self.scope.kind
    }

    fn accessor(&self) -> Arc<dyn ServicesAccessor> {
        self.scope.accessor()
    }

    fn dispose(&self) -> BoxFuture<'static, ()> {
        let scope = self.scope.clone();
        async move { scope.dispose() }.boxed()
    }
}

impl IDisposable for Scope {
    fn dispose(&self) {
        Scope::dispose(self);
    }
}

pub fn create_app_scope(options: ScopeOptions) -> Result<Arc<Scope>, String> {
    Scope::create_app(options)
}
