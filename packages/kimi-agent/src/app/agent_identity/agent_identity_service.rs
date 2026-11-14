use std::sync::Arc;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use kimi_core::primitives::errors::{Error2, domains::core::INTERNAL};
use parking_lot::Mutex;

use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::config::config::IConfigService;

use super::agent_identity::{
    AgentIdentityFuture, AgentIdentitySnapshot, IAgentIdentity, build_agent_identity_snapshot,
};
use super::config_section::{IDENTITY_SECTION, IdentityConfig};

pub struct AgentIdentityService {
    snapshot: Arc<Mutex<Option<AgentIdentitySnapshot>>>,
    frozen: Shared<BoxFuture<'static, AgentIdentitySnapshot>>,
}

impl AgentIdentityService {
    pub fn new(
        config: Arc<dyn IConfigService>,
        bootstrap: Arc<dyn IBootstrapService>,
    ) -> Self {
        let ready = config.ready();
        let snapshot = Arc::new(Mutex::new(None));
        let frozen_snapshot = snapshot.clone();
        let task = tokio::spawn(async move {
            let _ = ready.await;
            let section = config
                .get(IDENTITY_SECTION)
                .and_then(|value| serde_json::from_value::<IdentityConfig>(value).ok())
                .unwrap_or_default();
            let args = bootstrap.args();
            let resolved = build_agent_identity_snapshot(
                super::agent_identity::AgentIdentityInput {
                    name: section.name,
                    slug: section.slug,
                    host_display_name: args.display_name.clone(),
                    host_request_headers: args.request_headers.clone().into_iter().collect(),
                },
            );
            *frozen_snapshot.lock() = Some(resolved.clone());
            resolved
        });
        let frozen = async move {
            match task.await {
                Ok(resolved) => resolved,
                Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Err(_) => std::future::pending::<AgentIdentitySnapshot>().await,
            }
        }
        .boxed()
        .shared();
        Self {
            snapshot,
            frozen,
        }
    }
}

impl IAgentIdentity for AgentIdentityService {
    fn resolved(&self) -> AgentIdentityFuture {
        let frozen = self.frozen.clone();
        async move { Ok(frozen.await) }.boxed()
    }

    fn current(&self) -> AgentIdentitySnapshot {
        self.snapshot.lock().clone().unwrap_or_else(|| {
            std::panic::panic_any(Error2::new(
                INTERNAL,
                "agent identity read before config load completed",
            ))
        })
    }
}
