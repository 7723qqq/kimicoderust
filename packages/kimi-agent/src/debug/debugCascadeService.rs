use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::_base::di::cascade_engine::CascadeEngine;
use crate::_base::di::instantiation::{IInstantiationService, ServiceIdentifier};
use crate::_base::di::instantiation_service::InstantiationService;
use crate::_base::di::lifecycle::{DisposableRef, DisposableStore, IDisposable};
use crate::app::event::event::IEventService;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

use super::debug_cascade::{
    DebugCascadeEntry, DebugFailedUnit, DebugPendingGroup, DebugPendingUnit, DiUnitChanged,
    DiUnitChangedPayload, IDebugCascadeService,
};
use super::errors::{DEBUG_SCOPE_NOT_FOUND, DEBUG_TOKEN_NOT_FOUND};
use super::scope_tree::{resolve_scope_container, scope_path_of_engine, walk_scope_containers};

pub struct DebugCascadeService {
    root: Arc<InstantiationService>,
    events: Arc<dyn IEventService>,
    store: Mutex<DisposableStore>,
    engine_subscriptions: Arc<Mutex<HashMap<usize, DisposableRef>>>,
    torn_down: std::sync::atomic::AtomicBool,
}

impl DebugCascadeService {
    pub fn new(instantiation: Arc<dyn IInstantiationService>, events: Arc<dyn IEventService>) -> Self {
        let root = instantiation.as_concrete();
        let this = Self {
            root: root.clone(),
            events,
            store: Mutex::new(DisposableStore::new()),
            engine_subscriptions: Arc::new(Mutex::new(HashMap::new())),
            torn_down: std::sync::atomic::AtomicBool::new(false),
        };
        let tree = root.cascade_tree.clone();
        for engine in tree.engines() {
            this.watch_engine(engine);
        }
        let subscriptions = this.engine_subscriptions.clone();
        this.store.lock().add(tree.on_did_add_engine(Arc::new(move |engine| {
            let subscriptions = subscriptions.clone();
            let root = root.clone();
            let events = this.events.clone();
            watch_engine(&root, &events, &subscriptions, engine);
        })));
        let subscriptions = this.engine_subscriptions.clone();
        this.store.lock().add(tree.on_did_remove_engine(Arc::new(move |engine| {
            unwatch_engine(&subscriptions, &engine);
        })));
        this
    }

    pub fn history(&self) -> Vec<DebugCascadeEntry> {
        let mut entries = Vec::new();
        for info in walk_scope_containers(&self.root) {
            for entry in info.container.cascade.history() {
                entries.push(DebugCascadeEntry {
                    scope_path: info.path,
                    ..entry
                });
            }
        }
        entries.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.scope_path.cmp(&b.scope_path)));
        entries
    }

    pub fn pending(&self) -> Vec<DebugPendingGroup> {
        let mut groups = Vec::new();
        for info in walk_scope_containers(&self.root) {
            let waiting = info
                .container
                .cascade
                .pending_snapshot()
                .into_iter()
                .map(|(token, missing)| DebugPendingUnit { token, missing })
                .collect::<Vec<_>>();
            let failed = info
                .container
                .cascade
                .units_snapshot()
                .into_iter()
                .filter(|unit| unit.state == crate::_base::di::cascade_engine::UnitState::Failed)
                .map(|unit| DebugFailedUnit {
                    token: unit.token,
                    error: unit.error,
                })
                .collect::<Vec<_>>();
            if !waiting.is_empty() || !failed.is_empty() {
                groups.push(DebugPendingGroup {
                    scope_path: info.path,
                    waiting,
                    failed,
                });
            }
        }
        groups
    }

    pub async fn unprovide(&self, scope_path: &str, token: &str) -> Result<(), WireAsyncError> {
        let (container, id) = self.resolve(scope_path, token)?;
        container.unprovide(id);
        container.cascade.when_idle().await
    }

    pub async fn update(&self, scope_path: &str, token: &str, config: Option<serde_json::Value>) -> Result<(), WireAsyncError> {
        let (container, id) = self.resolve(scope_path, token)?;
        match config {
            Some(config) => container.fiber_host.update_token(id, config, true).await,
            None => container.cascade.update(id, &format!("debug update {token}")).await,
        }
    }

    pub async fn dispose(&self, scope_path: Option<&str>, token: Option<&str>) -> Result<(), WireAsyncError> {
        if scope_path.is_none() && token.is_none() {
            if !self.torn_down.swap(true, std::sync::atomic::Ordering::Relaxed) {
                self.store.lock().dispose();
                let subscriptions = self
                    .engine_subscriptions
                    .lock()
                    .drain()
                    .map(|(_, subscription)| subscription)
                    .collect::<Vec<_>>();
                for subscription in subscriptions {
                    subscription.dispose();
                }
            }
            return Ok(());
        }
        let (Some(scope_path), Some(token)) = (scope_path, token) else {
            return Err(Box::new(Error2::new(
                DEBUG_TOKEN_NOT_FOUND,
                "dispose requires both a scope path and a token",
            )));
        };
        let (container, id) = self.resolve(scope_path, token)?;
        container
            .cascade
            .submit(crate::_base::di::cascade_engine::CascadeChange {
                action: crate::_base::di::cascade_engine::CascadeAction::Unprovide,
                token: id,
                descriptor: None,
                instance: None,
                activation: None,
                config: None,
                reason: format!("debug dispose {token}"),
            })
            .await
    }

    fn resolve(&self, scope_path: &str, token: &str) -> Result<(Arc<InstantiationService>, ServiceIdentifier), WireAsyncError> {
        let container = resolve_scope_container(&self.root, scope_path).ok_or_else(|| {
            Box::new(Error2::new(
                DEBUG_SCOPE_NOT_FOUND,
                format!("no DI container at scope path '{scope_path}'"),
            )) as WireAsyncError
        })?;
        let id = container.find_identifier(token).ok_or_else(|| {
            Box::new(Error2::new(
                DEBUG_TOKEN_NOT_FOUND,
                format!("token '{token}' is not registered in container '{scope_path}'"),
            )) as WireAsyncError
        })?;
        Ok((container, id))
    }

    fn watch_engine(&self, engine: Arc<CascadeEngine>) {
        watch_engine(&self.root, &self.events, &self.engine_subscriptions, engine);
    }
}

#[async_trait]
impl IDebugCascadeService for DebugCascadeService {
    fn history(&self) -> Vec<DebugCascadeEntry> {
        DebugCascadeService::history(self)
    }

    fn pending(&self) -> Vec<DebugPendingGroup> {
        DebugCascadeService::pending(self)
    }

    async fn unprovide(&self, scope_path: &str, token: &str) -> Result<(), WireAsyncError> {
        DebugCascadeService::unprovide(self, scope_path, token).await
    }

    async fn update(&self, scope_path: &str, token: &str, config: Option<serde_json::Value>) -> Result<(), WireAsyncError> {
        DebugCascadeService::update(self, scope_path, token, config).await
    }

    async fn dispose(&self, scope_path: Option<&str>, token: Option<&str>) -> Result<(), WireAsyncError> {
        DebugCascadeService::dispose(self, scope_path, token).await
    }
}

fn watch_engine(
    root: &Arc<InstantiationService>,
    events: &Arc<dyn IEventService>,
    subscriptions: &Arc<Mutex<HashMap<usize, DisposableRef>>>,
    engine: Arc<CascadeEngine>,
) {
    let address = Arc::as_ptr(&engine) as usize;
    if subscriptions.lock().contains_key(&address) {
        return;
    }
    let root = root.clone();
    let events = events.clone();
    let subscription = engine.on_did_change_unit_state(Arc::new(move |change| {
        let payload = DiUnitChangedPayload {
            scope: scope_path_of_engine(&root, &engine).unwrap_or_else(|| "#unknown".to_string()),
            token: change.token,
            state: change.state,
            error: change.error,
        };
        events.publish(DiUnitChanged::new(payload));
    }));
    subscriptions.lock().insert(address, subscription);
}

fn unwatch_engine(subscriptions: &Arc<Mutex<HashMap<usize, DisposableRef>>>, engine: &Arc<CascadeEngine>) {
    subscriptions.lock().remove(&(Arc::as_ptr(engine) as usize));
}
