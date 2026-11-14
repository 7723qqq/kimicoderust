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
pub struct UserConfiguredDenyPermissionPolicyService {
    rules_service: Arc<dyn IAgentPermissionRulesService>,
}

impl UserConfiguredDenyPermissionPolicyService {
    pub fn new(rules_service: Arc<dyn IAgentPermissionRulesService>) -> Self {
        Self { rules_service }
    }
}

impl PermissionPolicy for UserConfiguredDenyPermissionPolicyService {
    fn name(&self) -> &str {
        "user-configured-deny"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = evaluate_user_configured_rule(
            &context,
            PermissionRuleDecision::Deny,
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
    let reason = match context
        .snapshot
        .rule_reasons
        .get(rule.as_str())
        .map(String::as_str)
        .filter(|reason| !reason.trim().is_empty())
    {
        Some(why) => crate::i18n::LocalizedText::with_params(
            "engine.permission.deniedByUserRule",
            crate::i18n::i18n_params!["rule" => rule, "why" => why],
        )
        .render(),
        None => crate::i18n::LocalizedText::with_params(
            "engine.permission.deniedByUserRuleNoWhy",
            crate::i18n::i18n_params!["rule" => rule],
        )
        .render(),
    };
    Some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Deny,
        policy_name: "UserConfiguredDeny".into(),
        session_approval_rule: None,
        reason: Some(reason),
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_user_configured_deny_overrides_all_modes_and_policies() {
        let engine_yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            deny_rules: vec!["Bash(rm -rf *)".into()],
            ..Default::default()
        });
        let verdict = engine_yolo.evaluate("Bash", &json!({ "command": "rm -rf /" }));
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict.reason,
            Some("Denied by user rule: Bash(rm -rf *)".into())
        );
        assert!(!verdict.is_allow());

        let verdict_safe = engine_yolo.evaluate("Bash", &json!({ "command": "echo hello" }));
        assert_eq!(verdict_safe.decision, VerdictDecision::Allow);
        assert_eq!(verdict_safe.policy_name, "YoloModeApprove");
        assert_eq!(verdict_safe.reason, None);

        let engine_auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            deny_rules: vec!["Write".into()],
            ..Default::default()
        });
        let verdict_auto = engine_auto.evaluate("Write", &json!({ "path": "src/main.rs" }));
        assert_eq!(verdict_auto.decision, VerdictDecision::Deny);
        assert_eq!(verdict_auto.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_auto.reason,
            Some("Denied by user rule: Write".into())
        );
        assert!(!verdict_auto.is_allow());

        let engine_wildcard = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["*(*.secret)".into()],
            ..Default::default()
        });
        let verdict_read = engine_wildcard.evaluate("Read", &json!({ "path": "app.secret" }));
        assert_eq!(verdict_read.decision, VerdictDecision::Deny);
        assert_eq!(verdict_read.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_read.reason,
            Some("Denied by user rule: *(*.secret)".into())
        );

        let verdict_edit = engine_wildcard.evaluate("Edit", &json!({ "path": "app.secret" }));
        assert_eq!(verdict_edit.decision, VerdictDecision::Deny);
        assert_eq!(verdict_edit.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_edit.reason,
            Some("Denied by user rule: *(*.secret)".into())
        );

        let engine_session = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            deny_rules: vec!["Bash(dropdb *)".into()],
            session_approvals: vec!["Bash".into()],
            ..Default::default()
        });
        let verdict_denied = engine_session.evaluate("Bash", &json!({ "command": "dropdb prod" }));
        assert_eq!(verdict_denied.decision, VerdictDecision::Deny);
        assert_eq!(verdict_denied.policy_name, "UserConfiguredDeny");
        assert_eq!(
            verdict_denied.reason,
            Some("Denied by user rule: Bash(dropdb *)".into())
        );
    }
}
