use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::types::{
    PermissionMode, PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct AutoModeApprovePermissionPolicyService {
    mode_service: Arc<dyn IAgentPermissionModeService>,
}

impl AutoModeApprovePermissionPolicyService {
    pub fn new(mode_service: Arc<dyn IAgentPermissionModeService>) -> Self {
        Self { mode_service }
    }
}

impl PermissionPolicy for AutoModeApprovePermissionPolicyService {
    fn name(&self) -> &str {
        "auto-mode-approve"
    }

    fn evaluate(
        &self,
        _context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = (self.mode_service.mode() == PermissionMode::Auto).then_some(
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
    (context.mode == PermissionMode::Auto).then_some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Allow,
        policy_name: "AutoModeApprove".into(),
        session_approval_rule: None,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_auto_mode_approves_safe_and_mutating() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        assert_eq!(verdict.reason, None);
        assert!(verdict.is_allow());

        let verdict = engine.evaluate("Edit", &json!({ "path": "Cargo.toml" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        assert_eq!(verdict.reason, None);

        let verdict = engine.evaluate(
            "GitHubCreateIssue",
            &json!({ "owner": "octocat", "repo": "hello-world", "title": "t" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "AutoModeApprove");
        assert_eq!(verdict.reason, None);
    }
}
