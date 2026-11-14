use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::utils::workdir_slug::{encode_work_dir_key, workspace_root_key};
use crate::app::scopes::LifecycleScope;
use crate::app::workspace::workspace::Workspace;
use crate::wire::record::WireAsyncError;

use super::workspace_aliases::IWorkspaceAliases;

#[derive(Debug, Clone)]
pub struct WorkspaceAliasIndexEntry {
    pub session_id: String,
    pub session_dir: String,
    pub work_dir: String,
}

#[async_trait]
pub trait IWorkspaceAliasesCatalog: Send + Sync {
    async fn prime(&self) -> Result<(), WireAsyncError>;
    async fn stamp(&self) -> Result<Option<String>, WireAsyncError>;
    async fn load(&self) -> Result<Vec<Workspace>, WireAsyncError>;
    fn on_did_change(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn IDisposable>;
}

#[async_trait]
pub trait IWorkspaceAliasesSessionIndex: Send + Sync {
    async fn size(&self) -> Result<Option<u64>, WireAsyncError>;
    async fn entries(&self) -> Result<Vec<WorkspaceAliasIndexEntry>, WireAsyncError>;
    fn on_did_write(&self, listener: Arc<dyn Fn() + Send + Sync>) -> Box<dyn IDisposable>;
}

#[derive(Clone)]
struct CatalogSnapshot {
    by_id: HashMap<String, Workspace>,
    ids_by_root_key: HashMap<String, Vec<String>>,
    stamp: Option<String>,
}

#[derive(Clone)]
struct SessionIndexSnapshot {
    ids_by_root_key: HashMap<String, Vec<String>>,
}

struct SessionIndexCache {
    snapshot: SessionIndexSnapshot,
    size: Option<u64>,
}

pub struct WorkspaceAliasesService {
    catalog: Arc<dyn IWorkspaceAliasesCatalog>,
    session_index: Arc<dyn IWorkspaceAliasesSessionIndex>,
    catalog_cache: Arc<Mutex<Option<CatalogSnapshot>>>,
    session_index_cache: Arc<Mutex<Option<SessionIndexCache>>>,
    catalog_load: tokio::sync::Mutex<()>,
    session_index_load: tokio::sync::Mutex<()>,
    invalidation_generation: Arc<AtomicU64>,
    catalog_merge_primed: AtomicBool,
    subscriptions: Mutex<Vec<Box<dyn IDisposable>>>,
}

impl WorkspaceAliasesService {
    pub fn new(catalog: Arc<dyn IWorkspaceAliasesCatalog>, session_index: Arc<dyn IWorkspaceAliasesSessionIndex>) -> Self {
        let catalog_cache = Arc::new(Mutex::new(None));
        let session_index_cache = Arc::new(Mutex::new(None));
        let invalidation_generation = Arc::new(AtomicU64::new(0));
        let catalog_generation = invalidation_generation.clone();
        let catalog_invalidation = catalog_cache.clone();
        let catalog_subscription = catalog.on_did_change(Arc::new(move || {
            catalog_generation.fetch_add(1, Ordering::AcqRel);
            *catalog_invalidation.lock() = None;
        }));
        let index_generation = invalidation_generation.clone();
        let index_invalidation = session_index_cache.clone();
        let index_subscription = session_index.on_did_write(Arc::new(move || {
            index_generation.fetch_add(1, Ordering::AcqRel);
            *index_invalidation.lock() = None;
        }));
        Self {
            catalog,
            session_index,
            catalog_cache,
            session_index_cache,
            catalog_load: tokio::sync::Mutex::new(()),
            session_index_load: tokio::sync::Mutex::new(()),
            invalidation_generation,
            catalog_merge_primed: AtomicBool::new(false),
            subscriptions: Mutex::new(vec![catalog_subscription, index_subscription]),
        }
    }

    fn generation(&self) -> u64 {
        self.invalidation_generation.load(Ordering::Acquire)
    }

    async fn catalog_snapshot(&self) -> Result<CatalogSnapshot, WireAsyncError> {
        loop {
            if let Some(cache) = self.catalog_cache.lock().clone()
                && self.catalog.stamp().await? == cache.stamp
            {
                return Ok(cache);
            }
            let _load = self.catalog_load.lock().await;
            if let Some(cache) = self.catalog_cache.lock().clone()
                && self.catalog.stamp().await? == cache.stamp
            {
                return Ok(cache);
            }
            let generation = self.generation();
            if !self.catalog_merge_primed.load(Ordering::Acquire) {
                self.catalog.prime().await?;
                self.catalog_merge_primed.store(true, Ordering::Release);
            }
            let stamp = self.catalog.stamp().await?;
            let workspaces = self.catalog.load().await?;
            let snapshot = CatalogSnapshot {
                by_id: workspaces.iter().map(|workspace| (workspace.id.clone(), workspace.clone())).collect(),
                ids_by_root_key: root_key_index(&workspaces, |workspace| workspace.root.clone(), |workspace| workspace.id.clone()),
                stamp,
            };
            if generation == self.generation() {
                *self.catalog_cache.lock() = Some(snapshot.clone());
                return Ok(snapshot);
            }
        }
    }

    async fn session_index_snapshot(&self) -> Result<SessionIndexSnapshot, WireAsyncError> {
        loop {
            if let Some(cache) = self.session_index_cache.lock().as_ref().clone()
                && self.session_index.size().await? == cache.size
            {
                return Ok(cache.snapshot);
            }
            let _load = self.session_index_load.lock().await;
            if let Some(cache) = self.session_index_cache.lock().as_ref().clone()
                && self.session_index.size().await? == cache.size
            {
                return Ok(cache.snapshot);
            }
            let generation = self.generation();
            let entries = self.session_index.entries().await?;
            let snapshot = SessionIndexSnapshot {
                ids_by_root_key: root_key_index(&entries, |entry| entry.work_dir.clone(), |entry| encode_work_dir_key(&entry.work_dir)),
            };
            if generation == self.generation() {
                let size = self.session_index.size().await?;
                *self.session_index_cache.lock() = Some(SessionIndexCache {
                    snapshot: snapshot.clone(),
                    size,
                });
                return Ok(snapshot);
            }
        }
    }

    pub fn dispose(&self) {
        for subscription in self.subscriptions.lock().drain(..) {
            subscription.dispose();
        }
    }
}

#[async_trait]
impl IWorkspaceAliases for WorkspaceAliasesService {
    async fn resolve_alias_ids(&self, id: &str) -> Result<Vec<String>, WireAsyncError> {
        loop {
            let generation = self.generation();
            let (catalog, index) = tokio::try_join!(self.catalog_snapshot(), self.session_index_snapshot())?;
            if generation != self.generation() {
                continue;
            }
            let Some(entry) = catalog.by_id.get(id) else {
                return Ok(vec![id.to_string()]);
            };
            let root_key = workspace_root_key(&entry.root);
            let from_catalog = catalog.ids_by_root_key.get(&root_key);
            let from_index = index.ids_by_root_key.get(&root_key);
            match (from_catalog, from_index) {
                (None, None) => return Ok(vec![id.to_string()]),
                (None, Some(index)) => return Ok(index.clone()),
                (Some(catalog), None) => return Ok(catalog.clone()),
                (Some(catalog), Some(index)) => {
                    let mut merged = catalog.clone();
                    for alias in index {
                        if !merged.contains(alias) {
                            merged.push(alias.clone());
                        }
                    }
                    return Ok(merged);
                }
            }
        }
    }
}

fn root_key_index<T>(items: &[T], root_of: impl Fn(&T) -> String, id_of: impl Fn(&T) -> String) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::<String, Vec<String>>::new();
    for item in items {
        let key = workspace_root_key(&root_of(item));
        let id = id_of(item);
        let bucket = map.entry(key).or_default();
        if !bucket.contains(&id) {
            bucket.push(id);
        }
    }
    map
}

pub fn register_workspace_aliases_service() {
    register_scoped_service(LifecycleScope::App, ScopeActivation::OnScopeCreated, "workspaceAliases");
}
