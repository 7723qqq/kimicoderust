use std::sync::LazyLock;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use indexmap::IndexMap;
use std::sync::Arc;

use kimi_core::primitives::errors::{Error2, Error2Options};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::session::errors::SESSION_OCCUPIED;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLockHolder {
    pub version: u64,
    pub token: String,
    pub pid: u32,
    pub hostname: String,
    pub started_at: i64,
    pub client_identity: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionLockHeldEvent {
    pub session_id: String,
}

pub struct SessionLockLease {
    pub session_id: String,
    release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl SessionLockLease {
    pub fn release(&self) {
        if let Some(release) = self.release.lock().take() {
            release();
        }
    }
}

pub trait ISessionLockService: Send + Sync {
    fn on_did_hold(
        &self,
        listener: Arc<dyn Fn(SessionLockHeldEvent) + Send + Sync>,
    ) -> DisposableRef;
    fn hold(&self, session_id: &str) -> Result<(), WireAsyncError>;
    fn try_hold(&self, session_id: &str) -> Result<bool, WireAsyncError>;
    fn release(&self, session_id: &str);
    fn lease(&self, session_id: &str) -> Result<SessionLockLease, WireAsyncError>;
    fn held(&self, session_id: &str) -> bool;
    fn takeover(&self, session_id: &str) -> Result<(), WireAsyncError>;
}

pub fn session_occupied_error(
    session_id: &str,
    holder: Option<&SessionLockHolder>,
) -> Error2 {
    let message = holder
        .and_then(|holder| holder.client_display_name.as_deref())
        .filter(|display_name| !display_name.is_empty())
        .map(|display_name| {
            format!("This session is in use by another {display_name} instance. Stop the session in that instance or exit it, then try again.")
        })
        .unwrap_or_else(|| {
            "This session is in use by another Kimi Code process. Stop the session in that process or exit it, then try again.".to_string()
        });
    let mut details = Map::new();
    details.insert("sessionId".to_string(), Value::String(session_id.to_string()));
    if let Some(holder) = holder {
        details.insert("holder".to_string(), serde_json::to_value(holder).unwrap());
    }
    Error2::with_options(
        SESSION_OCCUPIED,
        message,
        Error2Options {
            details: Some(details),
            ..Default::default()
        },
    )
}

pub trait SessionLockHoldsBackend: Send + Sync {
    fn publish(&self, session_id: &str) -> Result<String, WireAsyncError>;
    fn unpublish(&self, session_id: &str, token: &str);
    fn evict(&self, session_id: &str) -> Result<(), WireAsyncError>;
}

#[derive(Debug)]
struct HeldState {
    token: String,
    held: bool,
    leases: usize,
}

pub struct SessionLockHolds {
    backend: Arc<dyn SessionLockHoldsBackend>,
    held_sessions: Arc<Mutex<IndexMap<String, Arc<Mutex<HeldState>>>>>,
    listeners: Arc<Mutex<Vec<(u64, Arc<dyn Fn(SessionLockHeldEvent) + Send + Sync>)>>>,
    next_listener_id: std::sync::atomic::AtomicU64,
}

impl SessionLockHolds {
    pub fn new(backend: Arc<dyn SessionLockHoldsBackend>) -> Self {
        Self {
            backend,
            held_sessions: Arc::new(Mutex::new(IndexMap::new())),
            listeners: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn holding(&self) -> bool {
        !self.held_sessions.lock().is_empty()
    }

    pub fn release_all(&self) {
        let held = std::mem::take(&mut *self.held_sessions.lock());
        for (session_id, state) in held {
            let token = state.lock().token.clone();
            self.backend.unpublish(&session_id, &token);
        }
    }

    fn state_for(&self, session_id: &str) -> Result<Arc<Mutex<HeldState>>, WireAsyncError> {
        if let Some(state) = self.held_sessions.lock().get(session_id).cloned() {
            return Ok(state);
        }
        let token = self.backend.publish(session_id)?;
        let state = Arc::new(Mutex::new(HeldState {
            token,
            held: false,
            leases: 0,
        }));
        self.held_sessions
            .lock()
            .insert(session_id.to_string(), state.clone());
        let listeners = self.listeners.lock().clone();
        for (_, listener) in listeners {
            listener(SessionLockHeldEvent {
                session_id: session_id.to_string(),
            });
        }
        Ok(state)
    }

    fn unpublish_if_unused(&self, session_id: &str, state: &Arc<Mutex<HeldState>>) {
        {
            let state = state.lock();
            if state.held || state.leases > 0 {
                return;
            }
        }
        {
            let mut held_sessions = self.held_sessions.lock();
            if held_sessions
                .get(session_id)
                .is_none_or(|current| !Arc::ptr_eq(current, state))
            {
                return;
            }
            held_sessions.remove(session_id);
        }
        let token = state.lock().token.clone();
        self.backend.unpublish(session_id, &token);
    }
}

impl ISessionLockService for SessionLockHolds {
    fn on_did_hold(
        &self,
        listener: Arc<dyn Fn(SessionLockHeldEvent) + Send + Sync>,
    ) -> DisposableRef {
        let id = self.next_listener_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.listeners.lock().push((id, listener));
        let listeners = Arc::downgrade(&self.listeners);
        to_disposable(move || {
            if let Some(listeners) = listeners.upgrade() {
                listeners.lock().retain(|(candidate, _)| *candidate != id);
            }
        })
    }

    fn hold(&self, session_id: &str) -> Result<(), WireAsyncError> {
        self.state_for(session_id)?.lock().held = true;
        Ok(())
    }

    fn try_hold(&self, session_id: &str) -> Result<bool, WireAsyncError> {
        match self.hold(session_id) {
            Ok(()) => Ok(true),
            Err(error) => {
                if error
                    .downcast_ref::<Error2>()
                    .is_some_and(|error| error.code().as_str() == SESSION_OCCUPIED)
                {
                    Ok(false)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn release(&self, session_id: &str) {
        let state = self.held_sessions.lock().get(session_id).cloned();
        let Some(state) = state else {
            return;
        };
        {
            let mut state_guard = state.lock();
            if !state_guard.held {
                return;
            }
            state_guard.held = false;
        }
        self.unpublish_if_unused(session_id, &state);
    }

    fn lease(&self, session_id: &str) -> Result<SessionLockLease, WireAsyncError> {
        let state = self.state_for(session_id)?;
        state.lock().leases += 1;
        let held_sessions = self.held_sessions.clone();
        let backend = self.backend.clone();
        let session_id = session_id.to_string();
        let release_session_id = session_id.clone();
        let lease_state = state.clone();
        let release = Box::new(move || {
            {
                let mut state = lease_state.lock();
                state.leases = state.leases.saturating_sub(1);
            }
            let unused = {
                let state = lease_state.lock();
                !state.held && state.leases == 0
            };
            if !unused {
                return;
            }
            {
                let mut sessions = held_sessions.lock();
                if sessions
                    .get(&release_session_id)
                    .is_none_or(|current| !Arc::ptr_eq(current, &lease_state))
                {
                    return;
                }
                sessions.remove(&release_session_id);
            }
            let token = lease_state.lock().token.clone();
            backend.unpublish(&release_session_id, &token);
        });
        Ok(SessionLockLease {
            session_id: session_id.to_string(),
            release: Mutex::new(Some(release)),
        })
    }

    fn held(&self, session_id: &str) -> bool {
        self.held_sessions.lock().contains_key(session_id)
    }

    fn takeover(&self, session_id: &str) -> Result<(), WireAsyncError> {
        if self.held_sessions.lock().contains_key(session_id) {
            return Ok(());
        }
        self.backend.evict(session_id)
    }
}

impl Drop for SessionLockHolds {
    fn drop(&mut self) {
        self.release_all();
    }
}


pub static I_SESSION_LOCK_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn ISessionLockService>>> =
    LazyLock::new(|| create_decorator("sessionLockService"));
