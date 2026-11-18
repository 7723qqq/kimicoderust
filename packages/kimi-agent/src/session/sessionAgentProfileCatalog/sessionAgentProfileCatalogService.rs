use std::sync::{Arc, Weak};

use async_trait::async_trait;
use indexmap::IndexMap;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::app::agent_profile_catalog::agent_profile_catalog::{
    AgentProfile, DEFAULT_AGENT_PROFILE_NAME,
};
use crate::app::agent_profile_catalog::agent_profile_registry::{
    AgentProfileRegistration, IAgentProfileRegistry,
};
use crate::app::agent_profile_catalog::builtin_agent_profile_loader::BUILTIN_AGENT_PROFILE_SOURCE_ID;
use crate::app::scopes::LifecycleScope;
use crate::wire::record::WireAsyncError;

use super::agent_profile_catalog_seed::ISessionAgentProfileCatalogSeed;
use super::session_agent_profile_catalog::{
    AgentProfileInspection, AgentProfileSuppressedCandidate, AgentProfileSuppressedReason,
    ISessionAgentProfileCatalog,
};

#[derive(Clone)]
struct ProfileCandidate {
    profile: Arc<AgentProfile>,
    source_id: String,
    priority: i32,
}

#[derive(Default)]
struct CatalogProjection {
    merged: IndexMap<String, Arc<AgentProfile>>,
    inspections: IndexMap<String, AgentProfileInspection>,
}

pub struct SessionAgentProfileCatalogService {
    registry: Arc<dyn IAgentProfileRegistry>,
    seed: Arc<dyn ISessionAgentProfileCatalogSeed>,
    log: Arc<dyn ILogService>,
    projection: Arc<Mutex<CatalogProjection>>,
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(String) + Send + Sync>)>>>,
    next_listener_id: std::sync::atomic::AtomicU64,
    weak: Mutex<Weak<Self>>,
    _subscription: Mutex<Option<DisposableRef>>,
}

impl SessionAgentProfileCatalogService {
    pub fn new(
        registry: Arc<dyn IAgentProfileRegistry>,
        seed: Arc<dyn ISessionAgentProfileCatalogSeed>,
        log: Arc<dyn ILogService>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            registry,
            seed,
            log,
            projection: Arc::new(Mutex::new(CatalogProjection::default())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: std::sync::atomic::AtomicU64::new(0),
            weak: Mutex::new(Weak::new()),
            _subscription: Mutex::new(None),
        });
        *this.weak.lock() = Arc::downgrade(&this);
        this.reproject();
        let weak = Arc::downgrade(&this);
        let subscription = this.registry.on_did_change(Arc::new(move |change| {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if change.workspace_key.as_deref().is_some_and(|key| key != this.seed.workspace_key()) {
                return;
            }
            if change.context_key.as_deref().is_some_and(|key| Some(key) != this.seed.context_key()) {
                return;
            }
            this.reproject();
            this.fire(change.source_id);
        }));
        *this._subscription.lock() = Some(subscription);
        this
    }

    pub fn ready(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<AgentProfile>> {
        self.projection.lock().merged.get(name).cloned()
    }

    pub fn get_default(&self) -> Arc<AgentProfile> {
        self.get(DEFAULT_AGENT_PROFILE_NAME).unwrap_or_else(|| {
            panic!("Default agent profile \"{DEFAULT_AGENT_PROFILE_NAME}\" is not registered")
        })
    }

    pub fn list(&self) -> Vec<Arc<AgentProfile>> {
        self.projection.lock().merged.values().cloned().collect()
    }

    pub fn inspect(&self, name: &str) -> Option<AgentProfileInspection> {
        self.projection.lock().inspections.get(name).cloned()
    }

    pub fn reload(&self) {
        self.reproject();
        self.fire("catalog".to_string());
    }

    fn relevant_entries(&self) -> Vec<AgentProfileRegistration> {
        let workspace_key = self.seed.workspace_key();
        let context_key = self.seed.context_key();
        self.registry
            .entries()
            .into_iter()
            .filter(|entry| {
                entry.workspace_key.as_deref().is_none_or(|key| key == workspace_key)
                    && entry.context_key.as_deref().is_none_or(|key| Some(key) == context_key)
            })
            .collect()
    }

    fn reproject(&self) {
        let mut merged: IndexMap<String, Arc<AgentProfile>> = IndexMap::new();
        let mut inspections: IndexMap<String, AgentProfileInspection> = IndexMap::new();
        let entries = self.relevant_entries();
        if let Some(builtin_entry) = entries
            .iter()
            .find(|entry| entry.source_id == BUILTIN_AGENT_PROFILE_SOURCE_ID)
        {
            for profile in &builtin_entry.contribution.profiles {
                merged.insert(profile.name.clone(), Arc::new(profile.clone()));
                inspections.insert(
                    profile.name.clone(),
                    AgentProfileInspection {
                        name: profile.name.clone(),
                        profile: Arc::new(profile.clone()),
                        source_id: builtin_entry.source_id.clone(),
                        priority: builtin_entry.priority as i32,
                        suppressed: Vec::new(),
                    },
                );
            }
        }

        let mut file_candidates = IndexMap::<String, Vec<ProfileCandidate>>::new();
        let mut ordered = entries
            .iter()
            .filter(|entry| entry.source_id != BUILTIN_AGENT_PROFILE_SOURCE_ID)
            .cloned()
            .collect::<Vec<_>>();
        ordered.sort_by(|left, right| right.priority.cmp(&left.priority));
        for entry in ordered {
            let mut entry_profiles = IndexMap::new();
            for profile in entry.contribution.profiles {
                entry_profiles.insert(profile.name.clone(), profile);
            }
            for profile in entry_profiles.into_values() {
                file_candidates
                    .entry(profile.name.clone())
                    .or_default()
                    .push(ProfileCandidate {
                        profile: Arc::new(profile),
                        source_id: entry.source_id.clone(),
                        priority: entry.priority as i32,
                    });
            }
        }

        for candidates in file_candidates.into_values() {
            let mut suppressed = Vec::new();
            let mut winner = false;
            for (index, candidate) in candidates.iter().enumerate() {
                if merged.contains_key(&candidate.profile.name)
                    && candidate.profile.override_ != Some(true)
                {
                    self.log.warn(
                        &format!(
                            "agent file profile \"{}\" ignored: a same-name builtin profile exists; set \"override: true\" in the frontmatter to replace it",
                            candidate.profile.name
                        ),
                        None,
                    );
                    suppressed.push(AgentProfileSuppressedCandidate {
                        source_id: candidate.source_id.clone(),
                        priority: candidate.priority,
                        reason: AgentProfileSuppressedReason::BuiltinOverrideRequired,
                    });
                    continue;
                }
                let replaced = merged.get(&candidate.profile.name).cloned();
                let effective = if candidate.profile.subagents.is_none()
                    && replaced.as_ref().is_some_and(|replaced| replaced.subagents.is_some())
                {
                    let mut profile = (*candidate.profile).clone();
                    profile.subagents = replaced.unwrap().subagents.clone();
                    Arc::new(profile)
                } else {
                    candidate.profile.clone()
                };
                merged.insert(candidate.profile.name.clone(), effective.clone());
                let mut candidate_suppressed = suppressed.clone();
                candidate_suppressed.extend(candidates[index + 1..].iter().map(|rest| {
                    AgentProfileSuppressedCandidate {
                        source_id: rest.source_id.clone(),
                        priority: rest.priority,
                        reason: AgentProfileSuppressedReason::Priority,
                    }
                }));
                inspections.insert(
                    candidate.profile.name.clone(),
                    AgentProfileInspection {
                        name: candidate.profile.name.clone(),
                        profile: effective,
                        source_id: candidate.source_id.clone(),
                        priority: candidate.priority,
                        suppressed: candidate_suppressed,
                    },
                );
                winner = true;
                break;
            }
            if !winner && !suppressed.is_empty()
                && let Some(name) = candidates.first().map(|candidate| candidate.profile.name.clone())
                && let Some(existing) = inspections.get_mut(&name)
            {
                existing.suppressed = suppressed;
            }
        }
        *self.projection.lock() = CatalogProjection {
            merged,
            inspections,
        };
    }

    fn fire(&self, source_id: String) {
        let listeners = self.listeners.lock().clone();
        for (_, listener) in listeners {
            listener(source_id.clone());
        }
    }
}

#[async_trait]
impl ISessionAgentProfileCatalog for SessionAgentProfileCatalogService {
    async fn ready(&self) -> Result<(), WireAsyncError> {
        Ok(())
    }

    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(String) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.listeners);
        crate::_base::di::lifecycle::to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    fn get(&self, name: &str) -> Option<Arc<AgentProfile>> {
        SessionAgentProfileCatalogService::get(self, name)
    }

    fn get_default(&self) -> Arc<AgentProfile> {
        SessionAgentProfileCatalogService::get_default(self)
    }

    fn list(&self) -> Vec<Arc<AgentProfile>> {
        SessionAgentProfileCatalogService::list(self)
    }

    fn inspect(&self, name: &str) -> Option<AgentProfileInspection> {
        SessionAgentProfileCatalogService::inspect(self, name)
    }

    async fn load(&self) -> Result<(), WireAsyncError> {
        SessionAgentProfileCatalogService::ready(self)
    }

    async fn reload(&self) -> Result<(), WireAsyncError> {
        SessionAgentProfileCatalogService::ready(self)?;
        SessionAgentProfileCatalogService::reload(self);
        Ok(())
    }
}
