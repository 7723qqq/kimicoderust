use crate::agent::environment_binding::agent_environment::IAgentEnvironmentService;
use crate::environment::environment::{EnvironmentCapability, EnvironmentLease};
use crate::environment::environment_registry::EnvironmentError;
use crate::wire::record::WireAsyncError;

pub fn acquire_environment_lease(
    environment: &dyn IAgentEnvironmentService,
    required: &[EnvironmentCapability],
) -> Result<Option<EnvironmentLease>, WireAsyncError> {
    match environment.acquire(required) {
        Ok(lease) => Ok(Some(lease)),
        Err(error) if error.downcast_ref::<EnvironmentError>().is_some() => Ok(None),
        Err(error) => Err(error),
    }
}
