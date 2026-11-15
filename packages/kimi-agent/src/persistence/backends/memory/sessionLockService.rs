use std::sync::Arc;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::persistence::interface::session_lock::{
    ISessionLockService, SessionLockHeldEvent, SessionLockHolds, SessionLockHoldsBackend,
    SessionLockLease,
};
use crate::wire::record::WireAsyncError;

struct InMemorySessionLockBackend;

impl SessionLockHoldsBackend for InMemorySessionLockBackend {
    fn publish(&self, _session_id: &str) -> Result<String, WireAsyncError> {
        Ok(String::new())
    }

    fn unpublish(&self, _session_id: &str, _token: &str) {}

    fn evict(&self, _session_id: &str) -> Result<(), WireAsyncError> {
        Ok(())
    }
}

pub struct InMemorySessionLockService {
    holds: SessionLockHolds,
}

impl InMemorySessionLockService {
    pub fn new() -> Self {
        Self {
            holds: SessionLockHolds::new(Arc::new(InMemorySessionLockBackend)),
        }
    }

    pub fn release_all(&self) {
        self.holds.release_all();
    }
}

impl Default for InMemorySessionLockService {
    fn default() -> Self {
        Self::new()
    }
}

impl ISessionLockService for InMemorySessionLockService {
    fn on_did_hold(
        &self,
        listener: Arc<dyn Fn(SessionLockHeldEvent) + Send + Sync>,
    ) -> DisposableRef {
        self.holds.on_did_hold(listener)
    }

    fn hold(&self, session_id: &str) -> Result<(), WireAsyncError> {
        self.holds.hold(session_id)
    }

    fn try_hold(&self, session_id: &str) -> Result<bool, WireAsyncError> {
        self.holds.try_hold(session_id)
    }

    fn release(&self, session_id: &str) {
        self.holds.release(session_id);
    }

    fn lease(&self, session_id: &str) -> Result<SessionLockLease, WireAsyncError> {
        self.holds.lease(session_id)
    }

    fn held(&self, session_id: &str) -> bool {
        self.holds.held(session_id)
    }

    fn takeover(&self, session_id: &str) -> Result<(), WireAsyncError> {
        self.holds.takeover(session_id)
    }
}
