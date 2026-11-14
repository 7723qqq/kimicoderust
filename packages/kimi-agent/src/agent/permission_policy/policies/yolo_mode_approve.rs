use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::types::{
    PermissionMode, PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct YoloModeApprovePermissionPolicyService {
    mode_service: Arc<dyn IAgentPermissionModeService>,
}

impl YoloModeApprovePermissionPolicyService {
    pub fn new(mode_service: Arc<dyn IAgentPermissionModeService>) -> Self {
        Self { mode_service }
    }
}

impl PermissionPolicy for YoloModeApprovePermissionPolicyService {
    fn name(&self) -> &str {
        "yolo-mode-approve"
    }

    fn evaluate(
        &self,
        _context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = (self.mode_service.mode() == PermissionMode::Yolo).then_some(
            PermissionPolicyResult::Approve {
                reason: None,
                execution_metadata: None,
            },
        );
        async move { Ok(result) }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    (context.mode == PermissionMode::Yolo).then_some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Allow,
        policy_name: "YoloModeApprove".into(),
        session_approval_rule: None,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_yolo_mode_approves_mutating_tools() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        let verdict = engine.evaluate("Edit", &json!({ "path": "README.md" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        let verdict = engine.evaluate("Bash", &json!({ "command": "cargo check" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        let verdict = engine.evaluate(
            "GitHubCreateIssue",
            &json!({ "owner": "octocat", "repo": "hello-world", "title": "test" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "YoloModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());
    }
}
