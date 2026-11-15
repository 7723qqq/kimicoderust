use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::agent::r#loop::r#loop::I_AGENT_LOOP_SERVICE;
use crate::agent::task::task::I_AGENT_TASK_SERVICE;
use crate::state::event_dispatcher::I_EVENT_DISPATCHER;
use std::collections::HashSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::json;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::di::scope::{IAgentScopeHandle, ScopeActivation, register_scoped_service};
use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::_base::log::log::ILogService;
use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::agent::task::task::IAgentTaskService;
use crate::app::event::event_bus::ISessionEventBus;
use crate::app::scopes::LifecycleScope;
use crate::features::swarm::session::session_swarm_service::SubagentSuspended;
use crate::os::interface::host_timers::{IHostTimerHandle, IHostTimers};
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

use super::mirror_agent_run::{SubagentCancelled, SubagentCompleted, SubagentFailed, SubagentStarted};
use super::subagent_scope_cache::{
    ISessionSubagentScopeCacheService, resolve_subagent_scope_cache_size,
    resolve_subagent_scope_evict_timeout_ms,
};

pub const MAX_EVICT_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictOutcome {
    Removed,
    Missing,
    Closing,
    Deferred,
    Timeout,
    Failed,
}

pub struct SessionSubagentScopeCacheService {
    agent_lifecycle: Arc<dyn IAgentLifecycleService>,
    log: Arc<dyn ILogService>,
    capacity: usize,
    remove_timeout_ms: u64,
    timers: Option<Arc<dyn IHostTimers>>,
    retired: Arc<Mutex<IndexMap<String, usize>>>,
    closing: Arc<Mutex<HashSet<String>>>,
    evictions: Arc<Mutex<Shared<BoxFuture<'static, ()>>>>,
    weak: Mutex<Weak<Self>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl SessionSubagentScopeCacheService {
    pub fn new(
        agent_lifecycle: Arc<dyn IAgentLifecycleService>,
        bus: Arc<dyn ISessionEventBus>,
        log: Arc<dyn ILogService>,
        timers: Option<Arc<dyn IHostTimers>>,
    ) -> Result<Arc<Self>, WireAsyncError> {
        let this = Arc::new(Self {
            agent_lifecycle,
            log,
            capacity: resolve_subagent_scope_cache_size(None)?,
            remove_timeout_ms: resolve_subagent_scope_evict_timeout_ms(None)?,
            timers,
            retired: Arc::new(Mutex::new(IndexMap::new())),
            closing: Arc::new(Mutex::new(HashSet::new())),
            evictions: Arc::new(Mutex::new(
                async {}.boxed().shared(),
            )),
            weak: Mutex::new(Weak::new()),
            subscriptions: Mutex::new(Vec::new()),
        });
        *this.weak.lock() = Arc::downgrade(&this);
        if this.capacity == 0 {
            return Ok(this);
        }
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(bus.subscribe_subagent_completed(
            Arc::new(move |event: SubagentCompleted| {
                if let Some(this) = weak.upgrade() {
                    this.retire(event.subagent_id);
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(bus.subscribe_subagent_failed(
            Arc::new(move |event: SubagentFailed| {
                if let Some(this) = weak.upgrade() {
                    this.retire(event.subagent_id);
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(bus.subscribe_subagent_cancelled(
            Arc::new(move |event: SubagentCancelled| {
                if let Some(this) = weak.upgrade() {
                    this.retire(event.subagent_id);
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(bus.subscribe_subagent_started(
            Arc::new(move |event: SubagentStarted| {
                if let Some(this) = weak.upgrade() {
                    this.revive(&event.subagent_id);
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(bus.subscribe_subagent_suspended(
            Arc::new(move |event: SubagentSuspended| {
                if let Some(this) = weak.upgrade() {
                    this.revive(&event.subagent_id);
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(this.agent_lifecycle.on_did_create(
            Arc::new(move |agent| {
                if let Some(this) = weak.upgrade() {
                    this.revive(&agent.agent_id);
                }
            }),
        ));
        let closing = this.closing.clone();
        this.subscriptions.lock().push(this.agent_lifecycle.on_will_close(
            Arc::new(move |agent| {
                closing.lock().insert(agent.agent_id.clone());
            }),
        ));
        let closing = this.closing.clone();
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(this.agent_lifecycle.on_did_close(
            Arc::new(move |agent| {
                closing.lock().remove(&agent.agent_id);
                if let Some(this) = weak.upgrade() {
                    this.revive(&agent.agent_id);
                }
            }),
        ));
        Ok(this)
    }

    fn retire(self: &Arc<Self>, agent_id: String) {
        self.retired.lock().shift_remove(&agent_id);
        self.retired.lock().insert(agent_id, 0);
        let previous = self.evictions.lock().clone();
        let this = self.clone();
        let next = async move {
            previous.await;
            if let Err(error) = this.evict_overflow().await {
                on_unexpected_error(error);
            }
        }
        .boxed()
        .shared();
        *self.evictions.lock() = next;
    }

    fn revive(&self, agent_id: &str) {
        self.retired.lock().shift_remove(agent_id);
    }

    async fn evict_overflow(self: &Arc<Self>) -> Result<(), WireAsyncError> {
        let mut skipped = HashSet::new();
        while self.retired.lock().len() > self.capacity {
            let Some((agent_id, attempts)) = self.oldest_candidate(&skipped) else {
                return Ok(());
            };
            self.retired.lock().shift_remove(&agent_id);
            let outcome = self.evict(&agent_id).await;
            if matches!(outcome, EvictOutcome::Removed | EvictOutcome::Missing) {
                continue;
            }
            skipped.insert(agent_id.clone());
            if outcome == EvictOutcome::Deferred {
                self.log.debug(
                    "subagent scope eviction deferred; agent still busy",
                    Some(json!({ "agentId": agent_id })),
                );
                if !self.retired.lock().contains_key(&agent_id) {
                    self.retired.lock().insert(agent_id, attempts);
                }
                continue;
            }
            if matches!(outcome, EvictOutcome::Closing | EvictOutcome::Failed) {
                if !self.retired.lock().contains_key(&agent_id) {
                    self.retired.lock().insert(agent_id, attempts);
                }
                continue;
            }
            let next_attempt = attempts + 1;
            if next_attempt >= MAX_EVICT_ATTEMPTS {
                self.log.warn(
                    "subagent scope eviction abandoned; agent still busy",
                    Some(json!({ "agentId": agent_id, "attempts": next_attempt })),
                );
            }
            if !self.retired.lock().contains_key(&agent_id) {
                self.retired.lock().insert(agent_id, next_attempt);
            }
        }
        Ok(())
    }

    fn oldest_candidate(&self, skipped: &HashSet<String>) -> Option<(String, usize)> {
        self.retired
            .lock()
            .iter()
            .find(|(agent_id, attempts)| {
                **attempts < MAX_EVICT_ATTEMPTS && !skipped.contains(*agent_id)
            })
            .map(|(agent_id, attempts)| (agent_id.clone(), *attempts))
    }

    async fn evict(&self, agent_id: &str) -> EvictOutcome {
        let Some(context) = self.agent_lifecycle.get(agent_id) else {
            return if self.closing.lock().contains(agent_id) {
                EvictOutcome::Closing
            } else {
                EvictOutcome::Missing
            };
        };
        let Some(handle) = self.agent_lifecycle.handle_of(agent_id) else {
            return if self.closing.lock().contains(agent_id) {
                EvictOutcome::Closing
            } else {
                EvictOutcome::Missing
            };
        };
        if self.busy(&handle) {
            return EvictOutcome::Deferred;
        }
        if let Err(error) = handle.accessor().get(&I_EVENT_DISPATCHER).flush().await {
            self.log.warn(
                "subagent scope eviction skipped; wire flush failed",
                Some(json!({ "agentId": agent_id, "error": error.to_string() })),
            );
            return EvictOutcome::Failed;
        }
        if self
            .agent_lifecycle
            .handle_of(agent_id)
            .is_none_or(|current| !Arc::ptr_eq(&current, &handle))
        {
            return if self.closing.lock().contains(agent_id) {
                EvictOutcome::Closing
            } else {
                EvictOutcome::Missing
            };
        }
        if self.busy(&handle) {
            return EvictOutcome::Deferred;
        }
        let started_at = std::time::Instant::now();
        let lifecycle = self.agent_lifecycle.clone();
        let removal = tokio::spawn(async move { lifecycle.remove(context).await });
        let (outcome, timer) = race_removal(removal, self.timers.clone(), self.remove_timeout_ms).await;
        if let (Some(timers), Some(timer)) = (self.timers.clone(), timer) {
            timers.clear_timeout(timer);
        }
        let duration_ms = started_at.elapsed().as_millis() as u64;
        match outcome {
            RemovalRace::Timeout => {
                self.log.warn(
                    "subagent scope eviction timed out; moving on to the next eviction",
                    Some(json!({ "agentId": agent_id, "durationMs": duration_ms })),
                );
                EvictOutcome::Timeout
            }
            RemovalRace::Removed => {
                self.log.debug(
                    "subagent scope evicted",
                    Some(json!({ "agentId": agent_id, "durationMs": duration_ms })),
                );
                EvictOutcome::Removed
            }
            RemovalRace::Failed(error) => {
                let gone = self.agent_lifecycle.get(agent_id).is_none()
                    && !self.closing.lock().contains(agent_id);
                self.log.warn(
                    "subagent scope eviction failed",
                    Some(json!({
                        "agentId": agent_id,
                        "durationMs": duration_ms,
                        "error": error,
                    })),
                );
                if gone { EvictOutcome::Removed } else { EvictOutcome::Failed }
            }
        }
    }

    fn busy(&self, handle: &Arc<IAgentScopeHandle>) -> bool {
        let snapshot = handle.accessor().get(&I_AGENT_LOOP_SERVICE).snapshot();
        if snapshot.state == "running" || snapshot.has_pending_requests {
            return true;
        }
        !handle.accessor().get(&I_AGENT_TASK_SERVICE).list(true).is_empty()
    }
}

impl ISessionSubagentScopeCacheService for SessionSubagentScopeCacheService {}

enum RemovalRace {
    Removed,
    Timeout,
    Failed(String),
}

async fn race_removal(
    mut removal: tokio::task::JoinHandle<Result<(), WireAsyncError>>,
    timers: Option<Arc<dyn IHostTimers>>,
    timeout_ms: u64,
) -> (RemovalRace, Option<IHostTimerHandle>) {
    if let Some(timers) = timers {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = std::sync::Mutex::new(Some(sender));
        let timer = timers.set_timeout(
            Arc::new(move || {
                if let Some(sender) = sender.lock().unwrap_or_else(|error| error.into_inner()).take() {
                    let _ = sender.send(());
                }
            }),
            timeout_ms,
        );
        let outcome = tokio::select! {
            result = &mut removal => removal_outcome(result),
            _ = receiver => RemovalRace::Timeout,
        };
        (outcome, Some(timer))
    } else {
        let outcome = tokio::select! {
            result = &mut removal => removal_outcome(result),
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => RemovalRace::Timeout,
        };
        (outcome, None)
    }
}

fn removal_outcome(
    result: Result<Result<(), WireAsyncError>, tokio::task::JoinError>,
) -> RemovalRace {
    match result {
        Ok(Ok(())) => RemovalRace::Removed,
        Ok(Err(error)) => RemovalRace::Failed(error.to_string()),
        Err(error) => RemovalRace::Failed(error.to_string()),
    }
}

pub fn register_subagent_scope_cache_service() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "subagent",
    );
}
