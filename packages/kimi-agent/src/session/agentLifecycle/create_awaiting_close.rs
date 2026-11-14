use std::sync::Arc;
use std::time::Duration;

use kimi_core::primitives::errors::Error2;

use crate::_base::utils::abort::AbortSignal;
use crate::agent::agent_context::agent_context::AgentContext;
use crate::session::subagent::subagent_scope_cache::resolve_subagent_scope_evict_timeout_ms;
use crate::wire::record::WireAsyncError;

use super::agent_lifecycle::{CreateAgentOptions, IAgentLifecycleService};
use super::errors::AGENT_ALREADY_EXISTS;

pub const CLOSE_WAIT_POLL_MS: u64 = 50;

pub async fn create_agent_awaiting_close(
    lifecycle: &Arc<dyn IAgentLifecycleService>,
    opts: CreateAgentOptions,
    signal: Option<&AbortSignal>,
) -> Result<Arc<AgentContext>, WireAsyncError> {
    let deadline = tokio::time::Instant::now()
        + Duration::from_millis(resolve_subagent_scope_evict_timeout_ms(None)?);
    loop {
        if let Some(signal) = signal {
            signal.throw_if_aborted()?;
        }
        match lifecycle.create(Some(opts.clone())).await {
            Ok(context) => return Ok(context),
            Err(error) => {
                let closing = error
                    .downcast_ref::<Error2>()
                    .is_some_and(|error| error.code().as_str() == AGENT_ALREADY_EXISTS);
                if !closing || tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(Duration::from_millis(CLOSE_WAIT_POLL_MS)).await;
            }
        }
    }
}
