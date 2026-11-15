use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable, to_disposable};
use crate::_base::utils::abort::{AbortSignal, abortable};
use crate::mcp_core::connection_manager::{
    McpConnectionManager, McpConnectionView, McpResolved, McpServerEntry, McpStatusListener,
};
use crate::mcp_core::oauth::service::McpOAuthService;
use crate::mcp_core::types::MCPClient;
use crate::wire::record::WireAsyncError;

pub struct MergedMcpConnectionView {
    base: Arc<McpConnectionManager>,
    overlay: Arc<McpConnectionManager>,
    overlay_names: HashSet<String>,
}

impl MergedMcpConnectionView {
    pub fn new(
        base: Arc<McpConnectionManager>,
        overlay: Arc<McpConnectionManager>,
        overlay_names: HashSet<String>,
    ) -> Self {
        Self {
            base,
            overlay,
            overlay_names,
        }
    }

    fn owner(&self, name: &str) -> &Arc<McpConnectionManager> {
        if self.overlay_names.contains(name) {
            &self.overlay
        } else {
            &self.base
        }
    }
}

#[async_trait]
impl McpConnectionView for MergedMcpConnectionView {
    fn oauth_service(&self) -> Option<Arc<McpOAuthService>> {
        self.overlay
            .oauth_service()
            .or_else(|| self.base.oauth_service())
    }

    fn list(&self) -> Vec<McpServerEntry> {
        let mut entries = self
            .base
            .list()
            .into_iter()
            .filter(|entry| !self.overlay_names.contains(&entry.name))
            .collect::<Vec<_>>();
        entries.extend(self.overlay.list());
        entries
    }

    fn get(&self, name: &str) -> Option<McpServerEntry> {
        self.owner(name).get(name)
    }

    fn deferred_of(&self, name: &str) -> bool {
        self.owner(name).deferred_of(name)
    }

    fn resolved(&self, name: &str) -> Option<McpResolved> {
        self.owner(name).resolved(name)
    }

    fn get_remote_server_url(&self, name: &str) -> Option<String> {
        self.owner(name).get_remote_server_url(name)
    }

    async fn mark_needs_auth(
        &self,
        name: &str,
        error: WireAsyncError,
        client: Option<Arc<dyn MCPClient>>,
    ) -> Result<bool, WireAsyncError> {
        self.owner(name).mark_needs_auth(name, error, client).await
    }

    async fn reconnect(&self, name: &str) -> Result<(), WireAsyncError> {
        self.owner(name).reconnect(name).await
    }

    async fn reconnect_and_join(&self, name: &str) -> Result<(), WireAsyncError> {
        self.owner(name).reconnect_and_join(name).await
    }

    async fn wait_for_initial_load(&self, signal: Option<AbortSignal>) -> Result<(), WireAsyncError> {
        if let Some(signal) = &signal {
            signal.throw_if_aborted()?;
        }
        let both = async {
            let (base, overlay) = tokio::join!(
                self.base.wait_for_initial_load(None),
                self.overlay.wait_for_initial_load(None)
            );
            base?;
            overlay?;
            Ok(())
        };
        match signal {
            Some(signal) => abortable(both, &signal).await,
            None => both.await,
        }
    }

    fn initial_load_duration_ms(&self) -> u64 {
        self.base
            .initial_load_duration_ms()
            .max(self.overlay.initial_load_duration_ms())
    }

    fn on_status_change(&self, listener: McpStatusListener) -> DisposableRef {
        let overlay_names = self.overlay_names.clone();
        let base_listener = listener.clone();
        let unsubscribe_base = self.base.on_status_change(Arc::new(move |entry| {
            if !overlay_names.contains(&entry.name) {
                base_listener(entry);
            }
        }));
        let unsubscribe_overlay = self.overlay.on_status_change(listener);
        to_disposable(move || {
            unsubscribe_base.dispose();
            unsubscribe_overlay.dispose();
        })
    }
}
