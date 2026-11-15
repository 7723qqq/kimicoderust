use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::tool::tool_contract::ToolDisclosure;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserToolRegistration {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<ToolDisclosure>,
}

pub trait IAgentUserToolService: Send + Sync {
    fn list(&self) -> Vec<UserToolRegistration>;

    fn inherit_user_tools(
        &self,
        parent: &dyn IAgentUserToolService,
        active_tool_names: Option<Vec<String>>,
    );

    fn register(&self, input: UserToolRegistration);

    fn unregister(&self, name: &str);
}


pub static I_AGENT_USER_TOOL_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentUserToolService>>> =
    LazyLock::new(|| create_decorator("agentUserToolService"));
