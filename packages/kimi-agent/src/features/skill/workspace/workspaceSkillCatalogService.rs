use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::future::join_all;
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use tokio::sync::{Mutex as AsyncMutex, OnceCell};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::event::{Emitter, Event};
use crate::features::skill::catalog::registry::InMemorySkillCatalog;
use crate::features::skill::catalog::skillSource::{ISkillSource, SkillContribution};
use crate::features::skill::catalog::types::SkillCatalog;
use crate::features::skill::session::skillCatalogData::ISessionSkillCatalogData;

use super::workspaceSkillCatalog::IWorkspaceSkillCatalog;

pub use crate::skills::*;

pub const WORKSPACE_SKILL_CATALOG_CONTRIBUTIONS_KEY: &str = "workspaceSkillCatalog.contributions";
pub const WORKSPACE_SKILL_CATALOG_MERGED_KEY: &str = "workspaceSkillCatalog.merged";

#[derive(Clone)]
struct PrioritizedContribution {
    contribution: SkillContribution,
    priority: i32,
}

pub struct WorkspaceSkillCatalogService {
    sources: Vec<Arc<dyn ISkillSource>>,
    source_load_locks: BTreeMap<String, Arc<AsyncMutex<()>>>,
    contributions: Arc<Mutex<IndexMap<String, PrioritizedContribution>>>,
    merged: Arc<RwLock<Arc<InMemorySkillCatalog>>>,
    ready_cell: OnceCell<Result<(), String>>,
    on_did_change_emitter: Emitter<String>,
    self_weak: std::sync::Weak<Self>,
    source_subscriptions: Mutex<Vec<DisposableRef>>,
}

impl WorkspaceSkillCatalogService {
    pub fn new(mut sources: Vec<Arc<dyn ISkillSource>>) -> Arc<Self> {
        sources.sort_by_key(|source| source.priority());
        let source_load_locks = sources
            .iter()
            .map(|source| (source.id().to_string(), Arc::new(AsyncMutex::new(()))))
            .collect();
        let service = Arc::new_cyclic(|weak| Self {
            sources,
            source_load_locks,
            contributions: Arc::new(Mutex::new(IndexMap::new())),
            merged: Arc::new(RwLock::new(Arc::new(InMemorySkillCatalog::new()))),
            ready_cell: OnceCell::new(),
            on_did_change_emitter: Emitter::new(),
            self_weak: weak.clone(),
            source_subscriptions: Mutex::new(Vec::new()),
        });
        let mut subscriptions: Vec<DisposableRef> = Vec::new();
        for source in &service.sources {
            let Some(event) = source.on_did_change() else { continue };
            let weak = Arc::downgrade(&service);
            let id = source.id().to_string();
            subscriptions.push(Arc::new(event.subscribe(move |_| {
                let Some(service) = weak.upgrade() else { return };
                let id = id.clone();
                spawn_reload(service, id);
            })));
        }
        *service.source_subscriptions.lock() = subscriptions;
        service
    }

    fn remerge(&self) {
        let mut merged = InMemorySkillCatalog::new();
        let mut ordered: Vec<_> = self.contributions.lock().values().cloned().collect();
        ordered.sort_by_key(|entry| entry.priority);
        for entry in ordered {
            for skill in &entry.contribution.skills {
                merged.register(skill.clone(), true);
            }
            let variants = entry
                .contribution
                .variants
                .as_ref()
                .unwrap_or(&entry.contribution.skills);
            merged.register_variants(variants, true);
            merged.add_roots(entry.contribution.scanned_roots.as_deref().unwrap_or(&[]));
            merged.record_skipped(entry.contribution.skipped.as_deref().unwrap_or(&[]));
        }
        *self.merged.write() = Arc::new(merged);
    }

    async fn load_all(&self) -> Result<(), String> {
        for source in &self.sources {
            self.load_source(source.clone(), false).await?;
        }
        self.remerge();
        Ok(())
    }

    async fn reload_source(&self, id: &str) -> Result<(), String> {
        let Some(source) = self.sources.iter().find(|source| source.id() == id) else {
            return Ok(());
        };
        self.load_source(source.clone(), true).await
    }

    async fn load_source(&self, source: Arc<dyn ISkillSource>, fire_change: bool) -> Result<(), String> {
        let lock = self.source_load_locks.get(source.id()).cloned().unwrap();
        let _guard = lock.lock().await;
        let contribution = source.load().await?;
        self.contributions.lock().insert(
            source.id().to_string(),
            PrioritizedContribution {
                contribution,
                priority: source.priority(),
            },
        );
        if fire_change {
            self.remerge();
            self.on_did_change_emitter.fire(source.id().to_string());
        }
        Ok(())
    }
}

#[async_trait]
impl IWorkspaceSkillCatalog for WorkspaceSkillCatalogService {
    fn catalog(&self) -> Arc<InMemorySkillCatalog> {
        self.merged.read().clone()
    }

    fn on_did_change(&self) -> Event<String> {
        self.on_did_change_emitter.event()
    }

    async fn ready(&self) -> Result<(), String> {
        self.ready_cell.get_or_init(|| self.load_all()).await.clone()
    }

    async fn load(&self) -> Result<(), String> {
        self.ready().await
    }

    async fn reload(&self) -> Result<(), String> {
        self.load_all().await?;
        self.on_did_change_emitter.fire("catalog".to_string());
        Ok(())
    }

    async fn reload_sources(&self, ids: &[String]) -> Result<(), String> {
        let results = join_all(ids.iter().map(|id| self.reload_source(id))).await;
        for result in results {
            result?;
        }
        Ok(())
    }

    fn session_data(&self) -> Arc<dyn ISessionSkillCatalogData> {
        Arc::new(WorkspaceSessionSkillCatalogData {
            service: self.self_weak.upgrade().unwrap(),
        })
    }
}

struct WorkspaceSessionSkillCatalogData {
    service: Arc<WorkspaceSkillCatalogService>,
}

#[async_trait]
impl ISessionSkillCatalogData for WorkspaceSessionSkillCatalogData {
    fn catalog(&self) -> Arc<InMemorySkillCatalog> {
        self.service.catalog()
    }

    fn on_did_change(&self) -> Event<String> {
        self.service.on_did_change()
    }

    async fn ready(&self) -> Result<(), String> {
        self.service.ready().await
    }
}

fn spawn_reload(service: Arc<WorkspaceSkillCatalogService>, id: String) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move {
            let _ = service.reload_source(&id).await;
        });
        return;
    }
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        runtime.block_on(async move {
            let _ = service.reload_source(&id).await;
        });
    });
}
