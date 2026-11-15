use std::ops::Deref;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::_base::log::log::ILogService;
use crate::app::agent_identity::agent_identity::IAgentIdentity;
use crate::mcp_core::oauth::service::{McpOAuthService, McpOAuthServiceOptions};
use crate::mcp_core::oauth::store::McpOAuthStore;

pub type IMcpOAuthService = McpOAuthService;

pub struct AppMcpOAuthService {
    service: Arc<McpOAuthService>,
    background_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl AppMcpOAuthService {
    pub fn new(
        store: Arc<dyn McpOAuthStore>,
        identity: Arc<dyn IAgentIdentity>,
        log: Arc<dyn ILogService>,
    ) -> Arc<Self> {
        let resolve_identity = identity.clone();
        let service = Arc::new(McpOAuthService::new(McpOAuthServiceOptions {
            store,
            client_label: None,
            resolve_client_name: Some(Arc::new(move || resolve_identity.current().slug)),
            coordinator: None,
            log: Some(log.clone()),
            scheduler: None,
            auth_request_timeout_ms: None,
            shutdown_drain_timeout_ms: None,
        }));
        let this = Arc::new(Self {
            service,
            background_tasks: Mutex::new(Vec::new()),
        });
        let service = this.service.clone();
        let task = tokio::spawn(async move {
            let result = async {
                identity.resolved().await?;
                service.sweep_proactive_refresh().await
            }
            .await;
            if let Err(error) = result {
                log.warn(
                    &format!("mcp oauth proactive-refresh sweep failed: {error}"),
                    None,
                );
            }
        });
        this.background_tasks.lock().push(task);
        this
    }

    pub fn service(&self) -> Arc<McpOAuthService> {
        self.service.clone()
    }
}

impl Deref for AppMcpOAuthService {
    type Target = McpOAuthService;

    fn deref(&self) -> &Self::Target {
        &self.service
    }
}

impl Drop for AppMcpOAuthService {
    fn drop(&mut self) {
        for task in self.background_tasks.lock().drain(..) {
            task.abort();
        }
    }
}
