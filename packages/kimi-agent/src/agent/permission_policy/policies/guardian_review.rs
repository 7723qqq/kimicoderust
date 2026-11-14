use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use serde_json::Value;

use crate::agent::guardian::guardian_service::{GuardianVerdict, IAgentGuardianService};
use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::types::{
    PermissionDecisionReason, PermissionMode, PermissionPolicy, PermissionPolicyResult,
};
use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;
use crate::turn_loop::types::{FileOperation, ToolResourceAccess};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct GuardianReviewPermissionPolicyService {
    mode_service: Arc<dyn IAgentPermissionModeService>,
    guardian: Arc<dyn IAgentGuardianService>,
}

impl GuardianReviewPermissionPolicyService {
    pub fn new(
        mode_service: Arc<dyn IAgentPermissionModeService>,
        guardian: Arc<dyn IAgentGuardianService>,
    ) -> Self {
        Self {
            mode_service,
            guardian,
        }
    }

    async fn evaluate_inner(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> Result<Option<PermissionPolicyResult>, WireAsyncError> {
        if self.mode_service.mode() != PermissionMode::Yolo
            || !self.guardian.enabled()
            || !is_high_risk(&context)
        {
            return Ok(None);
        }
        let result = match self.guardian.review(context).await {
            GuardianVerdict::Allow {
                risk_level,
                user_authorization,
                rationale,
            } if risk_level == "low" || user_authorization == "explicit" => {
                PermissionPolicyResult::Approve {
                    reason: Some(guardian_reason(rationale)),
                    execution_metadata: None,
                }
            }
            GuardianVerdict::Allow {
                risk_level,
                user_authorization,
                rationale,
            } => PermissionPolicyResult::Ask {
                reason: Some(guardian_reason(format!(
                    "guardian allowed but risk={risk_level}, authorization={user_authorization}: {rationale}"
                ))),
                resolve_approval: None,
                resolve_error: None,
                resolve_non_interactive: None,
            },
            GuardianVerdict::Deny {
                risk_level,
                rationale,
            } => PermissionPolicyResult::Ask {
                reason: Some(guardian_reason(format!(
                    "denied ({risk_level}): {rationale}"
                ))),
                resolve_approval: None,
                resolve_error: None,
                resolve_non_interactive: None,
            },
            GuardianVerdict::Bypass { .. } => return Ok(None),
        };
        Ok(Some(result))
    }
}

impl PermissionPolicy for GuardianReviewPermissionPolicyService {
    fn name(&self) -> &str {
        "guardian-review"
    }

    fn evaluate(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let this = self.clone();
        async move { this.evaluate_inner(context).await }.boxed()
    }
}

fn is_high_risk(context: &ResolvedToolExecutionHookContext) -> bool {
    if context.base.tool_call.name == "Bash" {
        return true;
    }
    context
        .execution
        .accesses
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|access| match access {
            ToolResourceAccess::File(file) => {
                matches!(file.operation, FileOperation::Write | FileOperation::ReadWrite)
            }
            _ => false,
        })
}

fn guardian_reason(rationale: String) -> PermissionDecisionReason {
    let mut reason = PermissionDecisionReason::new();
    reason.insert("guardian".to_string(), Value::String(rationale));
    reason
}
