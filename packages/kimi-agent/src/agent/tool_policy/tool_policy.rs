use futures_util::future::BoxFuture;

use super::evaluate::ToolActivationPolicy;
use crate::tool::tool_contract::ToolSource;
use crate::wire::record::WireAsyncError;

pub trait IAgentToolPolicyService: Send + Sync {
    fn is_tool_active(&self, name: &str, source: Option<ToolSource>) -> bool;
    fn is_tool_denied(&self, name: &str, source: Option<ToolSource>) -> bool;
    fn get_policy_fingerprint(&self) -> String;
    fn is_tool_active_for_disclosure(&self, name: &str, source: Option<ToolSource>) -> bool;
    fn is_tool_active_for_profile(
        &self,
        profile: &ToolActivationPolicy,
        name: &str,
        source: Option<ToolSource>,
    ) -> bool;
    fn set_session_disabled_tools(
        &self,
        names: Vec<String>,
    ) -> BoxFuture<'static, Result<(), WireAsyncError>>;
}
