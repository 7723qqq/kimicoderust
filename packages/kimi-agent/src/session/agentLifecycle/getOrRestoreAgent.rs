use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::session::agent_lifecycle::agent_lifecycle::I_AGENT_LIFECYCLE_SERVICE;
use crate::session::session_metadata::session_metadata::I_SESSION_METADATA;
use std::sync::Arc;

use crate::_base::di::scope::{IAgentScopeHandle, ISessionScopeHandle};
use crate::session::session_metadata::session_metadata::ISessionMetadata;
use crate::wire::record::WireAsyncError;

use super::agent_lifecycle::{CreateAgentOptions, IAgentLifecycleService};

pub async fn get_or_restore_agent(
    session: &Arc<ISessionScopeHandle>,
    agent_id: &str,
) -> Result<Option<Arc<IAgentScopeHandle>>, WireAsyncError> {
    let lifecycle = session.accessor().get(&I_AGENT_LIFECYCLE_SERVICE);
    if let Some(live) = lifecycle.handle_of(agent_id) {
        return Ok(Some(live));
    }
    let meta = session
        .accessor()
        .get(&I_SESSION_METADATA)
        .read()
        .await?
        .agents
        .unwrap_or_default()
        .remove(agent_id);
    let Some(meta) = meta else {
        return Ok(None);
    };
    lifecycle
        .clone()
        .create(Some(CreateAgentOptions {
            agent_id: Some(agent_id.to_string()),
            forked_from: meta.forked_from,
            labels: meta.labels,
            ..Default::default()
        }))
        .await?;
    Ok(lifecycle.handle_of(agent_id))
}
