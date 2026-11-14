use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::_base::errors::errors::SharedError;
use crate::agent::permission_rules::permission_rules::PermissionRule;
use crate::agent::tool_executor::tool_executor::ExecutableToolResult;
use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;
use crate::wire::record::WireAsyncError;

pub use crate::agent::interaction::approval::{ApprovalRequest, ApprovalResponse};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[default]
    Manual,
    Auto,
    Yolo,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionData {
    pub mode: PermissionMode,
    pub rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Approve,
    Deny,
    Ask,
}

pub type PermissionDecisionReason = Map<String, Value>;
pub type PermissionPolicyContext = ResolvedToolExecutionHookContext;
pub type ResolveApproval =
    Arc<dyn Fn(ApprovalResponse) -> Option<PermissionPolicyResolution> + Send + Sync>;
pub type ResolveError =
    Arc<dyn Fn(SharedError) -> Option<PermissionPolicyResolution> + Send + Sync>;
pub type ResolveNonInteractive = Arc<dyn Fn() -> PermissionPolicyResolution + Send + Sync>;

#[derive(Clone)]
pub enum PermissionPolicyResolution {
    Approve {
        reason: Option<PermissionDecisionReason>,
        execution_metadata: Option<Value>,
    },
    Deny {
        reason: Option<PermissionDecisionReason>,
        message: Option<String>,
    },
    Ask {
        reason: Option<PermissionDecisionReason>,
        resolve_approval: Option<ResolveApproval>,
        resolve_error: Option<ResolveError>,
        resolve_non_interactive: Option<ResolveNonInteractive>,
    },
    Result {
        result: ExecutableToolResult,
    },
}

#[derive(Clone)]
pub enum PermissionPolicyResult {
    Approve {
        reason: Option<PermissionDecisionReason>,
        execution_metadata: Option<Value>,
    },
    Deny {
        reason: Option<PermissionDecisionReason>,
        message: Option<String>,
    },
    Ask {
        reason: Option<PermissionDecisionReason>,
        resolve_approval: Option<ResolveApproval>,
        resolve_error: Option<ResolveError>,
        resolve_non_interactive: Option<ResolveNonInteractive>,
    },
}

impl From<PermissionPolicyResult> for PermissionPolicyResolution {
    fn from(result: PermissionPolicyResult) -> Self {
        match result {
            PermissionPolicyResult::Approve {
                reason,
                execution_metadata,
            } => Self::Approve {
                reason,
                execution_metadata,
            },
            PermissionPolicyResult::Deny { reason, message } => Self::Deny { reason, message },
            PermissionPolicyResult::Ask {
                reason,
                resolve_approval,
                resolve_error,
                resolve_non_interactive,
            } => Self::Ask {
                reason,
                resolve_approval,
                resolve_error,
                resolve_non_interactive,
            },
        }
    }
}

pub trait PermissionPolicy: Send + Sync {
    fn name(&self) -> &str;

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>>;
}
