use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use futures_util::future::BoxFuture;
use parking_lot::Mutex;

use crate::_base::di::lifecycle::{IDisposable, to_disposable};
use crate::_base::di::scope::{IAgentScopeHandle, ScopeActivation, register_scoped_service};
use crate::agent::full_compaction::compaction_ops::{
    CompactionCancelled, CompactionCompleted, CompactionStarted,
};
use crate::agent::full_compaction::full_compaction::IAgentFullCompactionService;
use crate::agent::r#loop::r#loop::IAgentLoopService;
use crate::agent::r#loop::turn_events::{TurnEndReason, TurnStarted};
use crate::agent::r#loop::turn_ops::{TurnEnded, TurnModelState, turn_replayable_key};
use crate::agent::state::agent_state::{I_AGENT_STATE_SERVICE, IAgentStateService};
use crate::agent::task::task::IAgentTaskService;
use crate::agent::task::task_ops::{TaskStarted, TaskTerminatedNotice};
use crate::app::event::event_bus::IEventBus;
use crate::app::scopes::LifecycleScope;
use crate::human::interaction::facade::interactions;
use crate::human::interaction::interaction::{
    INTERACTION_TAG_SESSION_ID, Interaction, InteractionQuery,
};
use crate::session::agent_lifecycle::agent_lifecycle::{IAgentLifecycleService, MAIN_AGENT_ID};
use crate::session::session_context::session_context::ISessionContext;
use crate::session::state::session_state::ISessionStateService;
use crate::state::event_dispatcher::IEventDispatcher;
use crate::state::state::{StateDefinition, define_state};

use super::session_activity::{
    ISessionActivityView, SessionActivityCause, SessionActivityChangedEvent, SessionActivityState,
    SessionPendingInteraction, SessionTurnOutcome,
};

#[derive(Debug, Clone)]
pub struct AgentWorkFold {
    pub turn_active: bool,
    pub background: HashSet<String>,
    pub compacting: bool,
    pub last_turn_reason: Option<SessionTurnOutcome>,
}

pub static SESSION_ACTIVITY_FOLDS_KEY: LazyLock<StateDefinition<HashMap<String, AgentWorkFold>>> =
    LazyLock::new(|| define_state("sessionActivity.folds", Arc::new(HashMap::new)));
pub static SESSION_ACTIVITY_CURRENT_KEY: LazyLock<StateDefinition<SessionActivityState>> =
    LazyLock::new(|| {
        define_state(
            "sessionActivity.current",
            Arc::new(|| SessionActivityState {
                busy: false,
                main_turn_active: false,
                pending_interaction: SessionPendingInteraction::None,
                last_turn_reason: None,
            }),
        )
    });

use std::sync::LazyLock;

pub struct SessionActivityView {
    states: Arc<dyn ISessionStateService>,
    agents: Arc<dyn IAgentLifecycleService>,
    ctx: Arc<dyn ISessionContext>,
    agent_subscriptions: Arc<Mutex<HashMap<String, Vec<Box<dyn IDisposable>>>>>,
    active_at: AtomicI64,
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(SessionActivityChangedEvent) + Send + Sync>)>>>,
    next_listener_id: AtomicU64,
    weak: Mutex<Weak<Self>>,
    subscriptions: Mutex<Vec<Box<dyn IDisposable>>>,
}

impl SessionActivityView {
    pub fn new(
        states: Arc<dyn ISessionStateService>,
        agents: Arc<dyn IAgentLifecycleService>,
        ctx: Arc<dyn ISessionContext>,
    ) -> Arc<Self> {
        states.contribute_state(&SESSION_ACTIVITY_FOLDS_KEY);
        states.contribute_state(&SESSION_ACTIVITY_CURRENT_KEY);
        let this = Arc::new(Self {
            states,
            agents,
            ctx,
            agent_subscriptions: Arc::new(Mutex::new(HashMap::new())),
            active_at: AtomicI64::new(now_ms()),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: AtomicU64::new(0),
            weak: Mutex::new(Weak::new()),
            subscriptions: Mutex::new(Vec::new()),
        });
        *this.weak.lock() = Arc::downgrade(&this);
        for agent in this.agents.list(None) {
            if let Some(handle) = this.agents.handle_of(&agent.agent_id) {
                this.attach_agent(handle);
            }
        }
        let current = this.aggregate();
        this.states.set(&SESSION_ACTIVITY_CURRENT_KEY, current);
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(this.agents.on_did_create_scope(
            Arc::new(move |event| {
                if let Some(this) = weak.upgrade() {
                    this.attach_agent(event.handle);
                    this.recompute(SessionActivityCause::AgentLifecycle);
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(this.agents.on_did_close(
            Arc::new(move |agent| {
                if let Some(this) = weak.upgrade() {
                    this.agent_subscriptions.lock().remove(&agent.agent_id);
                    let removed = this
                        .states
                        .get(&SESSION_ACTIVITY_FOLDS_KEY)
                        .contains_key(&agent.agent_id);
                    if removed {
                        this.update_folds(|folds| {
                            folds.remove(&agent.agent_id);
                        });
                        this.recompute(SessionActivityCause::AgentLifecycle);
                    }
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(interactions().on_did_change_pending(
            Arc::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.recompute(SessionActivityCause::Interaction);
                }
            }),
        ));
        this
    }

    pub fn state(&self) -> SessionActivityState {
        self.states.get(&SESSION_ACTIVITY_CURRENT_KEY)
    }

    pub fn last_active_at(&self) -> i64 {
        self.active_at.load(Ordering::Relaxed)
    }

    fn attach_agent(self: &Arc<Self>, handle: Arc<IAgentScopeHandle>) {
        if self
            .states
            .get(&SESSION_ACTIVITY_FOLDS_KEY)
            .contains_key(handle.id())
        {
            return;
        }
        let agent_id = handle.id().to_string();
        let seed = seed_fold(&handle);
        self.update_folds(|folds| {
            folds.insert(agent_id.clone(), seed);
        });
        let bus = handle.accessor().get::<IEventBus>();
        let mut subscriptions = Vec::new();
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_turn_started(Arc::new(move |_event: TurnStarted| {
            if let Some(this) = weak.upgrade() {
                this.patch_fold(&id, |fold| {
                    fold.turn_active = true;
                    if id == MAIN_AGENT_ID {
                        fold.last_turn_reason = None;
                    }
                });
            }
        })));
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_turn_ended(Arc::new(move |event: TurnEnded| {
            if let Some(this) = weak.upgrade() {
                this.patch_fold(&id, |fold| {
                    fold.turn_active = false;
                    if id == MAIN_AGENT_ID {
                        fold.last_turn_reason = map_turn_reason(Some(event.reason));
                    }
                });
            }
        })));
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_task_started(Arc::new(move |event: TaskStarted| {
            if let Some(this) = weak.upgrade() {
                this.patch_fold(&id, |fold| {
                    fold.background.insert(event.info.task_id.clone());
                });
            }
        })));
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_task_terminated_notice(
            Arc::new(move |event: TaskTerminatedNotice| {
                if let Some(this) = weak.upgrade() {
                    this.patch_fold(&id, |fold| {
                        fold.background.remove(&event.info.task_id);
                    });
                }
            }),
        ));
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_compaction_started(
            Arc::new(move |_event: CompactionStarted| {
                if let Some(this) = weak.upgrade() {
                    this.patch_fold(&id, |fold| fold.compacting = true);
                }
            }),
        ));
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_compaction_completed(
            Arc::new(move |_event: CompactionCompleted| {
                if let Some(this) = weak.upgrade() {
                    this.patch_fold(&id, |fold| fold.compacting = false);
                }
            }),
        ));
        let weak = Arc::downgrade(self);
        let id = agent_id.clone();
        subscriptions.push(bus.subscribe_compaction_cancelled(
            Arc::new(move |_event: CompactionCancelled| {
                if let Some(this) = weak.upgrade() {
                    this.patch_fold(&id, |fold| fold.compacting = false);
                }
            }),
        ));
        let dispatcher = handle.accessor().get::<IEventDispatcher>();
        let weak = Arc::downgrade(self);
        let restore_handle = handle.clone();
        subscriptions.push(dispatcher.hooks.on_did_restore.register(
            "sessionActivity",
            Arc::new(move |_ctx, next| {
                let weak = weak.clone();
                let handle = restore_handle.clone();
                Box::pin(async move {
                    if let Some(this) = weak.upgrade() {
                        let seed = seed_fold(&handle);
                        this.update_folds(|folds| {
                            folds.insert(handle.id().to_string(), seed);
                        });
                        this.recompute(SessionActivityCause::AgentLifecycle);
                    }
                    next().await
                }) as BoxFuture<'static, Result<(), crate::wire::record::WireAsyncError>>
            }),
        ));
        self.agent_subscriptions
            .lock()
            .insert(agent_id, subscriptions);
    }

    fn patch_fold(&self, agent_id: &str, patch: impl FnOnce(&mut AgentWorkFold)) {
        let mut folds = self.states.get(&SESSION_ACTIVITY_FOLDS_KEY);
        let Some(previous) = folds.get(agent_id).cloned() else {
            return;
        };
        let mut next = previous.clone();
        patch(&mut next);
        folds.insert(agent_id.to_string(), next.clone());
        self.states.set(&SESSION_ACTIVITY_FOLDS_KEY, folds);
        let cause = if !previous.turn_active && next.turn_active {
            Some(SessionActivityCause::TurnStarted)
        } else if previous.turn_active && !next.turn_active {
            Some(SessionActivityCause::TurnEnded)
        } else if previous.background.len() != next.background.len()
            || previous.compacting != next.compacting
        {
            Some(SessionActivityCause::Background)
        } else if agent_id == MAIN_AGENT_ID && previous.last_turn_reason != next.last_turn_reason {
            Some(SessionActivityCause::TurnEnded)
        } else {
            None
        };
        if let Some(cause) = cause {
            self.recompute(cause);
        }
    }

    fn recompute(&self, cause: SessionActivityCause) {
        let next = self.aggregate();
        if self.state() == next {
            return;
        }
        self.states.set(&SESSION_ACTIVITY_CURRENT_KEY, next.clone());
        self.active_at.store(now_ms(), Ordering::Relaxed);
        let listeners = self.listeners.lock().clone();
        for (_, listener) in listeners {
            listener(SessionActivityChangedEvent {
                state: next.clone(),
                cause,
            });
        }
    }

    fn aggregate(&self) -> SessionActivityState {
        let folds = self.states.get(&SESSION_ACTIVITY_FOLDS_KEY);
        let busy = folds.values().any(|fold| {
            fold.turn_active || !fold.background.is_empty() || fold.compacting
        });
        let main = folds.get(MAIN_AGENT_ID);
        let pending = interactions().find_all(InteractionQuery {
            resolved: Some(false),
            tags: Some(HashMap::from([(
                INTERACTION_TAG_SESSION_ID.to_string(),
                self.ctx.session_id(),
            )])),
        });
        SessionActivityState {
            busy,
            main_turn_active: main.is_some_and(|fold| fold.turn_active),
            pending_interaction: resolve_pending_interaction(&pending),
            last_turn_reason: main.and_then(|fold| fold.last_turn_reason),
        }
    }

    fn update_folds(&self, update: impl FnOnce(&mut HashMap<String, AgentWorkFold>)) {
        let mut folds = self.states.get(&SESSION_ACTIVITY_FOLDS_KEY);
        update(&mut folds);
        self.states.set(&SESSION_ACTIVITY_FOLDS_KEY, folds);
    }
}

impl ISessionActivityView for SessionActivityView {
    fn state(&self) -> SessionActivityState {
        SessionActivityView::state(self)
    }

    fn last_active_at(&self) -> i64 {
        SessionActivityView::last_active_at(self)
    }

    fn on_did_change(
        &self,
        listener: Arc<dyn Fn(SessionActivityChangedEvent) + Send + Sync>,
    ) -> Box<dyn IDisposable> {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }
}

fn seed_fold(handle: &Arc<IAgentScopeHandle>) -> AgentWorkFold {
    let loop_ = handle.accessor().get::<IAgentLoopService>();
    let tasks = handle.accessor().get::<IAgentTaskService>();
    let compaction = handle.accessor().get::<IAgentFullCompactionService>();
    let states = handle.accessor().get(&I_AGENT_STATE_SERVICE);
    let running = loop_.snapshot().state == "running";
    let last_ended = (handle.id() == MAIN_AGENT_ID
        && states.has(&turn_replayable_key().state_key()))
    .then(|| {
        serde_json::from_value::<TurnModelState>(
            states.get(&turn_replayable_key().state_key()),
        )
        .unwrap_or_else(|error| panic!("{error}"))
        .last_ended
    })
    .flatten();
    AgentWorkFold {
        turn_active: running,
        background: tasks.list(true).into_iter().map(|task| task.task_id).collect(),
        compacting: compaction.compacting().is_some(),
        last_turn_reason: if running {
            None
        } else {
            map_turn_reason(last_ended.map(|last| last.reason))
        },
    }
}

fn map_turn_reason(reason: Option<TurnEndReason>) -> Option<SessionTurnOutcome> {
    match reason {
        None => None,
        Some(TurnEndReason::Completed) => Some(SessionTurnOutcome::Completed),
        Some(TurnEndReason::Cancelled) => Some(SessionTurnOutcome::Cancelled),
        Some(_) => Some(SessionTurnOutcome::Failed),
    }
}

fn resolve_pending_interaction(pending: &[Interaction]) -> SessionPendingInteraction {
    if pending.iter().any(|interaction| interaction.kind == "approval") {
        return SessionPendingInteraction::Approval;
    }
    if pending.iter().any(|interaction| interaction.kind == "question") {
        return SessionPendingInteraction::Question;
    }
    SessionPendingInteraction::None
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn register_session_activity_service() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "sessionActivity",
    );
}
