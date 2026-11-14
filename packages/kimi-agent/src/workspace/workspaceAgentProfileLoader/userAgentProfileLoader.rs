use futures_util::future::BoxFuture;

use crate::app::agent_profile_catalog::agent_profile_catalog::AgentProfile;
use crate::wire::record::WireAsyncError;

pub trait IUserAgentProfileLoader: Send + Sync {
    fn ready(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
    fn get_default_profile(&self) -> AgentProfile;
    fn reload(&self) -> BoxFuture<'static, Result<(), WireAsyncError>>;
}
