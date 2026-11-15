use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, StreamExt};
use indexmap::IndexMap;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::errors::unexpected_error::safely_call_listener;
use crate::_base::utils::paths::canonical_workspace_root;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;

use super::config_section::{WORKSPACE_TRUST_SECTION, workspace_trust_required};
use super::trust_env::trust_workspace_env_trusted;
use super::trust_record::{
    delete_workspace_trust, read_workspace_trust, write_workspace_trust,
};
use super::workspace_trust::{
    IWorkspaceTrust, WorkspaceTrustAsyncError, WorkspaceTrustChange,
};

#[derive(Debug, Clone)]
struct SharedTrustError(Arc<String>);

impl SharedTrustError {
    fn new(error: WorkspaceTrustAsyncError) -> Self {
        Self(Arc::new(error.to_string()))
    }

    fn boxed(self) -> WorkspaceTrustAsyncError {
        Box::new(self)
    }
}

impl fmt::Display for SharedTrustError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl Error for SharedTrustError {}

type SharedTrustResult = Result<(), SharedTrustError>;
type SharedTrustFuture = Shared<BoxFuture<'static, SharedTrustResult>>;
type TrustListener = Arc<dyn Fn(WorkspaceTrustChange) + Send + Sync>;

#[async_trait]
pub trait IWorkspaceTrustEntry: IWorkspaceTrust {
    fn is_persisted_trusted(&self) -> bool;
    async fn refresh(&self) -> Result<bool, WorkspaceTrustAsyncError>;
}

pub trait IWorkspaceTrustStore: Send + Sync {
    fn for_root(&self, root: &str) -> Arc<dyn IWorkspaceTrustEntry>;
    fn for_roots(&self, roots: &[String]) -> Arc<dyn IWorkspaceTrustEntry>;
}

struct EntryState {
    initialization: Option<SharedTrustFuture>,
    initialization_generation: u64,
    initialization_failed: bool,
    trusted: bool,
    mutation_tail: Option<SharedTrustFuture>,
    listeners: IndexMap<u64, TrustListener>,
    next_listener_id: u64,
    is_disposed: bool,
}

struct WorkspaceTrustEntryInner {
    root: String,
    env_trusted: bool,
    docs: Arc<dyn IAtomicDocumentStore>,
    telemetry: Arc<dyn ITelemetryService>,
    config: Arc<dyn IConfigService>,
    state: Arc<parking_lot::Mutex<EntryState>>,
}

#[derive(Clone)]
pub struct WorkspaceTrustEntry {
    inner: Arc<WorkspaceTrustEntryInner>,
}

impl WorkspaceTrustEntry {
    pub fn new(
        root: String,
        docs: Arc<dyn IAtomicDocumentStore>,
        telemetry: Arc<dyn ITelemetryService>,
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        let env_trusted = trust_workspace_env_trusted(|name| bootstrap.get_env(name));
        Self {
            inner: Arc::new(WorkspaceTrustEntryInner {
                root,
                env_trusted,
                docs,
                telemetry,
                config,
                state: Arc::new(parking_lot::Mutex::new(EntryState {
                    initialization: None,
                    initialization_generation: 0,
                    initialization_failed: false,
                    trusted: false,
                    mutation_tail: None,
                    listeners: IndexMap::new(),
                    next_listener_id: 0,
                    is_disposed: false,
                })),
            }),
        }
    }

    pub fn is_persisted_trusted(&self) -> bool {
        self.inner.state.lock().trusted
    }

    pub fn is_trusted(&self) -> bool {
        !self.trust_gate_enabled()
            || self.inner.env_trusted
            || self.inner.state.lock().trusted
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        if let Some(initialization) = self.inner.state.lock().initialization.clone() {
            return async move { initialization.await.map_err(SharedTrustError::boxed) }.boxed();
        }
        let (recovering, generation) = {
            let mut state = self.inner.state.lock();
            state.initialization_generation += 1;
            (
                state.initialization_failed,
                state.initialization_generation,
            )
        };
        let this = self.clone();
        let initialization = async move {
            let trusted = read_workspace_trust(
                this.inner.docs.as_ref(),
                &this.inner.root,
                Some(&this.inner.telemetry),
            )
            .await
            .map_err(SharedTrustError::new)?;
            this.inner.state.lock().trusted = trusted;
            this.inner.state.lock().initialization_failed = false;
            if recovering {
                this.fire(WorkspaceTrustChange { trusted });
            }
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
                state.initialization_failed = true;
                if state.initialization_generation == generation {
                    state.initialization = None;
                }
            }
        });
        async move { initialization.await.map_err(SharedTrustError::boxed) }.boxed()
    }

    pub async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        self.ready().await?;
        Ok(self.is_trusted())
    }

    pub async fn refresh(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        let previous = self.inner.state.lock().mutation_tail.clone();
        let this = self.clone();
        let refresh = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let trusted = read_workspace_trust(
                this.inner.docs.as_ref(),
                &this.inner.root,
                Some(&this.inner.telemetry),
            )
            .await
            .map_err(SharedTrustError::new)?;
            if this.inner.state.lock().trusted != trusted {
                this.inner.state.lock().trusted = trusted;
                this.fire(WorkspaceTrustChange { trusted });
            }
            Ok(())
        }
        .boxed()
        .shared();
        self.inner.state.lock().mutation_tail = Some(refresh.clone());
        refresh.await.map_err(SharedTrustError::boxed)?;
        Ok(self.is_trusted())
    }

    pub fn on_config_change(&self) {
        if !self.trust_gate_enabled() {
            self.fire(WorkspaceTrustChange {
                trusted: self.is_persisted_trusted(),
            });
            return;
        }
        let this = self.clone();
        let ready = self.ready();
        tokio::spawn(async move {
            let _ = ready.await;
            this.fire(WorkspaceTrustChange {
                trusted: this.is_persisted_trusted(),
            });
        });
    }

    pub async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        self.mutate(true).await
    }

    pub async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        self.mutate(false).await
    }

    pub fn on_did_change(&self, listener: TrustListener) -> DisposableRef {
        add_listener(&self.inner.state, listener)
    }

    fn trust_gate_enabled(&self) -> bool {
        workspace_trust_required(self.inner.config.as_ref())
    }

    fn mutate(&self, trusted: bool) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        let previous = self.inner.state.lock().mutation_tail.clone();
        let this = self.clone();
        let mutation = async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            this.ready().await.map_err(SharedTrustError::new)?;
            if this.inner.state.lock().trusted == trusted {
                return Ok(());
            }
            if trusted {
                write_workspace_trust(
                    this.inner.docs.as_ref(),
                    &this.inner.root,
                    chrono::Utc::now().timestamp_millis(),
                )
                .await
                .map_err(SharedTrustError::new)?;
            } else {
                delete_workspace_trust(this.inner.docs.as_ref(), &this.inner.root)
                    .await
                    .map_err(SharedTrustError::new)?;
            }
            this.inner.state.lock().trusted = trusted;
            this.fire(WorkspaceTrustChange { trusted });
            let mut properties = serde_json::Map::new();
            properties.insert("trusted".to_string(), trusted.into());
            this.inner
                .telemetry
                .track2("workspace_trust_changed", Some(properties));
            Ok(())
        }
        .boxed()
        .shared();
        self.inner.state.lock().mutation_tail = Some(mutation.clone());
        async move { mutation.await.map_err(SharedTrustError::boxed) }.boxed()
    }

    fn fire(&self, change: WorkspaceTrustChange) {
        fire_listeners(&self.inner.state, change);
    }

    pub fn dispose(&self) {
        dispose_listeners(&self.inner.state);
    }
}

#[async_trait]
impl IWorkspaceTrust for WorkspaceTrustEntry {
    fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        WorkspaceTrustEntry::ready(self)
    }

    async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        WorkspaceTrustEntry::get(self).await
    }

    fn is_trusted(&self) -> bool {
        WorkspaceTrustEntry::is_trusted(self)
    }

    async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        WorkspaceTrustEntry::trust(self).await
    }

    async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        WorkspaceTrustEntry::untrust(self).await
    }

    fn on_did_change(&self, listener: TrustListener) -> DisposableRef {
        WorkspaceTrustEntry::on_did_change(self, listener)
    }
}

#[async_trait]
impl IWorkspaceTrustEntry for WorkspaceTrustEntry {
    fn is_persisted_trusted(&self) -> bool {
        WorkspaceTrustEntry::is_persisted_trusted(self)
    }

    async fn refresh(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        WorkspaceTrustEntry::refresh(self).await
    }
}

struct GroupState {
    initialization: Option<SharedTrustFuture>,
    initialization_generation: u64,
    listeners: IndexMap<u64, TrustListener>,
    next_listener_id: u64,
    is_disposed: bool,
}

#[derive(Clone)]
pub struct WorkspaceTrustGroup {
    primary: Arc<dyn IWorkspaceTrustEntry>,
    entries: Arc<Vec<Arc<dyn IWorkspaceTrustEntry>>>,
    state: Arc<parking_lot::Mutex<GroupState>>,
    _subscriptions: Arc<Vec<DisposableRef>>,
}

impl WorkspaceTrustGroup {
    pub fn new(
        roots: &[String],
        for_root: impl Fn(&str) -> Arc<dyn IWorkspaceTrustEntry>,
    ) -> Result<Self, WorkspaceTrustAsyncError> {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        let state = Arc::new(parking_lot::Mutex::new(GroupState {
            initialization: None,
            initialization_generation: 0,
            listeners: IndexMap::new(),
            next_listener_id: 0,
            is_disposed: false,
        }));
        for root in roots {
            let key = canonical_workspace_root(root);
            if !seen.insert(key) {
                continue;
            }
            entries.push(for_root(root));
        }
        let Some(primary) = entries.first().cloned() else {
            return Err("workspace trust requires a primary root".into());
        };
        let entries = Arc::new(entries);
        let mut group_subscriptions = Vec::new();
        for entry in entries.iter() {
            let state = state.clone();
            let entries = Arc::downgrade(&entries);
            group_subscriptions.push(entry.on_did_change(move |_| {
                let Some(entries) = entries.upgrade() else {
                    return;
                };
                let trusted = entries
                    .iter()
                    .any(|entry| entry.is_persisted_trusted());
                fire_listeners(&state, WorkspaceTrustChange { trusted });
            }));
        }
        Ok(Self {
            primary,
            entries,
            state,
            _subscriptions: Arc::new(group_subscriptions),
        })
    }

    pub fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        if let Some(initialization) = self.state.lock().initialization.clone() {
            return async move { initialization.await.map_err(SharedTrustError::boxed) }.boxed();
        }
        let generation = {
            let mut state = self.state.lock();
            state.initialization_generation += 1;
            state.initialization_generation
        };
        let entries = self.entries.clone();
        let initialization = async move {
            let ready = entries
                .iter()
                .map(|entry| entry.ready())
                .collect::<Vec<_>>();
            let results = futures_util::future::join_all(ready).await;
            for result in results {
                result.map_err(SharedTrustError::new)?;
            }
            Ok(())
        }
        .boxed()
        .shared();
        self.state.lock().initialization = Some(initialization.clone());
        let state = self.state.clone();
        let observed = initialization.clone();
        tokio::spawn(async move {
            if observed.await.is_err() {
                let mut state = state.lock();
                if state.initialization_generation == generation {
                    state.initialization = None;
                }
            }
        });
        async move { initialization.await.map_err(SharedTrustError::boxed) }.boxed()
    }

    pub fn is_persisted_trusted(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.is_persisted_trusted())
    }

    pub fn is_trusted(&self) -> bool {
        self.entries.iter().any(|entry| entry.is_trusted())
    }

    pub async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        let values = futures_util::future::join_all(
            self.entries.iter().map(|entry| entry.get()),
        )
        .await;
        let mut trusted = false;
        for value in values {
            trusted |= value?;
        }
        Ok(trusted)
    }

    pub async fn refresh(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        let values = futures_util::future::join_all(
            self.entries.iter().map(|entry| entry.refresh()),
        )
        .await;
        for value in values {
            value?;
        }
        Ok(self.is_trusted())
    }

    pub async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        self.primary.trust().await
    }

    pub async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        let values = futures_util::future::join_all(
            self.entries.iter().map(|entry| entry.untrust()),
        )
        .await;
        for value in values {
            value?;
        }
        Ok(())
    }

    pub fn on_did_change(&self, listener: TrustListener) -> DisposableRef {
        add_listener(&self.state, listener)
    }

    pub fn dispose(&self) {
        dispose_listeners(&self.state);
        for subscription in &*self._subscriptions {
            subscription.dispose();
        }
    }
}

#[async_trait]
impl IWorkspaceTrust for WorkspaceTrustGroup {
    fn ready(&self) -> BoxFuture<'static, Result<(), WorkspaceTrustAsyncError>> {
        WorkspaceTrustGroup::ready(self)
    }

    async fn get(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        WorkspaceTrustGroup::get(self).await
    }

    fn is_trusted(&self) -> bool {
        WorkspaceTrustGroup::is_trusted(self)
    }

    async fn trust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        WorkspaceTrustGroup::trust(self).await
    }

    async fn untrust(&self) -> Result<(), WorkspaceTrustAsyncError> {
        WorkspaceTrustGroup::untrust(self).await
    }

    fn on_did_change(&self, listener: TrustListener) -> DisposableRef {
        WorkspaceTrustGroup::on_did_change(self, listener)
    }
}

#[async_trait]
impl IWorkspaceTrustEntry for WorkspaceTrustGroup {
    fn is_persisted_trusted(&self) -> bool {
        WorkspaceTrustGroup::is_persisted_trusted(self)
    }

    async fn refresh(&self) -> Result<bool, WorkspaceTrustAsyncError> {
        WorkspaceTrustGroup::refresh(self).await
    }
}

struct StoreInner {
    docs: Arc<dyn IAtomicDocumentStore>,
    telemetry: Arc<dyn ITelemetryService>,
    bootstrap: Arc<dyn IBootstrapService>,
    config: Arc<dyn IConfigService>,
    entries: Arc<parking_lot::Mutex<IndexMap<String, WorkspaceTrustEntry>>>,
    groups: parking_lot::Mutex<IndexMap<String, WorkspaceTrustGroup>>,
    _config_subscription: DisposableRef,
    is_disposed: AtomicBool,
}

#[derive(Clone)]
pub struct WorkspaceTrustStore {
    inner: Arc<StoreInner>,
}

impl WorkspaceTrustStore {
    pub fn new(
        docs: Arc<dyn IAtomicDocumentStore>,
        telemetry: Arc<dyn ITelemetryService>,
        bootstrap: Arc<dyn IBootstrapService>,
        config: Arc<dyn IConfigService>,
    ) -> Self {
        let entries = Arc::new(parking_lot::Mutex::new(IndexMap::<
            String,
            WorkspaceTrustEntry,
        >::new()));
        let listener_entries = entries.clone();
        let config_subscription = config.on_did_section_change(move |event| {
            if event.domain != WORKSPACE_TRUST_SECTION {
                return;
            }
            for entry in listener_entries.lock().values() {
                entry.on_config_change();
            }
        });
        Self {
            inner: Arc::new(StoreInner {
                docs,
                telemetry,
                bootstrap,
                config,
                entries,
                groups: parking_lot::Mutex::new(IndexMap::new()),
                _config_subscription: config_subscription,
                is_disposed: AtomicBool::new(false),
            }),
        }
    }

    pub fn for_root(&self, root: &str) -> Arc<dyn IWorkspaceTrustEntry> {
        let key = canonical_workspace_root(root);
        if let Some(entry) = self.inner.entries.lock().get(&key).cloned() {
            return Arc::new(entry);
        }
        let entry = WorkspaceTrustEntry::new(
            root.to_string(),
            self.inner.docs.clone(),
            self.inner.telemetry.clone(),
            self.inner.bootstrap.clone(),
            self.inner.config.clone(),
        );
        if self.inner.is_disposed.load(Ordering::Relaxed) {
            entry.dispose();
        }
        self.inner.entries.lock().insert(key, entry.clone());
        Arc::new(entry)
    }

    pub fn for_roots(&self, roots: &[String]) -> Arc<dyn IWorkspaceTrustEntry> {
        let primary = roots
            .first()
            .expect("workspace trust requires a primary root");
        let mut keys = roots
            .iter()
            .map(|root| canonical_workspace_root(root))
            .collect::<Vec<_>>();
        keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        keys.dedup();
        let key = std::iter::once(canonical_workspace_root(primary))
            .chain(keys)
            .collect::<Vec<_>>()
            .join("\0");
        if let Some(group) = self.inner.groups.lock().get(&key).cloned() {
            return Arc::new(group);
        }
        let this = self.clone();
        let group = WorkspaceTrustGroup::new(roots, move |root| this.for_root(root))
            .expect("workspace trust requires a primary root");
        if self.inner.is_disposed.load(Ordering::Relaxed) {
            group.dispose();
        }
        self.inner.groups.lock().insert(key, group.clone());
        Arc::new(group)
    }

    pub fn dispose(&self) {
        self.inner.is_disposed.store(true, Ordering::Relaxed);
        self.inner._config_subscription.dispose();
        for entry in self.inner.entries.lock().values() {
            entry.dispose();
        }
        for group in self.inner.groups.lock().values() {
            group.dispose();
        }
    }
}

impl IWorkspaceTrustStore for WorkspaceTrustStore {
    fn for_root(&self, root: &str) -> Arc<dyn IWorkspaceTrustEntry> {
        WorkspaceTrustStore::for_root(self, root)
    }

    fn for_roots(&self, roots: &[String]) -> Arc<dyn IWorkspaceTrustEntry> {
        WorkspaceTrustStore::for_roots(self, roots)
    }
}

trait ListenerState {
    fn listeners(&mut self) -> (&mut IndexMap<u64, TrustListener>, &mut u64);
    fn listener_snapshot(&self) -> Vec<TrustListener>;
    fn is_disposed(&self) -> bool;
    fn mark_disposed(&mut self);
}

impl ListenerState for EntryState {
    fn listeners(&mut self) -> (&mut IndexMap<u64, TrustListener>, &mut u64) {
        (&mut self.listeners, &mut self.next_listener_id)
    }

    fn listener_snapshot(&self) -> Vec<TrustListener> {
        self.listeners.values().cloned().collect()
    }

    fn is_disposed(&self) -> bool {
        self.is_disposed
    }

    fn mark_disposed(&mut self) {
        self.is_disposed = true;
    }
}

impl ListenerState for GroupState {
    fn listeners(&mut self) -> (&mut IndexMap<u64, TrustListener>, &mut u64) {
        (&mut self.listeners, &mut self.next_listener_id)
    }

    fn listener_snapshot(&self) -> Vec<TrustListener> {
        self.listeners.values().cloned().collect()
    }

    fn is_disposed(&self) -> bool {
        self.is_disposed
    }

    fn mark_disposed(&mut self) {
        self.is_disposed = true;
    }
}

fn add_listener<S: ListenerState + Send + 'static>(
    state: &Arc<parking_lot::Mutex<S>>,
    listener: TrustListener,
) -> DisposableRef {
    let id = {
        let mut state = state.lock();
        if state.is_disposed() {
            drop(state);
            return to_disposable(|| {});
        }
        let (listeners, next_id) = state.listeners();
        let id = *next_id;
        *next_id += 1;
        listeners.insert(id, listener);
        id
    };
    let state = state.clone();
    to_disposable(move || {
        state.lock().listeners().0.shift_remove(&id);
    })
}

fn fire_listeners<S: ListenerState>(
    state: &Arc<parking_lot::Mutex<S>>,
    change: WorkspaceTrustChange,
) {
    let listeners = state.lock().listener_snapshot();
    for listener in listeners {
        safely_call_listener(|| listener(change));
    }
}

fn dispose_listeners<S: ListenerState>(state: &Arc<parking_lot::Mutex<S>>) {
    let mut state = state.lock();
    state.mark_disposed();
    state.listeners().0.clear();
}
