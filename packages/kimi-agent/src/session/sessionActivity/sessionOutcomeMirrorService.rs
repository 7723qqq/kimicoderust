use std::sync::{Arc, Weak};

use futures_util::future::{BoxFuture, Shared};
use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::json;

use crate::_base::di::lifecycle::IDisposable;
use crate::_base::di::scope::{ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::agent::r#loop::turn_events::TurnStarted;
use crate::agent::r#loop::turn_ops::{
    TurnEnded, TurnModelLastEnded, TurnModelState, turn_replayable_key,
};
use crate::agent::state::agent_state::{I_AGENT_STATE_SERVICE, IAgentStateService};
use crate::agent::undo::undo_service::ContextUndone;
use crate::app::event::event_bus::IEventBus;
use crate::app::scopes::LifecycleScope;
use crate::session::agent_lifecycle::agent_lifecycle::{IAgentLifecycleService, MAIN_AGENT_ID};
use crate::session::session_metadata::session_metadata::{
    ISessionMetadata, SessionLastTurnReason, SessionMetaPatch,
};
use crate::state::event_dispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

use super::session_activity::SessionTurnOutcome;
use super::session_outcome_mirror::ISessionOutcomeMirror;

#[derive(Debug, Clone, Default)]
struct OutcomeState {
    last_persisted: Option<SessionTurnOutcome>,
    last_persisted_turn_id: Option<u64>,
    adopted: bool,
    turn_started_here: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct OutcomeWriteOptions {
    touch_updated_at: Option<bool>,
    turn_id: Option<u64>,
}

fn turn_last_ended(agent_states: &Arc<dyn IAgentStateService>) -> Option<TurnModelLastEnded> {
    serde_json::from_value::<TurnModelState>(
        agent_states.get(&turn_replayable_key().state_key()),
    )
    .unwrap_or_else(|error| panic!("{error}"))
    .last_ended
}

pub struct SessionOutcomeMirror {
    agents: Arc<dyn IAgentLifecycleService>,
    metadata: Arc<dyn ISessionMetadata>,
    log: Arc<dyn ILogService>,
    state: Arc<Mutex<OutcomeState>>,
    main_subscription: Arc<Mutex<Option<Vec<Box<dyn IDisposable>>>>>,
    metadata_ready: Shared<BoxFuture<'static, ()>>,
    weak: Mutex<Weak<Self>>,
    subscriptions: Mutex<Vec<Box<dyn IDisposable>>>,
}

impl SessionOutcomeMirror {
    pub fn new(
        agents: Arc<dyn IAgentLifecycleService>,
        metadata: Arc<dyn ISessionMetadata>,
        log: Arc<dyn ILogService>,
    ) -> Arc<Self> {
        let state = Arc::new(Mutex::new(OutcomeState::default()));
        let ready_state = state.clone();
        let ready_metadata = metadata.clone();
        let metadata_ready = async move {
            if let Ok(meta) = ready_metadata.read().await {
                let mut state = ready_state.lock();
                if !state.adopted {
                    state.last_persisted = meta.last_turn_reason.map(from_metadata_outcome);
                }
            }
        }
        .boxed()
        .shared();
        let this = Arc::new(Self {
            agents,
            metadata,
            log,
            state,
            main_subscription: Arc::new(Mutex::new(None)),
            metadata_ready,
            weak: Mutex::new(Weak::new()),
            subscriptions: Mutex::new(Vec::new()),
        });
        *this.weak.lock() = Arc::downgrade(&this);
        this.attach_main();
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(this.agents.on_did_create(
            Arc::new(move |agent| {
                if agent.agent_id == MAIN_AGENT_ID
                    && let Some(this) = weak.upgrade()
                {
                    this.attach_main();
                }
            }),
        ));
        let main_subscription = this.main_subscription.clone();
        this.subscriptions.lock().push(this.agents.on_did_close(
            Arc::new(move |agent| {
                if agent.agent_id == MAIN_AGENT_ID {
                    main_subscription.lock().take();
                }
            }),
        ));
        this
    }

    fn attach_main(self: &Arc<Self>) {
        if self.main_subscription.lock().is_some() {
            return;
        }
        let Some(handle) = self.agents.handle_of(MAIN_AGENT_ID) else {
            return;
        };
        let bus = handle.accessor().get::<IEventBus>();
        let dispatcher = handle.accessor().get::<IEventDispatcher>();
        let agent_states = handle.accessor().get(&I_AGENT_STATE_SERVICE);
        let mut subscriptions = Vec::new();
        let weak = Arc::downgrade(self);
        let restore_states = agent_states.clone();
        subscriptions.push(dispatcher.hooks.on_did_restore.register(
            "session-outcome-mirror",
            Arc::new(move |_ctx, next| {
                let weak = weak.clone();
                let states = restore_states.clone();
                Box::pin(async move {
                    next().await?;
                    if let Some(this) = weak.upgrade() {
                        this.reconcile_after_restore(&states).await;
                    }
                    Ok(())
                }) as BoxFuture<'static, Result<(), WireAsyncError>>
            }),
        ));
        let weak = Arc::downgrade(self);
        subscriptions.push(bus.subscribe_turn_ended(Arc::new(move |event: TurnEnded| {
            if let Some(this) = weak.upgrade() {
                match event.reason {
                    crate::agent::r#loop::turn_events::TurnEndReason::Completed => {
                        this.write(
                            Some(SessionTurnOutcome::Completed),
                            OutcomeWriteOptions {
                                turn_id: Some(event.turn_id),
                                ..Default::default()
                            },
                        );
                    }
                    crate::agent::r#loop::turn_events::TurnEndReason::Failed
                    | crate::agent::r#loop::turn_events::TurnEndReason::Blocked => {
                        this.write(
                            Some(SessionTurnOutcome::Failed),
                            OutcomeWriteOptions {
                                turn_id: Some(event.turn_id),
                                ..Default::default()
                            },
                        );
                    }
                    crate::agent::r#loop::turn_events::TurnEndReason::Cancelled
                        if event.interrupt_reason.as_deref() == Some("user_cancelled") =>
                    {
                        this.write(
                            Some(SessionTurnOutcome::Cancelled),
                            OutcomeWriteOptions {
                                turn_id: Some(event.turn_id),
                                ..Default::default()
                            },
                        );
                    }
                    _ => {}
                }
            }
        })));
        let weak = Arc::downgrade(self);
        subscriptions.push(bus.subscribe_turn_started(Arc::new(move |_event: TurnStarted| {
            if let Some(this) = weak.upgrade() {
                this.state.lock().turn_started_here = true;
                this.write(None, OutcomeWriteOptions::default());
            }
        })));
        let weak = Arc::downgrade(self);
        subscriptions.push(bus.subscribe_context_undone(Arc::new(move |event: ContextUndone| {
            if let Some(this) = weak.upgrade() {
                let state = this.state.lock();
                if event.from_turn_id.is_some()
                    && state.last_persisted_turn_id.is_some()
                    && state.last_persisted_turn_id < event.from_turn_id
                {
                    return;
                }
                drop(state);
                this.write(None, OutcomeWriteOptions::default());
            }
        })));
        *self.main_subscription.lock() = Some(subscriptions);
        self.seed_from_wire(&agent_states);
    }

    fn seed_from_wire(self: &Arc<Self>, agent_states: &Arc<dyn IAgentStateService>) {
        if !agent_states.has(&turn_replayable_key().state_key()) {
            return;
        }
        let Some(last_ended) = turn_last_ended(agent_states) else {
            return;
        };
        let weak = Arc::downgrade(self);
        let ready = self.metadata_ready.clone();
        tokio::spawn(async move {
            ready.await;
            if let Some(this) = weak.upgrade() {
                let state = this.state.lock();
                if state.turn_started_here || state.last_persisted.is_some() {
                    return;
                }
                drop(state);
                this.adopt_last_ended(last_ended.turn_id, &last_ended.reason);
            }
        });
    }

    fn adopt_last_ended(&self, turn_id: u64, reason: &str) {
        let outcome = match reason {
            "completed" => SessionTurnOutcome::Completed,
            "cancelled" => SessionTurnOutcome::Cancelled,
            _ => SessionTurnOutcome::Failed,
        };
        self.write(
            Some(outcome),
            OutcomeWriteOptions {
                touch_updated_at: Some(false),
                turn_id: Some(turn_id),
            },
        );
    }

    async fn reconcile_after_restore(&self, agent_states: &Arc<dyn IAgentStateService>) {
        self.metadata_ready.clone().await;
        if self.state.lock().turn_started_here
            || !agent_states.has(&turn_replayable_key().state_key())
        {
            return;
        }
        let last_ended = turn_last_ended(agent_states);
        if self.state.lock().last_persisted.is_none() {
            if let Some(last_ended) = last_ended {
                self.adopt_last_ended(last_ended.turn_id, &last_ended.reason);
            }
            return;
        }
        if last_ended.is_none() {
            self.write(
                None,
                OutcomeWriteOptions {
                    touch_updated_at: Some(false),
                    turn_id: None,
                },
            );
            return;
        }
        let mut state = self.state.lock();
        if state.last_persisted_turn_id.is_none() {
            state.last_persisted_turn_id = last_ended.map(|last| last.turn_id);
        }
    }

    fn write(&self, outcome: Option<SessionTurnOutcome>, opts: OutcomeWriteOptions) {
        {
            let mut state = self.state.lock();
            if state.last_persisted == outcome {
                if let Some(turn_id) = opts.turn_id {
                    state.last_persisted_turn_id = Some(turn_id);
                }
                return;
            }
        }
        let (previous, previous_turn_id) = {
            let mut state = self.state.lock();
            let previous = state.last_persisted;
            let previous_turn_id = state.last_persisted_turn_id;
            state.adopted = true;
            state.last_persisted = outcome;
            state.last_persisted_turn_id = if outcome.is_none() {
                None
            } else {
                opts.turn_id.or(state.last_persisted_turn_id)
            };
            (previous, previous_turn_id)
        };
        let metadata = self.metadata.clone();
        let state = self.state.clone();
        let log = self.log.clone();
        tokio::spawn(async move {
            let patch = SessionMetaPatch {
                last_turn_reason: outcome.map(to_metadata_outcome),
                ..Default::default()
            };
            if let Err(error) = metadata.update(patch, opts.touch_updated_at).await {
                log.warn(
                    &format!("session outcome persist failed: lastTurnReason={outcome:?}"),
                    Some(json!({ "error": error.to_string() })),
                );
                let mut state = state.lock();
                if state.last_persisted == outcome {
                    state.last_persisted = previous;
                    state.last_persisted_turn_id = previous_turn_id;
                }
            }
        });
    }
}

impl ISessionOutcomeMirror for SessionOutcomeMirror {}

fn from_metadata_outcome(outcome: SessionLastTurnReason) -> SessionTurnOutcome {
    match outcome {
        SessionLastTurnReason::Completed => SessionTurnOutcome::Completed,
        SessionLastTurnReason::Cancelled => SessionTurnOutcome::Cancelled,
        SessionLastTurnReason::Failed => SessionTurnOutcome::Failed,
    }
}

fn to_metadata_outcome(outcome: SessionTurnOutcome) -> SessionLastTurnReason {
    match outcome {
        SessionTurnOutcome::Completed => SessionLastTurnReason::Completed,
        SessionTurnOutcome::Cancelled => SessionLastTurnReason::Cancelled,
        SessionTurnOutcome::Failed => SessionLastTurnReason::Failed,
    }
}

pub fn register_session_outcome_mirror_service() {
    register_scoped_service(
        LifecycleScope::Session,
        ScopeActivation::OnScopeCreated,
        "sessionActivity",
    );
}
