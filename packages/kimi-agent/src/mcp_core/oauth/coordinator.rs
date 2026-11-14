use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::store::canonical_mcp_oauth_resource;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpOAuthCredentialsChangeKind {
    Updated,
    Invalidated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthCredentialsChangedEvent {
    pub server_name: String,
    pub server_url: String,
    pub kind: McpOAuthCredentialsChangeKind,
}

pub trait McpOAuthCredentialsCoordinator: Send + Sync {
    fn notify_credentials_changed(&self, server_name: &str, server_url: &str) -> Result<(), WireAsyncError>;
    fn notify_credentials_invalidated(&self, server_name: &str, server_url: &str) -> Result<(), WireAsyncError>;
    fn on_credentials_changed(
        &self,
        listener: Arc<dyn Fn(McpOAuthCredentialsChangedEvent) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send>;
}

pub struct McpOAuthCoordinator {
    listeners: Arc<Mutex<IndexMap<u64, Arc<dyn Fn(McpOAuthCredentialsChangedEvent) + Send + Sync>>>>,
    next_id: Mutex<u64>,
}

impl McpOAuthCoordinator {
    pub fn new() -> Self {
        Self {
            listeners: Arc::new(Mutex::new(IndexMap::new())),
            next_id: Mutex::new(0),
        }
    }

    fn emit(&self, event: McpOAuthCredentialsChangedEvent) {
        let mut visited = HashSet::new();
        loop {
            let next = {
                let listeners = self.listeners.lock();
                listeners
                    .iter()
                    .find(|(id, _)| !visited.contains(*id))
                    .map(|(id, listener)| (*id, listener.clone()))
            };
            let Some((id, listener)) = next else {
                break;
            };
            visited.insert(id);
            listener(event.clone());
        }
    }
}

impl McpOAuthCredentialsCoordinator for McpOAuthCoordinator {
    fn notify_credentials_changed(&self, server_name: &str, server_url: &str) -> Result<(), WireAsyncError> {
        let event = McpOAuthCredentialsChangedEvent {
            server_name: server_name.to_string(),
            server_url: canonical_mcp_oauth_resource(server_url)?,
            kind: McpOAuthCredentialsChangeKind::Updated,
        };
        self.emit(event);
        Ok(())
    }

    fn notify_credentials_invalidated(&self, server_name: &str, server_url: &str) -> Result<(), WireAsyncError> {
        let event = McpOAuthCredentialsChangedEvent {
            server_name: server_name.to_string(),
            server_url: canonical_mcp_oauth_resource(server_url)?,
            kind: McpOAuthCredentialsChangeKind::Invalidated,
        };
        self.emit(event);
        Ok(())
    }

    fn on_credentials_changed(
        &self,
        listener: Arc<dyn Fn(McpOAuthCredentialsChangedEvent) + Send + Sync>,
    ) -> Box<dyn FnOnce() + Send> {
        let id = {
            let existing = self
                .listeners
                .lock()
                .iter()
                .find(|(_, current)| Arc::ptr_eq(current, &listener))
                .map(|(id, _)| *id);
            if let Some(id) = existing {
                id
            } else {
                let mut next = self.next_id.lock();
                let id = *next;
                *next += 1;
                self.listeners.lock().insert(id, listener);
                id
            }
        };
        let listeners = self.listeners.clone();
        Box::new(move || {
            listeners.lock().shift_remove(&id);
        })
    }
}
