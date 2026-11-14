use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_policy::types::{
    PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, Copy, Default)]
pub struct FallbackAskPermissionPolicyService;

impl PermissionPolicy for FallbackAskPermissionPolicyService {
    fn name(&self) -> &str {
        "fallback-ask"
    }

    fn evaluate(
        &self,
        _context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        async move {
            Ok(Some(PermissionPolicyResult::Ask {
                reason: None,
                resolve_approval: None,
                resolve_error: None,
                resolve_non_interactive: None,
            }))
        }
        .boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> crate::permission::LocalPermissionVerdict {
    crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Ask,
        policy_name: "FallbackAsk".into(),
        session_approval_rule: None,
        reason: Some(
            crate::i18n::LocalizedText::with_params(
                "engine.permission.toolExecutionRequiresApproval",
                crate::i18n::i18n_params!["tool_name" => context.tool_name],
            )
            .render(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_manual_mode_fallback_ask_for_write() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "package.json" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
        assert_eq!(
            verdict.reason,
            Some("Tool execution requires approval: Write".into())
        );
        assert!(!verdict.is_allow());

        let verdict = engine.evaluate("Edit", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
        assert_eq!(
            verdict.reason,
            Some("Tool execution requires approval: Edit".into())
        );

        let verdict = engine.evaluate("DeployTool", &json!({ "env": "staging" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "FallbackAsk");
        assert_eq!(
            verdict.reason,
            Some("Tool execution requires approval: DeployTool".into())
        );
    }
}
