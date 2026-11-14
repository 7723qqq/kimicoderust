use std::sync::Arc;

use crate::_base::di::scope::ISessionScopeHandle;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::wire::record::WireAsyncError;

use super::agent_lifecycle::{CreateAgentOptions, IAgentLifecycleService, MAIN_AGENT_ID};

pub async fn ensure_main_agent(
    session: &Arc<ISessionScopeHandle>,
) -> Result<Arc<AgentContext>, WireAsyncError> {
    ensure_main_agent_with_options(session, None).await
}

pub async fn ensure_main_agent_with_options(
    session: &Arc<ISessionScopeHandle>,
    opts: Option<CreateAgentOptions>,
) -> Result<Arc<AgentContext>, WireAsyncError> {
    let mut opts = opts.unwrap_or_default();
    opts.agent_id = Some(MAIN_AGENT_ID.to_string());
    session
        .accessor()
        .get::<IAgentLifecycleService>()
        .create(Some(opts))
        .await
}
