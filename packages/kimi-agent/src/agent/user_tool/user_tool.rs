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
