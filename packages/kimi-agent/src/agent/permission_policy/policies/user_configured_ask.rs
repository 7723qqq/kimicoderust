use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_policy::types::{
    PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::agent::permission_rules::permission_rules::{
    IAgentPermissionRulesService, PermissionRuleDecision,
};
use crate::wire::record::WireAsyncError;

use super::user_configured_rule::evaluate_user_configured_rule;

#[derive(Clone)]
pub struct UserConfiguredAskPermissionPolicyService {
    rules_service: Arc<dyn IAgentPermissionRulesService>,
}

impl UserConfiguredAskPermissionPolicyService {
    pub fn new(rules_service: Arc<dyn IAgentPermissionRulesService>) -> Self {
        Self { rules_service }
    }
}

impl PermissionPolicy for UserConfiguredAskPermissionPolicyService {
    fn name(&self) -> &str {
        "user-configured-ask"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = evaluate_user_configured_rule(
            &context,
            PermissionRuleDecision::Ask,
            self.rules_service.as_ref(),
        );
        async move { Ok(result) }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
    rules: &[crate::agent::permission_rules::matches_rule::CompiledRule],
) -> Option<crate::permission::LocalPermissionVerdict> {
    let rule = super::user_configured_rule::first_matching_legacy_rule(context, rules)?;
    Some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Ask,
        policy_name: "UserConfiguredAsk".into(),
        session_approval_rule: Some(rule.clone()),
        reason: Some(
            crate::i18n::LocalizedText::with_params(
                "engine.permission.approvalRequiredByUserRule",
                crate::i18n::i18n_params!["rule" => rule],
            )
            .render(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_user_configured_ask() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ask_rules: vec!["Write(config/*)".into(), "Bash(deploy *)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "config/prod.json" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "UserConfiguredAsk");
        assert_eq!(
            verdict.reason,
            Some("Approval required by user rule: Write(config/*)".into())
        );
        assert!(!verdict.is_allow());
        assert_eq!(
            verdict.session_approval_rule.as_deref(),
            Some("Write(config/*)")
        );

        let verdict_bash = engine.evaluate("Bash", &json!({ "command": "deploy prod" }));
        assert_eq!(verdict_bash.decision, VerdictDecision::Ask);
        assert_eq!(verdict_bash.policy_name, "UserConfiguredAsk");
        assert_eq!(
            verdict_bash.reason,
            Some("Approval required by user rule: Bash(deploy *)".into())
        );

        let verdict_pass = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict_pass.decision, VerdictDecision::Allow);
        assert_eq!(verdict_pass.policy_name, "YoloModeApprove");
        assert_eq!(verdict_pass.reason, None);
    }
}
