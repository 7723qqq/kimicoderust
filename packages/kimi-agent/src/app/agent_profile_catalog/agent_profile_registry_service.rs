use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;

use crate::_base::di::collection::{CollectionChange, CollectionViewImpl};
use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::app::scopes::LifecycleScope;

use super::agent_profile_catalog::AgentProfile;
use super::agent_profile_contribution::{
    AgentProfileContribution, AgentProfileContributionRecord,
};
use super::agent_profile_registry::{
    AgentProfileRegistration, AgentProfileRegistryChange, IAgentProfileRegistry,
};

struct CallbackDisposable {
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl CallbackDisposable {
    fn new(callback: impl FnOnce() + Send + 'static) -> Self {
        Self {
            callback: Mutex::new(Some(Box::new(callback))),
        }
    }
}

impl IDisposable for CallbackDisposable {
    fn dispose(&self) {
        if let Some(callback) = self.callback.lock().take() {
            callback();
        }
    }
}

impl Drop for CallbackDisposable {
    fn drop(&mut self) {
        if let Some(callback) = self.callback.get_mut().take() {
            callback();
        }
    }
}

fn callback_disposable(
    callback: impl FnOnce() + Send + 'static,
) -> DisposableRef {
    Arc::new(CallbackDisposable::new(callback))
}

struct DirectRegistration {
    id: u64,
    registration: AgentProfileRegistration,
}

#[derive(Default)]
struct RegistryState {
    folded: IndexMap<String, AgentProfileContributionRecord>,
    direct: IndexMap<String, DirectRegistration>,
    next_registration_id: u64,
}

pub struct AgentProfileRegistryService {
    view: Arc<CollectionViewImpl<AgentProfileContributionRecord>>,
    state: Mutex<RegistryState>,
    listeners: Arc<Mutex<IndexMap<u64, Arc<dyn Fn(AgentProfileRegistryChange) + Send + Sync>>>>,
    next_listener_id: AtomicU64,
    weak: Mutex<Weak<Self>>,
    _subscription: Mutex<Option<DisposableRef>>,
}

impl AgentProfileRegistryService {
    pub fn new(view: Arc<CollectionViewImpl<AgentProfileContributionRecord>>) -> Arc<Self> {
        let folded = fold_records(&view);
        let this = Arc::new(Self {
            view,
            state: Mutex::new(RegistryState {
                folded,
                ..Default::default()
            }),
            listeners: Arc::new(Mutex::new(IndexMap::new())),
            next_listener_id: AtomicU64::new(0),
            weak: Mutex::new(Weak::new()),
            _subscription: Mutex::new(None),
        });
        *this.weak.lock() = Arc::downgrade(&this);
        let weak = Arc::downgrade(&this);
        let subscription = this.view.on_did_change_event.subscribe(move |change| {
            if let Some(this) = weak.upgrade() {
                this.on_view_change(change);
            }
        });
        *this._subscription.lock() = Some(callback_disposable(move || subscription.dispose()));
        this
    }

    pub fn entries(&self) -> Vec<AgentProfileRegistration> {
        let state = self.state.lock();
        let mut entries = IndexMap::new();
        for record in state.folded.values() {
            entries.insert(
                encode_key(
                    &record.source_id,
                    record.workspace_key.as_deref(),
                    record.context_key.as_deref(),
                ),
                AgentProfileRegistration {
                    source_id: record.source_id.clone(),
                    priority: record.priority.unwrap_or(0),
                    workspace_key: record.workspace_key.clone(),
                    context_key: record.context_key.clone(),
                    contribution: record.contribution.clone(),
                },
            );
        }
        for (key, direct) in &state.direct {
            entries.insert(key.clone(), direct.registration.clone());
        }
        entries.into_values().collect()
    }

    pub fn register(&self, registration: AgentProfileRegistration) -> DisposableRef {
        let key = encode_key(
            &registration.source_id,
            registration.workspace_key.as_deref(),
            registration.context_key.as_deref(),
        );
        let id = {
            let mut state = self.state.lock();
            state.next_registration_id += 1;
            let id = state.next_registration_id;
            state.direct.insert(
                key.clone(),
                DirectRegistration {
                    id,
                    registration,
                },
            );
            id
        };
        self.fire(decode_key(&key));
        let active = Arc::new(AtomicBool::new(true));
        let dispose_active = active.clone();
        let weak = self.weak.lock().clone();
        callback_disposable(move || {
            let Some(this) = weak.upgrade() else {
                return;
            };
            if !dispose_active.load(Ordering::Acquire) {
                return;
            }
            let removed = {
                let mut state = this.state.lock();
                if state.direct.get(&key).is_none_or(|entry| entry.id != id) {
                    false
                } else {
                    dispose_active.store(false, Ordering::Release);
                    state.direct.remove(&key);
                    true
                }
            };
            if removed {
                this.fire(decode_key(&key));
            }
        })
    }

    pub fn on_did_change(
        &self,
        listener: Arc<dyn Fn(AgentProfileRegistryChange) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().insert(id, listener);
        let listeners = Arc::downgrade(&self.listeners);
        callback_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().remove(&id);
            }
        })
    }

    fn on_view_change(&self, change: CollectionChange<AgentProfileContributionRecord>) {
        let previous = self.state.lock().folded.clone();
        let mut affected = IndexSet::new();
        for record in change.removed.iter().chain(change.added.iter()) {
            affected.insert(encode_key(
                &record.source_id,
                record.workspace_key.as_deref(),
                record.context_key.as_deref(),
            ));
        }
        let next = fold_records(&self.view);
        self.state.lock().folded = next.clone();
        for key in affected {
            if !same_record(previous.get(&key), next.get(&key)) {
                self.fire(decode_key(&key));
            }
        }
    }

    fn fire(&self, change: AgentProfileRegistryChange) {
        let listeners = self
            .listeners
            .lock()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for listener in listeners {
            listener(change.clone());
        }
    }
}

impl IAgentProfileRegistry for AgentProfileRegistryService {
    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(AgentProfileRegistryChange) + Send + Sync>,
    ) -> DisposableRef {
        AgentProfileRegistryService::on_did_change(self, listener)
    }

    fn entries(&self) -> Vec<AgentProfileRegistration> {
        AgentProfileRegistryService::entries(self)
    }

    fn register(&self, registration: AgentProfileRegistration) -> DisposableRef {
        AgentProfileRegistryService::register(self, registration)
    }
}

fn fold_records(
    view: &Arc<CollectionViewImpl<AgentProfileContributionRecord>>,
) -> IndexMap<String, AgentProfileContributionRecord> {
    let mut next = IndexMap::new();
    for record in view.records() {
        let record = record.value;
        next.insert(
            encode_key(
                &record.source_id,
                record.workspace_key.as_deref(),
                record.context_key.as_deref(),
            ),
            record,
        );
    }
    next
}

fn encode_key(source_id: &str, workspace_key: Option<&str>, context_key: Option<&str>) -> String {
    serde_json::to_string(&serde_json::json!([source_id, workspace_key, context_key]))
        .expect("agent profile registry key serialization cannot fail")
}

fn decode_key(key: &str) -> AgentProfileRegistryChange {
    let (source_id, workspace_key, context_key): (String, Option<String>, Option<String>) =
        serde_json::from_str(key).expect("agent profile registry keys always decode");
    AgentProfileRegistryChange {
        source_id,
        workspace_key,
        context_key,
    }
}

fn same_record(
    previous: Option<&AgentProfileContributionRecord>,
    next: Option<&AgentProfileContributionRecord>,
) -> bool {
    match (previous, next) {
        (None, None) => true,
        (Some(previous), Some(next)) => {
            previous.source_id == next.source_id
                && previous.priority == next.priority
                && previous.workspace_key == next.workspace_key
                && previous.context_key == next.context_key
                && same_contribution(&previous.contribution, &next.contribution)
        }
        _ => false,
    }
}

fn same_contribution(previous: &AgentProfileContribution, next: &AgentProfileContribution) -> bool {
    previous.skipped == next.skipped
        && previous.scanned_roots == next.scanned_roots
        && previous.profiles.len() == next.profiles.len()
        && previous
            .profiles
            .iter()
            .zip(&next.profiles)
            .all(|(previous, next)| same_profile(previous, next))
}

fn same_profile(previous: &AgentProfile, next: &AgentProfile) -> bool {
    let same_prefix = match (&previous.prompt_prefix, &next.prompt_prefix) {
        (None, None) => true,
        (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
        _ => false,
    };
    previous.name == next.name
        && previous.description == next.description
        && previous.when_to_use == next.when_to_use
        && previous.override_ == next.override_
        && previous.tools == next.tools
        && previous.disallowed_tools == next.disallowed_tools
        && previous.subagents == next.subagents
        && previous.summary_policy == next.summary_policy
        && Arc::ptr_eq(&previous.system_prompt, &next.system_prompt)
        && Arc::ptr_eq(
            &previous.render_system_prompt,
            &next.render_system_prompt,
        )
        && same_prefix
}

pub fn register_agent_profile_registry_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "agentProfileCatalog",
    );
}
