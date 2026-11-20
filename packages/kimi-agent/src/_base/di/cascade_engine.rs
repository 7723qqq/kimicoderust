use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::future::{BoxFuture, FutureExt};
use indexmap::IndexMap;
use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

use super::dependency_graph::{DependencyGraph, ObjectId, PairIndex, ScopedToken};
use super::descriptors::{DynamicValue, SyncDescriptor};
use super::errors::CascadeConflictError;
use super::instantiation::ServiceIdentifier;
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::event::{Emitter, Event};

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum UnitState {
    Pending,
    Activating,
    Active,
    Unloading,
    Failed,
}

impl UnitState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Pending",
            Self::Activating => "Activating",
            Self::Active => "Active",
            Self::Unloading => "Unloading",
            Self::Failed => "Failed",
        }
    }
}

impl fmt::Display for UnitState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CascadeAction {
    Provide,
    Unprovide,
    Update,
}

impl CascadeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provide => "provide",
            Self::Unprovide => "unprovide",
            Self::Update => "update",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnitActivation {
    Eager,
    OnDemand,
}

#[derive(Clone)]
pub struct CascadeChange {
    pub action: CascadeAction,
    pub token: ServiceIdentifier<DynamicValue>,
    pub descriptor: Option<SyncDescriptor<DynamicValue>>,
    pub instance: Option<DynamicValue>,
    pub activation: Option<UnitActivation>,
    pub config: Option<DynamicValue>,
    pub reason: String,
}

#[derive(Clone)]
pub struct CascadeHistoryChange {
    pub token: String,
    pub action: CascadeAction,
}

#[derive(Clone)]
pub struct CascadeHistoryEntry {
    pub seq: u64,
    pub reason: String,
    pub changes: Vec<CascadeHistoryChange>,
    pub affected: Vec<String>,
    pub torn_down: Vec<String>,
    pub rebuilt: Vec<String>,
    pub failed: Vec<String>,
    pub abort_waited: bool,
    pub abort_timed_out: bool,
    pub duration_ms: u64,
}

#[derive(Clone)]
pub struct UnitSnapshot {
    pub token: String,
    pub state: UnitState,
    pub error: Option<String>,
    pub ever_active: bool,
    pub in_flight: bool,
}

#[derive(Clone)]
pub struct UnitStateChange {
    pub token: String,
    pub state: UnitState,
    pub error: Option<String>,
}

pub type CascadeHook = Arc<
    dyn Fn(Vec<ScopedToken>, String) -> BoxFuture<'static, Result<(), String>> + Send + Sync,
>;

#[derive(Clone)]
pub struct CascadeEngineOptions {
    pub on_will_cascade: Option<CascadeHook>,
    pub abort_wait_ms: Option<u64>,
    pub resolve_timeout_ms: Option<u64>,
    pub history_capacity: Option<usize>,
    pub now: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

impl Default for CascadeEngineOptions {
    fn default() -> Self {
        Self {
            on_will_cascade: None,
            abort_wait_ms: None,
            resolve_timeout_ms: None,
            history_capacity: None,
            now: None,
        }
    }
}

impl CascadeEngineOptions {
    fn merge(&self, next: Self) -> Self {
        Self {
            on_will_cascade: next.on_will_cascade.or_else(|| self.on_will_cascade.clone()),
            abort_wait_ms: next.abort_wait_ms.or(self.abort_wait_ms),
            resolve_timeout_ms: next.resolve_timeout_ms.or(self.resolve_timeout_ms),
            history_capacity: next.history_capacity.or(self.history_capacity),
            now: next.now.or_else(|| self.now.clone()),
        }
    }
}

pub trait CascadeHost: Send + Sync {
    fn is_registered(&self, token: &ServiceIdentifier<DynamicValue>) -> bool;
    fn owner_scope_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<ObjectId>;
    fn is_materialized(&self, token: &ServiceIdentifier<DynamicValue>) -> bool;
    fn materialize(&self, token: &ServiceIdentifier<DynamicValue>) -> Result<DynamicValue, String>;
    fn retire(&self, token: ServiceIdentifier<DynamicValue>) -> BoxFuture<'static, Result<(), String>>;
    fn apply_provide(
        &self,
        token: ServiceIdentifier<DynamicValue>,
        descriptor: SyncDescriptor<DynamicValue>,
        config: Option<DynamicValue>,
    ) -> Result<u64, String>;
    fn apply_provide_instance(
        &self,
        token: ServiceIdentifier<DynamicValue>,
        instance: Option<DynamicValue>,
        config: Option<DynamicValue>,
    ) -> Result<u64, String>;
    fn apply_unprovide(&self, token: &ServiceIdentifier<DynamicValue>) -> Result<(), String>;
    fn recipe_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<SyncDescriptor<DynamicValue>>;
    fn dependencies_of(
        &self,
        recipe: &SyncDescriptor<DynamicValue>,
    ) -> Vec<ServiceIdentifier<DynamicValue>>;
}

pub trait CascadeScopeHandle: Send + Sync {
    fn cascade(&self) -> Arc<CascadeEngine>;
    fn cascade_disposed(&self) -> bool;
    fn cascade_depth(&self) -> usize;
    fn scope_id(&self) -> ObjectId;
}

#[derive(Clone)]
struct UnitRecord {
    state: UnitState,
    error: Option<String>,
    activation: UnitActivation,
    ever_active: bool,
}

type SubmitResult = Arc<Mutex<Option<oneshot::Sender<Result<(), String>>>>>;

struct QueuedRequest {
    engine: Arc<CascadeEngine>,
    change: CascadeChange,
    result: SubmitResult,
}

const DEFAULT_ABORT_WAIT_MS: u64 = 5000;
const DEFAULT_RESOLVE_TIMEOUT_MS: u64 = 30000;
const DEFAULT_HISTORY_CAPACITY: usize = 200;

pub struct CascadeTree {
    pub graph: Arc<Mutex<DependencyGraph>>,
    queue: Mutex<Vec<QueuedRequest>>,
    engines: Mutex<IndexMap<ObjectId, Arc<CascadeEngine>>>,
    scopes: Mutex<HashMap<ObjectId, Weak<dyn CascadeScopeHandle>>>,
    running: AtomicBool,
    orchestrator: Mutex<Option<ObjectId>>,
    in_flight: Mutex<PairIndex<bool>>,
    settle_waiters: Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    settle_notify: Notify,
    scope_seq: Mutex<IndexMap<ObjectId, i64>>,
    next_scope_seq: Mutex<i64>,
    dropped_scopes: Mutex<HashSet<ObjectId>>,
    on_did_add_engine: Emitter<Arc<CascadeEngine>>,
    on_did_remove_engine: Emitter<Arc<CascadeEngine>>,
    pub on_did_add_engine_event: Event<Arc<CascadeEngine>>,
    pub on_did_remove_engine_event: Event<Arc<CascadeEngine>>,
}

impl CascadeTree {
    pub fn engines(&self) -> Vec<Arc<CascadeEngine>> {
        self.engines.lock().values().cloned().collect()
    }

    pub fn new(graph: DependencyGraph) -> Arc<Self> {
        let on_did_add_engine = Emitter::new();
        let on_did_remove_engine = Emitter::new();
        Arc::new(Self {
            graph: Arc::new(Mutex::new(graph)),
            queue: Mutex::new(Vec::new()),
            engines: Mutex::new(IndexMap::new()),
            scopes: Mutex::new(HashMap::new()),
            running: AtomicBool::new(false),
            orchestrator: Mutex::new(None),
            in_flight: Mutex::new(PairIndex::new()),
            settle_waiters: Mutex::new(Vec::new()),
            settle_notify: Notify::new(),
            scope_seq: Mutex::new(IndexMap::new()),
            next_scope_seq: Mutex::new(0),
            dropped_scopes: Mutex::new(HashSet::new()),
            on_did_add_engine_event: on_did_add_engine.event(),
            on_did_remove_engine_event: on_did_remove_engine.event(),
            on_did_add_engine,
            on_did_remove_engine,
        })
    }

    fn add_engine(self: &Arc<Self>, engine: &Arc<CascadeEngine>) {
        self.engines
            .lock()
            .insert(engine.scope.scope_id(), engine.clone());
        self.scopes
            .lock()
            .insert(engine.scope.scope_id(), Arc::downgrade(&engine.scope));
        self.on_did_add_engine.fire(engine.clone());
    }

    fn remove_engine(&self, engine: &Arc<CascadeEngine>) {
        if self
            .engines
            .lock()
            .shift_remove(&engine.scope.scope_id())
            .is_some()
        {
            self.on_did_remove_engine.fire(engine.clone());
        }
    }

    pub fn in_flight_set(&self, reference: &ScopedToken, on: bool) {
        if on {
            self.in_flight
                .lock()
                .set(reference.scope, &reference.token, true);
        } else {
            self.in_flight
                .lock()
                .delete(reference.scope, &reference.token);
        }
    }

    pub fn in_flight_has(&self, reference: &ScopedToken) -> bool {
        self.in_flight
            .lock()
            .get(reference.scope, &reference.token)
            .is_some()
    }

    pub fn in_flight_clear(&self, references: &[ScopedToken]) {
        let mut in_flight = self.in_flight.lock();
        for reference in references {
            in_flight.delete(reference.scope, &reference.token);
        }
    }

    pub fn seq_of(&self, scope: ObjectId) -> i64 {
        if let Some(seq) = self.scope_seq.lock().get(&scope) {
            return *seq;
        }
        if self.dropped_scopes.lock().contains(&scope) {
            return -1;
        }
        let mut next = self.next_scope_seq.lock();
        let seq = *next;
        *next += 1;
        drop(next);
        self.scope_seq.lock().insert(scope, seq);
        seq
    }

    pub fn drop_scope(&self, scope: ObjectId) {
        self.dropped_scopes.lock().insert(scope);
        self.scope_seq.lock().shift_remove(&scope);
        self.in_flight.lock().delete_scope(scope);
        self.scopes.lock().remove(&scope);
    }

    pub fn add_settle_waiter(&self, waiter: impl FnOnce() + Send + 'static) {
        self.settle_waiters.lock().push(Box::new(waiter));
    }

    pub fn fire_settle_waiters(&self) {
        let waiters = self.settle_waiters.lock().drain(..).collect::<Vec<_>>();
        for waiter in waiters {
            waiter();
        }
        self.settle_notify.notify_waiters();
    }

    pub fn orchestrator(&self) -> Option<ObjectId> {
        *self.orchestrator.lock()
    }

    fn engine_of(&self, reference: &ScopedToken) -> Option<Arc<CascadeEngine>> {
        self.scopes
            .lock()
            .get(&reference.scope)
            .and_then(Weak::upgrade)
            .map(|scope| scope.cascade())
    }

    fn pump(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let batch = self.queue.lock().drain(..).collect::<Vec<_>>();
        if batch.is_empty() {
            self.running.store(false, Ordering::SeqCst);
            return;
        }
        let tree = self.clone();
        tokio::spawn(async move {
            let orchestrator = batch[0].engine.clone();
            let result = orchestrator.transact(batch.clone_requests()).await;
            for request in batch {
                if let Some(sender) = request.result.lock().take() {
                    let _ = sender.send(result.clone());
                }
            }
            tree.running.store(false, Ordering::SeqCst);
            tree.fire_settle_waiters();
            tree.pump();
        });
    }
}

trait CloneRequests {
    fn clone_requests(&self) -> Vec<QueuedRequest>;
}

impl CloneRequests for Vec<QueuedRequest> {
    fn clone_requests(&self) -> Vec<QueuedRequest> {
        self.iter()
            .map(|request| QueuedRequest {
                engine: request.engine.clone(),
                change: request.change.clone(),
                result: request.result.clone(),
            })
            .collect()
    }
}

pub struct CascadeEngine {
    host: Arc<dyn CascadeHost>,
    scope: Arc<dyn CascadeScopeHandle>,
    tree: Arc<CascadeTree>,
    options: Mutex<CascadeEngineOptions>,
    units: Mutex<IndexMap<ServiceIdentifier<DynamicValue>, UnitRecord>>,
    pending_index: Mutex<IndexMap<ServiceIdentifier<DynamicValue>, HashSet<ServiceIdentifier<DynamicValue>>>>,
    history: Mutex<Vec<CascadeHistoryEntry>>,
    history_seq: Mutex<u64>,
    disposed: AtomicBool,
    activation_suspended: Mutex<usize>,
    on_did_change_unit_state: Emitter<UnitStateChange>,
    on_did_cascade: Emitter<CascadeHistoryEntry>,
    pub on_did_change_unit_state_event: Event<UnitStateChange>,
    pub on_did_cascade_event: Event<CascadeHistoryEntry>,
}

impl CascadeEngine {
    pub fn new(
        host: Arc<dyn CascadeHost>,
        scope: Arc<dyn CascadeScopeHandle>,
        tree: Arc<CascadeTree>,
        options: CascadeEngineOptions,
    ) -> Arc<Self> {
        let on_did_change_unit_state = Emitter::new();
        let on_did_cascade = Emitter::new();
        let engine = Arc::new(Self {
            host,
            scope,
            tree: tree.clone(),
            options: Mutex::new(options),
            units: Mutex::new(IndexMap::new()),
            pending_index: Mutex::new(IndexMap::new()),
            history: Mutex::new(Vec::new()),
            history_seq: Mutex::new(0),
            disposed: AtomicBool::new(false),
            activation_suspended: Mutex::new(0),
            on_did_change_unit_state_event: on_did_change_unit_state.event(),
            on_did_cascade_event: on_did_cascade.event(),
            on_did_change_unit_state,
            on_did_cascade,
        });
        tree.add_engine(&engine);
        engine
    }

    pub fn configure(&self, options: CascadeEngineOptions) {
        let merged = self.options.lock().merge(options);
        *self.options.lock() = merged;
    }

    pub fn state_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<UnitState> {
        self.units.lock().get(token).map(|unit| unit.state)
    }

    pub fn activation_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<UnitActivation> {
        self.units.lock().get(token).map(|unit| unit.activation)
    }

    pub fn materializable(&self, token: &ServiceIdentifier<DynamicValue>) -> bool {
        self.host.recipe_of(token).is_some()
    }

    pub fn failure_of(&self, token: &ServiceIdentifier<DynamicValue>) -> Option<String> {
        self.units.lock().get(token).and_then(|unit| {
            if unit.state == UnitState::Failed {
                unit.error.clone()
            } else {
                None
            }
        })
    }

    pub fn is_in_flight(&self, token: &ServiceIdentifier<DynamicValue>) -> bool {
        let owner = self
            .host
            .owner_scope_of(token)
            .unwrap_or_else(|| self.scope.scope_id());
        self.tree.in_flight_has(&ScopedToken {
            scope: owner,
            token: token.clone(),
        })
    }

    pub fn history(&self) -> Vec<CascadeHistoryEntry> {
        self.history.lock().clone()
    }

    pub fn pending_snapshot(&self) -> IndexMap<String, Vec<String>> {
        let tokens = self
            .units
            .lock()
            .iter()
            .filter(|(_, unit)| unit.state == UnitState::Pending)
            .map(|(token, _)| token.clone())
            .collect::<Vec<_>>();
        let mut snapshot = IndexMap::new();
        for token in tokens {
            snapshot.insert(
                token.to_string(),
                self.missing_deps(&token)
                    .into_iter()
                    .map(|dependency| dependency.to_string())
                    .collect(),
            );
        }
        snapshot
    }

    pub fn units_snapshot(&self) -> Vec<UnitSnapshot> {
        let units = self
            .units
            .lock()
            .iter()
            .map(|(token, unit)| (token.clone(), unit.clone()))
            .collect::<Vec<_>>();
        units
            .into_iter()
            .map(|(token, unit)| UnitSnapshot {
                token: token.to_string(),
                state: unit.state,
                error: unit.error,
                ever_active: unit.ever_active,
                in_flight: self.is_in_flight(&token),
            })
            .collect()
    }

    pub fn submit(self: &Arc<Self>, change: CascadeChange) -> BoxFuture<'static, Result<(), String>> {
        if self.disposed.load(Ordering::SeqCst) {
            return async { Ok(()) }.boxed();
        }
        let (sender, receiver) = oneshot::channel();
        self.tree.queue.lock().push(QueuedRequest {
            engine: self.clone(),
            change,
            result: Arc::new(Mutex::new(Some(sender))),
        });
        self.tree.pump();
        async move {
            receiver
                .await
                .unwrap_or_else(|_| Err("cascade request was dropped".to_string()))?;
            Ok(())
        }
        .boxed()
    }

    pub fn submit_all(
        self: &Arc<Self>,
        changes: Vec<CascadeChange>,
    ) -> BoxFuture<'static, Result<(), String>> {
        if self.disposed.load(Ordering::SeqCst) || changes.is_empty() {
            return async { Ok(()) }.boxed();
        }
        let (sender, receiver) = oneshot::channel();
        let result = Arc::new(Mutex::new(Some(sender)));
        let mut queue = self.tree.queue.lock();
        for change in changes {
            queue.push(QueuedRequest {
                engine: self.clone(),
                change,
                result: result.clone(),
            });
        }
        drop(queue);
        self.tree.pump();
        async move {
            receiver
                .await
                .unwrap_or_else(|_| Err("cascade request was dropped".to_string()))?;
            Ok(())
        }
        .boxed()
    }

    pub fn update(
        self: &Arc<Self>,
        token: ServiceIdentifier<DynamicValue>,
        reason: Option<String>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let default_reason = format!("update {token}");
        self.submit(CascadeChange {
            action: CascadeAction::Update,
            token,
            descriptor: None,
            instance: None,
            activation: None,
            config: None,
            reason: reason.unwrap_or(default_reason),
        })
    }

    pub fn when_idle(&self) -> BoxFuture<'static, ()> {
        let tree = self.tree.clone();
        async move {
            loop {
                let notified = tree.settle_notify.notified();
                if !tree.running.load(Ordering::SeqCst) && tree.queue.lock().is_empty() {
                    return;
                }
                notified.await;
            }
        }
        .boxed()
    }

    pub fn suspend_activation(&self) {
        *self.activation_suspended.lock() += 1;
    }

    pub fn resume_activation(&self) {
        let mut suspended = self.activation_suspended.lock();
        if *suspended == 0 {
            return;
        }
        *suspended -= 1;
        if *suspended > 0 || self.disposed.load(Ordering::SeqCst) {
            return;
        }
        drop(suspended);
        let mut rebuilt = Vec::new();
        let mut failed = Vec::new();
        self.recheck_tree_fixpoint(&mut rebuilt, &mut failed);
    }

    fn recheck_tree_fixpoint(&self, rebuilt: &mut Vec<String>, failed: &mut Vec<String>) {
        let mut engines = self
            .tree
            .engines
            .lock()
            .values()
            .cloned()
            .filter(|engine| !engine.disposed.load(Ordering::SeqCst))
            .collect::<Vec<_>>();
        engines.sort_by_key(|engine| engine.scope.cascade_depth());
        loop {
            let mut progress = false;
            for engine in &engines {
                let before = rebuilt.len() + failed.len();
                engine.recheck_for_cascade(rebuilt, failed);
                if rebuilt.len() + failed.len() > before {
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }
    }

    pub fn resolve_when_available(
        &self,
        token: ServiceIdentifier<DynamicValue>,
        timeout_ms: Option<u64>,
    ) -> BoxFuture<'static, Result<DynamicValue, String>> {
        if !self.is_in_flight(&token) {
            return match self.host.materialize(&token) {
                Ok(value) => async { Ok(value) }.boxed(),
                Err(error) => async move { Err(error) }.boxed(),
            };
        }
        let (sender, receiver) = oneshot::channel();
        let waiter_host = self.host.clone();
        let waiter_token = token.clone();
        self.tree.add_settle_waiter(move || {
            let result = waiter_host.materialize(&waiter_token);
            let _ = sender.send(result);
        });
        let timeout = timeout_ms
            .or_else(|| self.options.lock().resolve_timeout_ms)
            .unwrap_or(DEFAULT_RESOLVE_TIMEOUT_MS);
        async move {
            match tokio::time::timeout(Duration::from_millis(timeout), receiver).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => std::future::pending::<Result<DynamicValue, String>>().await,
                Err(_) => Err(CascadeConflictError::new(
                    token.to_string(),
                    "timed out waiting for the in-flight cascade to settle",
                )
                .to_string()),
            }
        }
        .boxed()
    }

    pub fn observed_materialization(&self, token: &ServiceIdentifier<DynamicValue>) {
        let should_change = self
            .units
            .lock()
            .get(token)
            .is_some_and(|unit| unit.state == UnitState::Pending);
        if should_change {
            self.mark_pending(token, None, Some(true));
            self.set_unit_state(token, UnitState::Active, None);
        }
    }

    pub fn dispose(&self) {
        self.disposed.store(true, Ordering::SeqCst);
        let engine = self
            .tree
            .engines
            .lock()
            .get(&self.scope.scope_id())
            .cloned();
        if let Some(engine) = engine {
            self.tree.remove_engine(&engine);
        }
        self.tree.drop_scope(self.scope.scope_id());
        self.units.lock().clear();
        self.pending_index.lock().clear();
        let mut remaining = Vec::new();
        for request in self.tree.queue.lock().drain(..) {
            if request.engine.scope.scope_id() == self.scope.scope_id() {
                if let Some(sender) = request.result.lock().take() {
                    let _ = sender.send(Ok(()));
                }
            } else {
                remaining.push(request);
            }
        }
        *self.tree.queue.lock() = remaining;
        self.on_did_change_unit_state.dispose();
        self.on_did_cascade.dispose();
    }

    fn teardown_for_cascade(
        &self,
        token: ServiceIdentifier<DynamicValue>,
        torn_down: &mut Vec<String>,
        park_as_pending: bool,
    ) -> BoxFuture<'static, Result<(), String>> {
        if self.disposed.load(Ordering::SeqCst) || !self.host.is_materialized(&token) {
            return async { Ok(()) }.boxed();
        }
        self.unit_for(&token);
        self.set_unit_state(&token, UnitState::Unloading, None);
        let out = self.host.retire(token.clone());
        torn_down.push(self.label(&ScopedToken {
            scope: self.scope.scope_id(),
            token: token.clone(),
        }));
        if park_as_pending {
            self.mark_pending(&token, None, Some(true));
        }
        out
    }

    fn recheck_for_cascade(&self, rebuilt: &mut Vec<String>, failed: &mut Vec<String>) {
        if self.disposed.load(Ordering::SeqCst) || *self.activation_suspended.lock() > 0 {
            return;
        }
        self.recheck_pending(rebuilt, failed);
    }

    fn apply_change_for_cascade(&self, change: &CascadeChange) -> Result<(), String> {
        if self.disposed.load(Ordering::SeqCst) {
            return Ok(());
        }
        match change.action {
            CascadeAction::Provide => {
                if let Some(descriptor) = &change.descriptor {
                    self.host.apply_provide(
                        change.token.clone(),
                        descriptor.clone(),
                        change.config.clone(),
                    )?;
                    self.mark_pending(
                        &change.token,
                        Some(change.activation.unwrap_or(UnitActivation::Eager)),
                        None,
                    );
                } else {
                    self.host.apply_provide_instance(
                        change.token.clone(),
                        change.instance.clone(),
                        change.config.clone(),
                    )?;
                    self.unit_for(&change.token);
                    if let Some(unit) = self.units.lock().get_mut(&change.token) {
                        unit.ever_active = true;
                    }
                    self.set_unit_state(&change.token, UnitState::Active, None);
                }
            }
            CascadeAction::Unprovide => {
                self.host.apply_unprovide(&change.token)?;
                self.units.lock().shift_remove(&change.token);
            }
            CascadeAction::Update => {
                self.mark_pending(&change.token, None, Some(true));
            }
        }
        Ok(())
    }

    async fn transact(&self, batch: Vec<QueuedRequest>) -> Result<(), String> {
        let changes = merge_batch(batch);
        let started = self.now();
        let reason = changes
            .iter()
            .map(|request| request.change.reason.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let affected = self.tree.graph.lock().affected_set(
            changes
                .iter()
                .map(|request| ScopedToken {
                    scope: request.engine.scope.scope_id(),
                    token: request.change.token.clone(),
                })
                .collect::<Vec<_>>(),
        );
        for reference in &affected {
            self.tree.in_flight_set(reference, true);
        }
        *self.tree.orchestrator.lock() = Some(self.scope.scope_id());
        let abort = self.wait_for_abort(&affected, reason.clone()).await;
        let result = self
            .apply_transaction(&changes, &affected, reason, started, abort)
            .await;
        self.tree.in_flight_clear(&affected);
        *self.tree.orchestrator.lock() = None;
        result
    }

    fn now(&self) -> u64 {
        self.options.lock().now.as_ref().map(|now| now()).unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        })
    }

    async fn wait_for_abort(&self, affected: &[ScopedToken], reason: String) -> AbortState {
        let Some(hook) = self.options.lock().on_will_cascade.clone() else {
            return AbortState::default();
        };
        let wait_ms = self
            .options
            .lock()
            .abort_wait_ms
            .unwrap_or(DEFAULT_ABORT_WAIT_MS);
        let future = hook(affected.to_vec(), reason);
        match tokio::time::timeout(Duration::from_millis(wait_ms), future).await {
            Ok(Ok(())) => AbortState {
                waited: true,
                timed_out: false,
            },
            Ok(Err(error)) => {
                on_unexpected_error(error);
                AbortState {
                    waited: true,
                    timed_out: false,
                }
            }
            Err(_) => AbortState {
                waited: true,
                timed_out: true,
            },
        }
    }

    async fn apply_transaction(
        &self,
        changes: &[QueuedRequest],
        affected: &[ScopedToken],
        reason: String,
        started: u64,
        abort: AbortState,
    ) -> Result<(), String> {
        let mut torn_down = Vec::new();
        let mut rebuilt = Vec::new();
        let mut failed = Vec::new();
        let teardown_order = self.tree.graph.lock().reverse_topo_order(affected.to_vec());
        for reference in teardown_order {
            let Some(owner) = self.tree.engine_of(&reference) else {
                continue;
            };
            if owner.disposed.load(Ordering::SeqCst) {
                continue;
            }
            let removed = changes.iter().any(|request| {
                Arc::ptr_eq(&request.engine, &owner)
                    && request.change.token == reference.token
                    && request.change.action == CascadeAction::Unprovide
            });
            owner
                .teardown_for_cascade(reference.token, &mut torn_down, !removed)
                .await?;
        }
        for request in changes {
            request.engine.apply_change_for_cascade(&request.change)?;
        }
        self.recheck_tree_fixpoint(&mut rebuilt, &mut failed);
        let entry = CascadeHistoryEntry {
            seq: {
                let mut seq = self.history_seq.lock();
                *seq += 1;
                *seq
            },
            reason,
            changes: changes
                .iter()
                .map(|request| CascadeHistoryChange {
                    token: self.label(&ScopedToken {
                        scope: request.engine.scope.scope_id(),
                        token: request.change.token.clone(),
                    }),
                    action: request.change.action,
                })
                .collect(),
            affected: affected.iter().map(|reference| self.label(reference)).collect(),
            torn_down,
            rebuilt,
            failed,
            abort_waited: abort.waited,
            abort_timed_out: abort.timed_out,
            duration_ms: self.now().saturating_sub(started),
        };
        self.push_history(entry);
        Ok(())
    }

    fn unit_for(&self, token: &ServiceIdentifier<DynamicValue>) {
        let inserted = {
            let mut units = self.units.lock();
            if !units.contains_key(token) {
                units.insert(
                    token.clone(),
                    UnitRecord {
                        state: UnitState::Pending,
                        error: None,
                        activation: UnitActivation::Eager,
                        ever_active: false,
                    },
                );
                true
            } else {
                false
            }
        };
        if inserted {
            self.on_did_change_unit_state.fire(UnitStateChange {
                token: token.to_string(),
                state: UnitState::Pending,
                error: None,
            });
        }
    }

    fn set_unit_state(
        &self,
        token: &ServiceIdentifier<DynamicValue>,
        state: UnitState,
        error: Option<String>,
    ) {
        let changed = {
            let mut units = self.units.lock();
            let unit = units.entry(token.clone()).or_insert_with(|| UnitRecord {
                state: UnitState::Pending,
                error: None,
                activation: UnitActivation::Eager,
                ever_active: false,
            });
            let changed = unit.state != state;
            unit.state = state;
            unit.error = error.clone();
            changed
        };
        if changed {
            self.on_did_change_unit_state.fire(UnitStateChange {
                token: token.to_string(),
                state,
                error,
            });
        }
    }

    fn mark_pending(
        &self,
        token: &ServiceIdentifier<DynamicValue>,
        activation: Option<UnitActivation>,
        ever_active: Option<bool>,
    ) {
        self.unit_for(token);
        self.set_unit_state(token, UnitState::Pending, None);
        let mut units = self.units.lock();
        if let Some(unit) = units.get_mut(token) {
            if let Some(activation) = activation {
                unit.activation = activation;
            }
            if let Some(ever_active) = ever_active {
                unit.ever_active = ever_active;
            }
        }
    }

    fn recheck_pending(&self, rebuilt: &mut Vec<String>, failed: &mut Vec<String>) {
        loop {
            self.pending_index.lock().clear();
            let units = self
                .units
                .lock()
                .iter()
                .map(|(token, unit)| (token.clone(), unit.clone()))
                .collect::<Vec<_>>();
            let mut satisfied = Vec::new();
            for (token, unit) in units {
                if unit.state != UnitState::Pending {
                    continue;
                }
                let missing = self.missing_deps(&token);
                if missing.is_empty() {
                    if unit.ever_active || unit.activation == UnitActivation::Eager {
                        satisfied.push(token);
                    }
                } else {
                    for dependency in missing {
                        self.pending_index
                            .lock()
                            .entry(dependency)
                            .or_default()
                            .insert(token.clone());
                    }
                }
            }
            if satisfied.is_empty() {
                return;
            }
            let ordered = self.tree.graph.lock().topo_order(
                satisfied
                    .into_iter()
                    .map(|token| ScopedToken {
                        scope: self.scope.scope_id(),
                        token,
                    })
                    .collect::<Vec<_>>(),
            );
            for reference in ordered {
                self.activate(&reference.token, rebuilt, failed);
            }
            let tokens = self
                .units
                .lock()
                .iter()
                .filter(|(_, unit)| {
                    matches!(unit.state, UnitState::Pending | UnitState::Activating)
                })
                .map(|(token, _)| token.clone())
                .collect::<Vec<_>>();
            for token in tokens {
                if self.host.is_materialized(&token) {
                    if let Some(unit) = self.units.lock().get_mut(&token) {
                        unit.ever_active = true;
                    }
                    self.set_unit_state(&token, UnitState::Active, None);
                }
            }
        }
    }

    fn activate(
        &self,
        token: &ServiceIdentifier<DynamicValue>,
        rebuilt: &mut Vec<String>,
        failed: &mut Vec<String>,
    ) {
        self.unit_for(token);
        self.set_unit_state(token, UnitState::Activating, None);
        match self.host.materialize(token) {
            Ok(_) => {
                if let Some(unit) = self.units.lock().get_mut(token) {
                    unit.ever_active = true;
                }
                self.set_unit_state(token, UnitState::Active, None);
                rebuilt.push(self.label(&ScopedToken {
                    scope: self.scope.scope_id(),
                    token: token.clone(),
                }));
            }
            Err(error) => {
                self.set_unit_state(token, UnitState::Failed, Some(error));
                failed.push(self.label(&ScopedToken {
                    scope: self.scope.scope_id(),
                    token: token.clone(),
                }));
            }
        }
    }

    fn missing_deps(&self, token: &ServiceIdentifier<DynamicValue>) -> Vec<ServiceIdentifier<DynamicValue>> {
        let Some(recipe) = self.host.recipe_of(token) else {
            return vec![token.clone()];
        };
        self.host
            .dependencies_of(&recipe)
            .into_iter()
            .filter(|dependency| !self.is_available(dependency))
            .collect()
    }

    fn is_available(&self, dependency: &ServiceIdentifier<DynamicValue>) -> bool {
        if !self.host.is_registered(dependency) {
            return false;
        }
        let owner = self.host.owner_scope_of(dependency);
        let engine = owner.and_then(|scope| {
            self.tree.engine_of(&ScopedToken {
                scope,
                token: dependency.clone(),
            })
        });
        let state = engine.as_ref().and_then(|engine| engine.state_of(dependency));
        if state.is_none() || state == Some(UnitState::Active) {
            return true;
        }
        state == Some(UnitState::Pending)
            && engine.as_ref().is_some_and(|engine| {
                engine.activation_of(dependency) == Some(UnitActivation::OnDemand)
                    && engine.materializable(dependency)
            })
    }

    fn label(&self, reference: &ScopedToken) -> String {
        if self.tree.orchestrator() == Some(reference.scope) {
            return reference.token.to_string();
        }
        format!("#{}:{}", self.tree.seq_of(reference.scope), reference.token)
    }

    fn merge_key(&self) -> String {
        self.tree.seq_of(self.scope.scope_id()).to_string()
    }

    fn push_history(&self, entry: CascadeHistoryEntry) {
        self.history.lock().push(entry.clone());
        let capacity = self
            .options
            .lock()
            .history_capacity
            .unwrap_or(DEFAULT_HISTORY_CAPACITY);
        let overflow = self.history.lock().len().saturating_sub(capacity);
        if overflow > 0 {
            self.history.lock().drain(..overflow);
        }
        self.on_did_cascade.fire(entry);
    }
}

#[derive(Clone, Copy, Default)]
struct AbortState {
    waited: bool,
    timed_out: bool,
}

fn merge_batch(batch: Vec<QueuedRequest>) -> Vec<QueuedRequest> {
    let mut by_key = IndexMap::<String, QueuedRequest>::new();
    for request in batch {
        let key = format!(
            "{}:{}",
            request.engine.merge_key(),
            request.change.token
        );
        by_key.insert(key, request);
    }
    by_key.into_values().collect()
}
