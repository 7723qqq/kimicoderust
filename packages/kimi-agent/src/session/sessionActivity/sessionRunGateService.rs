use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::wire::wire::I_WIRE_SERVICE;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::app::scopes::LifecycleScope;
use crate::persistence::interface::append_log_store::IAppendLogStore;
use crate::persistence::interface::session_lock::ISessionLockService;
use crate::session::agent_lifecycle::agent_lifecycle::IAgentLifecycleService;
use crate::session::errors::SESSION_STALE;
use crate::session::session_context::session_context::ISessionContext;
use crate::wire::record::WireAsyncError;
use crate::wire::wire::IWireService;

use super::session_activity::ISessionActivityView;
use super::session_run_gate::ISessionRunGate;

pub const SESSION_LOCK_RELEASE_DELAY_MS: u64 = 3000;

pub struct SessionRunGateService {
    ctx: Arc<dyn ISessionContext>,
    session_locks: Arc<dyn ISessionLockService>,
    append_log_store: Arc<dyn IAppendLogStore>,
    agents: Arc<dyn IAgentLifecycleService>,
    activity: Arc<dyn ISessionActivityView>,
    timer: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    releasing: Arc<AtomicBool>,
    weak: Mutex<Weak<Self>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

impl SessionRunGateService {
    pub fn new(
        ctx: Arc<dyn ISessionContext>,
        session_locks: Arc<dyn ISessionLockService>,
        append_log_store: Arc<dyn IAppendLogStore>,
        agents: Arc<dyn IAgentLifecycleService>,
        activity: Arc<dyn ISessionActivityView>,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            ctx,
            session_locks,
            append_log_store,
            agents,
            activity,
            timer: Arc::new(Mutex::new(None)),
            releasing: Arc::new(AtomicBool::new(false)),
            weak: Mutex::new(Weak::new()),
            subscriptions: Mutex::new(Vec::new()),
        });
        *this.weak.lock() = Arc::downgrade(&this);
        let weak = Arc::downgrade(&this);
        this.subscriptions.lock().push(this.activity.on_did_change(
            Arc::new(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.schedule();
                }
            }),
        ));
        let weak = Arc::downgrade(&this);
        let session_id = this.ctx.session_id();
        this.subscriptions.lock().push(this.session_locks.on_did_hold(
            Arc::new(move |event| {
                if event.session_id == session_id
                    && let Some(this) = weak.upgrade()
                {
                    this.schedule();
                }
            }),
        ));
        this
    }

    pub fn begin(&self) -> Result<(), WireAsyncError> {
        let session_id = self.ctx.session_id();
        self.session_locks.hold(&session_id)?;
        let stale = self.agents.list(None).into_iter().any(|agent| {
            self.agents
                .handle_of(&agent.agent_id)
                .is_some_and(|handle| {
                    handle.accessor().get(&I_WIRE_SERVICE).externally_modified()
                })
        });
        if !stale {
            return Ok(());
        }
        self.session_locks.release(&session_id);
        Err(Box::new(Error2::with_options(
            SESSION_STALE,
            format!("Session \"{session_id}\" was updated by another client; reload it before continuing"),
            Error2Options {
                details: Some(Map::from_iter([(
                    "sessionId".to_string(),
                    Value::String(session_id),
                )])),
                ..Default::default()
            },
        )))
    }

    fn schedule(self: &Arc<Self>) {
        self.cancel_timer();
        if self.activity.state().busy || !self.session_locks.held(&self.ctx.session_id()) {
            return;
        }
        let weak = Arc::downgrade(self);
        *self.timer.lock() = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(SESSION_LOCK_RELEASE_DELAY_MS)).await;
            if let Some(this) = weak.upgrade() {
                this.release_when_idle().await;
            }
        }));
    }

    async fn release_when_idle(self: &Arc<Self>) {
        if self.releasing.swap(true, Ordering::AcqRel) {
            return;
        }
        if !self.activity.state().busy {
            let _ = self.append_log_store.flush().await;
            if !self.activity.state().busy {
                self.session_locks.release(&self.ctx.session_id());
            }
        }
        self.releasing.store(false, Ordering::Release);
        if !self.activity.state().busy && self.session_locks.held(&self.ctx.session_id()) {
            self.schedule();
        }
    }

    fn cancel_timer(&self) {
        if let Some(timer) = self.timer.lock().take() {
            timer.abort();
        }
    }
}

impl ISessionRunGate for SessionRunGateService {
    fn begin(&self) -> Result<(), WireAsyncError> {
        SessionRunGateService::begin(self)
    }
}

impl Drop for SessionRunGateService {
    fn drop(&mut self) {
        self.cancel_timer();
        self.session_locks.release(&self.ctx.session_id());
    }
}
