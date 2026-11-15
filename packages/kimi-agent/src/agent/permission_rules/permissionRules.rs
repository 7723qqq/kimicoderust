use std::sync::LazyLock;
use std::sync::Arc;
use crate::_base::di::instantiation::{ServiceIdentifier, create_decorator};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::agent::interaction::approval::ApprovalResponse;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionApprovalResultRecord {
    pub turn_id: u64,
    pub tool_call_id: String,
    pub tool_name: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_approval_rule: Option<String>,
    pub result: ApprovalResponse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRuleDecision {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionRuleScope {
    TurnOverride,
    SessionRuntime,
    Project,
    #[default]
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub decision: PermissionRuleDecision,
    #[serde(default)]
    pub scope: PermissionRuleScope,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[async_trait]
pub trait IAgentPermissionRulesService: Send + Sync {
    fn rules(&self) -> Vec<PermissionRule>;
    fn session_approval_rule_patterns(&self) -> Vec<String>;
    async fn add_rules(&self, rules: &[PermissionRule]) -> Result<(), WireAsyncError>;
    fn record_approval_result(&self, record: PermissionApprovalResultRecord);
}


pub static I_AGENT_PERMISSION_RULES_SERVICE: LazyLock<ServiceIdentifier<Arc<dyn IAgentPermissionRulesService>>> =
    LazyLock::new(|| create_decorator("agentPermissionRulesService"));
