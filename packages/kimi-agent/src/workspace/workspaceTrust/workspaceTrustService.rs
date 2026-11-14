use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::safely_call_listener;
use crate::app::config::config::IConfigService;
use crate::state::state::{StateDefinition, define_state};
use crate::workspace::state::workspace_state::IWorkspaceStateService;
use crate::workspace::workspace_dirs::workspace_dirs::IWorkspaceDirs;

use super::config_section::workspace_trust_required;
use super::workspace_trust::{
    IWorkspaceTrust, WorkspaceTrustAsyncError, WorkspaceTrustChange,
};
use super::workspace_trust_store::{IWorkspaceTrustEntry, IWorkspaceTrustStore};

pub use super::trust_env::{TRUST_WORKSPACE_ENV, trust_workspace_env_trusted};

pub static WORKSPACE_TRUST_TRUSTED_KEY: LazyLock<StateDefinition<bool>> =
    LazyLock::new(|| define_state("workspaceTrust.trusted", || false));

#[derive(Debug, Clone)]
struct ServiceSharedError(Arc<String>);

impl ServiceSharedError {
    fn new(error: WorkspaceTrustAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WorkspaceTrustAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for ServiceSharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for ServiceSharedError {}

type SharedServiceFuture = Shared<BoxFuture<'static, Result<(), ServiceSharedError>>>;
type TrustListener = Arc<dyn Fn(WorkspaceTrustChange) + Send + Sync>;

struct ServiceState {
    initialization: Option<SharedServiceFuture>,
    initialization_generation: u64,
    inner: Option<Arc<dyn IWorkspaceTrustEntry>>,
    inner_subscription: Option<Box<dyn IDisposable>>,
    listeners: IndexMap<u64, TrustListener>,
    next_listener_id: u64,
    is_disposed: bool,
}

struct WorkspaceTrustServiceInner {
    dirs: Arc<dyn IWorkspaceDirs>,
    trust_store: Arc<dyn IWorkspaceTrustStore>,
    states: Arc<dyn IWorkspaceStateService>,
    config: Arc<dyn IConfigService>,
    state: Arc<parking_lot::Mutex<ServiceState>>,
    _dirs_subscription: parking_lot::Mutex<Option<Box<dyn IDisposable>>>,
}

#[derive(Clone)]
pub struct WorkspaceTrustService {
    inner: Arc<WorkspaceTrustServiceInner>,
}

impl WorkspaceTrustService {
    pub fn new(
        dirs: Arc<dyn IWorkspaceDirs>,
        trust_store: Arc<dyn IWorkspaceTrustStore>,
        states: Arc<dyn IWorkspaceStateService>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        states.contribute_state(&WORKSPACE_TRUST_TRUSTED_KEY);
        let this = Self {
            inner: Arc::new(WorkspaceTrustServiceInner {
                dirs,
                trust_store,
                states,
                config,
                state: Arc::new(parking_lot::Mutex::new(ServiceState {
                    initialization: None,
                    initialization_generation: 0,
                    inner: None,
                    inner_subscription: None,
                    listeners: IndexMap::new(),
                    next_listener_id: 0,
                    is_disposed: false,
                })),
                _dirs_subscription: parking_lot::Mutex::new(None),
            }),
        };
        this.bind_trust(None);
        let weak = Arc::downgrade(&this.inner);
        let subscription = this.inner.dirs.on_did_change(move || {
            if let Some(inner) = weak.upgrade() {
                WorkspaceTrustService { inner }.rebind_trust();
            }
        });
        *this.inner._dirs_subscription.lock() = Some(subscription);
        this
    }

    fn bind_trust(&self, notified: Option<Arc<AtomicBool>>) {
        let old = self.inner.state.lock().inner_subscription.take();
        if let Some(old) = old {
            old.dispose();
        }
        let mut roots = vec![self.inner.dirs.primary()];
        roots.extend(self.inner.dirs.additional_dirs());
        let inner = self.inner.trust_store.for_roots(&roots);
        let weak = Arc::downgrade(&self.inner);
        let subscription = inner.on_did_change(move |change| {
            if let Some(notified) = &notified {
                notified.store(true, Ordering::SeqCst);
            }
            if let Some(inner) = weak.upgrade() {
                let this = WorkspaceTrustService { inner };
                this.set_trusted(change.trusted);
                this.fire(change);
            }
        });
        let mut state = self.inner.state.lock();
        state.inner = Some(inner);
        state.inner_subscription = Some(subscription);
    }

    fn rebind_trust(&self) {
        let previous = self.trusted();
        let notified = Arc::new(AtomicBool::new(false));
        self.bind_trust(Some(notified.clone()));
        self.inner.state.lock().initialization = None;
        let this = self.clone();
        tokio::spawn(async move {
            if this.ready().await.is_err() {
                return;
            }
            if !notified.load(Ordering::SeqCst) && previous != this.trusted() {
                this.fire(WorkspaceTrustChange {
                    trusted: this.trusted(),
                });
            }
        });
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        if let Some(initialization) = self.inner.state.lock().initialization.clone() {
            return async move { initialization.await.map_err(ServiceSharedError::boxed) }.boxed();
        }
        let generation = {
            let mut state = self.inner.state.lock();
            state.initialization_generation += 1;
            state.initialization_generation
        };
        let this = self.clone();
        let initialization = async move {
            let inner = this.trust_entry();
            inner.ready().await.map_err(ServiceSharedError::new)?;
            let trusted = inner.is_persisted_trusted();
            this.set_trusted(trusted);
            Ok(())
        }
        .boxed()
        .shared();
        self.inner.state.lock().initialization = Some(initialization.clone());
        let this = self.clone();
        let observed = initialization.clone();
        tokio::spawn(async move {
            if observed.await.is_err() {
                let mut state = this.inner.state.lock();
                if state.initialization_generation == generation {
                    state.initialization = None;
                }
            }
        });
        async move { initialization.await.map_err(ServiceSharedError::boxed) }.boxed()
    }

    fn trust_entry(&self) -> Arc<dyn IWorkspaceTrustEntry> {
        self.inner
            .state
            .lock()
            .inner
            .as_ref()
            .unwrap()
            .clone()
    }

    fn trusted(&self) -> bool {
        self.inner.states.get(&WORKSPACE_TRUST_TRUSTED_KEY)
    }

    fn set_trusted(&self, value: bool) {
        self.inner
            .states
            .set(&WORKSPACE_TRUST_TRUSTED_KEY, value);
    }

    pub fn is_trusted(&self) -> bool {
        self.trust_entry().is_trusted()
    }

    pub async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        self.inner.config.ready().await?;
        if !workspace_trust_required(self.inner.config.as_ref()) {
            return Ok(true);
        }
        self.ready().await?;
        self.trust_entry().get().await
    }

    pub async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        self.trust_entry().trust().await
    }

    pub async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        self.trust_entry().untrust().await
    }

    pub fn on_did_change(&self, listener: TrustListener) -> Box<dyn IDisposable> {
        let id = {
            let mut state = self.inner.state.lock();
            if state.is_disposed {
                drop(state);
                return to_disposable(|| {});
            }
            let id = state.next_listener_id;
            state.next_listener_id += 1;
            state.listeners.insert(id, listener);
            id
        };
        let state = self.inner.state.clone();
        to_disposable(move || {
            state.lock().listeners.shift_remove(&id);
        })
    }

    fn fire(&self, change: WorkspaceTrustChange) {
        let listeners = self
            .inner
            .state
            .lock()
            .listeners
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            safely_call_listener(|| listener(change));
        }
    }
}

impl WorkspaceTrustService {
    pub fn dispose(&self) {
        if let Some(subscription) = self.inner._dirs_subscription.lock().take() {
            subscription.dispose();
        }
        let subscription = {
            let mut state = self.inner.state.lock();
            state.is_disposed = true;
            state.listeners.clear();
            state.inner_subscription.take()
        };
        if let Some(subscription) = subscription {
            subscription.dispose();
        }
    }
}

#[async_trait]
impl IWorkspaceTrust for WorkspaceTrustService {
    fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        WorkspaceTrustService::ready(self)
    }

    async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        WorkspaceTrustService::get(self).await
    }

    fn is_trusted(&self) -> bool {
        WorkspaceTrustService::is_trusted(self)
    }

    async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        WorkspaceTrustService::trust(self).await
    }

    async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        WorkspaceTrustService::untrust(self).await
    }

    fn on_did_change(&self, listener: TrustListener) -> Box<dyn IDisposable> {
        WorkspaceTrustService::on_did_change(self, listener)
    }
}
