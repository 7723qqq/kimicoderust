use indexmap::IndexMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use tokio::sync::OnceCell;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::event::{Emitter, Event};
use crate::features::skill::catalog::registry::InMemorySkillCatalog;
use crate::features::skill::catalog::skillSource::SkillContribution;
use crate::features::skill::catalog::types::{SkillCatalog, SkillSummary, summarize_skill};

use super::skillCatalog::{ISessionSkillCatalog, ISkillCatalogSink};
use super::skillCatalogData::ISessionSkillCatalogData;

pub use crate::skills::*;

pub const SKILL_CATALOG_CONTRIBUTIONS_KEY: &str = "sessionSkillCatalog.contributions";
pub const SKILL_CATALOG_MERGED_KEY: &str = "sessionSkillCatalog.merged";

#[derive(Clone)]
struct SessionContribution {
    contribution: SkillContribution,
    priority: i32,
}

pub struct SessionSkillCatalogService {
    data: Arc<dyn ISessionSkillCatalogData>,
    contributions: Mutex<IndexMap<String, SessionContribution>>,
    merged: RwLock<Arc<InMemorySkillCatalog>>,
    ready_cell: OnceCell<Result<(), String>>,
    on_did_change_emitter: Emitter<String>,
    data_subscription: Mutex<Option<Box<dyn IDisposable>>>,
}

impl SessionSkillCatalogService {
    pub fn new(data: Arc<dyn ISessionSkillCatalogData>) -> Arc<Self> {
        let service = Arc::new(Self {
            data,
            contributions: Mutex::new(IndexMap::new()),
            merged: RwLock::new(Arc::new(InMemorySkillCatalog::new())),
            ready_cell: OnceCell::new(),
            on_did_change_emitter: Emitter::new(),
            data_subscription: Mutex::new(None),
        });
        let weak = Arc::downgrade(&service);
        let subscription = service.data.on_did_change().subscribe(move |source_id| {
            let Some(service) = weak.upgrade() else { return };
            service.remerge();
            service.on_did_change_emitter.fire(source_id);
        });
        *service.data_subscription.lock() = Some(Box::new(subscription));
        service.remerge();
        service
    }

    fn remerge(&self) {
        let mut merged = InMemorySkillCatalog::new();
        let base = self.data.catalog();
        for skill in base.list_default_skills() {
            merged.register(skill, true);
        }
        merged.register_variants(&base.list_skills(), true);
        merged.add_roots(&base.get_skill_roots());
        merged.record_skipped(&base.get_skipped_by_policy());
        let mut ordered: Vec<_> = self.contributions.lock().values().cloned().collect();
        ordered.sort_by_key(|entry| entry.priority);
        for entry in ordered {
            for skill in &entry.contribution.skills {
                merged.register(skill.clone(), true);
            }
            merged.register_variants(
                entry
                    .contribution
                    .variants
                    .as_ref()
                    .unwrap_or(&entry.contribution.skills),
                true,
            );
            merged.add_roots(entry.contribution.scanned_roots.as_deref().unwrap_or(&[]));
            merged.record_skipped(entry.contribution.skipped.as_deref().unwrap_or(&[]));
        }
        *self.merged.write() = Arc::new(merged);
    }
}

#[async_trait]
impl ISessionSkillCatalog for SessionSkillCatalogService {
    fn catalog(&self) -> Arc<InMemorySkillCatalog> {
        self.merged.read().clone()
    }

    fn on_did_change(&self) -> Event<String> {
        self.on_did_change_emitter.event()
    }

    async fn ready(&self) -> Result<(), String> {
        self.ready_cell
            .get_or_init(|| async {
                self.data.ready().await?;
                self.remerge();
                Ok(())
            })
            .await
            .clone()
    }

    async fn load(&self) -> Result<(), String> {
        self.ready().await
    }

    async fn reload(&self) -> Result<(), String> {
        self.ready().await?;
        self.remerge();
        self.on_did_change_emitter.fire("catalog".to_string());
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SkillSummary>, String> {
        self.ready().await?;
        Ok(self.catalog().list_skills().iter().map(summarize_skill).collect())
    }
}

impl ISkillCatalogSink for SessionSkillCatalogService {
    fn set(&self, id: String, contribution: SkillContribution, priority: i32) {
        self.contributions.lock().insert(
            id.clone(),
            SessionContribution {
                contribution,
                priority,
            },
        );
        self.remerge();
        self.on_did_change_emitter.fire(id);
    }

    fn remove(&self, id: &str) {
        self.contributions.lock().remove(id);
        self.remerge();
        self.on_did_change_emitter.fire(id.to_string());
    }
}
