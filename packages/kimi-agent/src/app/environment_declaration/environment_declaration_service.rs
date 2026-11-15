use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use kimi_core::primitives::errors::Error2;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::Serialize;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ScopeActivation, ServiceDependency, refine_service_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable};
use crate::_base::di::scope::register_scoped_service;
use crate::_base::event::AsyncEmitter;
use crate::_base::log::log::{I_LOG_SERVICE, ILogService};
use crate::_base::utils::abort::{AbortController, AbortSignal};
use crate::app::config::config::{I_CONFIG_SERVICE, IConfigService};
use crate::app::config::errors::CONFIG_INVALID;
use crate::app::scopes::LifecycleScope;
use crate::remote::declaration::configSection::{
    ENVIRONMENTS_SECTION, resolve_workspace_environment_declarations,
};
use crate::remote::declaration::remote_environment_declaration::{
    EnvironmentDeclarationSet, RemoteEnvironmentEntry,
};
use crate::wire::record::WireAsyncError;

use super::environment_declaration::{
    EnvironmentDeclarationChange, EnvironmentDeclarationChangeListener,
    I_ENVIRONMENT_DECLARATION_SERVICE, IEnvironmentDeclarationService,
};

static NO_ABORT: Lazy<AbortSignal> = Lazy::new(|| AbortController::new().signal());

#[derive(Clone, Debug)]
struct SharedReloadError(Arc<String>);

impl SharedReloadError {
    fn new(error: WireAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WireAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedReloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0.as_str())
    }
}

impl Error for SharedReloadError {}

type ReloadFuture = Shared<BoxFuture<'static, Result<(), SharedReloadError>>>;

struct EnvironmentDeclarationState {
    current: EnvironmentDeclarationSet,
    loading: Option<ReloadFuture>,
    catalog_key: Option<String>,
    registration_errors: HashMap<String, String>,
    disposed: bool,
}

struct EnvironmentDeclarationInner {
    config: Arc<dyn IConfigService>,
    log: Arc<dyn ILogService>,
    state: Mutex<EnvironmentDeclarationState>,
    subscriptions: DisposableStore,
    changes: AsyncEmitter<EnvironmentDeclarationChange>,
}

#[derive(Clone)]
pub struct EnvironmentDeclarationService {
    inner: Arc<EnvironmentDeclarationInner>,
}

#[derive(Serialize)]
struct CatalogEntry<'a> {
    id: &'a str,
    entry: &'a RemoteEnvironmentEntry,
}

impl EnvironmentDeclarationService {
    pub fn new(config: Arc<dyn IConfigService>, log: Arc<dyn ILogService>) -> Self {
        let inner = Arc::new(EnvironmentDeclarationInner {
            config: config.clone(),
            log,
            state: Mutex::new(EnvironmentDeclarationState {
                current: EnvironmentDeclarationSet::default(),
                loading: None,
                catalog_key: None,
                registration_errors: HashMap::new(),
                disposed: false,
            }),
            subscriptions: DisposableStore::new(),
            changes: AsyncEmitter::new(),
        });
        let weak = Arc::downgrade(&inner);
        let subscription = config.on_did_section_change(Box::new(move |event| {
            if event.domain != ENVIRONMENTS_SECTION {
                return;
            }
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.state.lock().disposed {
                return;
            }
            let log = inner.log.clone();
            tokio::spawn(async move {
                if let Err(error) = reload(&inner).await {
                    log.warn(
                        "remote environment declaration reload failed",
                        Some(serde_json::json!({ "error": error.to_string() })),
                    );
                }
            });
        }));
        inner.subscriptions.add(Arc::from(subscription));
        Self { inner }
    }

    pub fn dispose(&self) {
        self.inner.state.lock().disposed = true;
        self.inner.subscriptions.dispose();
        self.inner.changes.dispose();
    }
}

fn reload(inner: &Arc<EnvironmentDeclarationInner>) -> ReloadFuture {
    let mut state = inner.state.lock();
    let previous = state.loading.clone();
    let task_inner = inner.clone();
    let run = async move {
        if let Some(previous) = previous {
            let _ = previous.await;
        }
        if task_inner.state.lock().disposed {
            return Ok(());
        }
        let current = resolve_workspace_environment_declarations(task_inner.config.as_ref())
            .await
            .map_err(SharedReloadError::new)?;
        let (key, entries) = {
            let mut state = task_inner.state.lock();
            state.current = current;
            let current_entries = &state.current.entries;
            state
                .registration_errors
                .retain(|id, _| current_entries.iter().any(|entry| &entry.id == id));
            let mut sorted = state.current.entries.iter().collect::<Vec<_>>();
            sorted.sort_by(|a, b| a.id.cmp(&b.id));
            let sorted = sorted
                .into_iter()
                .map(|entry| CatalogEntry {
                    id: &entry.id,
                    entry: &entry.entry,
                })
                .collect::<Vec<_>>();
            let key = serde_json::to_string(&sorted).unwrap();
            if state.catalog_key.as_deref() == Some(key.as_str()) {
                return Ok(());
            }
            (key, state.current.entries.clone())
        };
        task_inner
            .changes
            .fire_async(entries, NO_ABORT.clone())
            .await;
        let mut state = task_inner.state.lock();
        state.catalog_key = if state.registration_errors.is_empty() {
            Some(key)
        } else {
            None
        };
        Ok(())
    }
    .boxed()
    .shared();
    state.loading = Some(run.clone());
    run
}

#[async_trait]
impl IEnvironmentDeclarationService for EnvironmentDeclarationService {
    fn on_did_change(
        &self,
        listener: EnvironmentDeclarationChangeListener,
    ) -> DisposableRef {
        Arc::new(self.inner.changes.event().subscribe_arc(listener))
    }

    async fn declarations(&self) -> Result<EnvironmentDeclarationSet, WireAsyncError> {
        reload(&self.inner)
            .await
            .map_err(SharedReloadError::boxed)?;
        Ok(self.inner.state.lock().current.clone())
    }

    fn registration_error(&self, environment_id: &str) -> Option<WireAsyncError> {
        self.inner
            .state
            .lock()
            .registration_errors
            .get(environment_id)
            .map(|message| Box::new(Error2::new(CONFIG_INVALID, message.clone())) as WireAsyncError)
    }

    fn report_registration(&self, environment_id: &str, error: Option<String>) {
        let mut state = self.inner.state.lock();
        let Some(error) = error else {
            state.registration_errors.remove(environment_id);
            return;
        };
        state.registration_errors.insert(
            environment_id.to_string(),
            format!("environment \"{environment_id}\" could not be registered: {error}"),
        );
        state.catalog_key = None;
    }
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("environment declaration dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static ENVIRONMENT_DECLARATION_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IEnvironmentDeclarationService> =
            Arc::new(EnvironmentDeclarationService::new(
                dependency::<dyn IConfigService>(&args, 0),
                dependency::<dyn ILogService>(&args, 1),
            ));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![
            ServiceDependency {
                id: I_CONFIG_SERVICE.clone(),
                index: 0,
                kind: DependencyKind::Instance,
            },
            ServiceDependency {
                id: refine_service_decorator(&I_LOG_SERVICE),
                index: 1,
                kind: DependencyKind::Instance,
            },
        ],
    );
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_ENVIRONMENT_DECLARATION_SERVICE.clone(),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "environmentDeclaration",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_environment_declaration_service_registered() -> Result<(), String> {
    Lazy::force(&ENVIRONMENT_DECLARATION_SERVICE_REGISTRATION).clone()
}
