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
pub struct UserConfiguredAllowPermissionPolicyService {
    rules_service: Arc<dyn IAgentPermissionRulesService>,
}

impl UserConfiguredAllowPermissionPolicyService {
    pub fn new(rules_service: Arc<dyn IAgentPermissionRulesService>) -> Self {
        Self { rules_service }
    }
}

impl PermissionPolicy for UserConfiguredAllowPermissionPolicyService {
    fn name(&self) -> &str {
        "user-configured-allow"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = evaluate_user_configured_rule(
            &context,
            PermissionRuleDecision::Allow,
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
        decision: crate::permission::VerdictDecision::Allow,
        policy_name: "UserConfiguredAllow".into(),
        session_approval_rule: None,
        reason: Some(
            crate::i18n::LocalizedText::with_params(
                "engine.permission.allowedByUserRule",
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
    fn test_user_configured_allow() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            allow_rules: vec!["Write(tmp/*)".into(), "Bash(npm run lint)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "tmp/cache.json" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "UserConfiguredAllow");
        assert_eq!(
            verdict.reason,
            Some("Allowed by user rule: Write(tmp/*)".into())
        );
        assert!(verdict.is_allow());

        let verdict_bash = engine.evaluate("Bash", &json!({ "command": "npm run lint" }));
        assert_eq!(verdict_bash.decision, VerdictDecision::Allow);
        assert_eq!(verdict_bash.policy_name, "UserConfiguredAllow");
        assert_eq!(
            verdict_bash.reason,
            Some("Allowed by user rule: Bash(npm run lint)".into())
        );

        let verdict_other = engine.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict_other.decision, VerdictDecision::Ask);
        assert_eq!(verdict_other.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_other.reason,
            Some("Tool execution requires approval: Write".into())
        );
    }
}
