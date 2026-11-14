use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use serde_json::Value;

use crate::agent::permission_policy::types::{
    PermissionDecisionReason, PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::agent::permission_rules::matches_rule::{
    PermissionRuleMatchInput, PermissionRuleMatchStrategy, match_permission_rule,
};
use crate::agent::permission_rules::permission_rules::{
    IAgentPermissionRulesService, PermissionRule, PermissionRuleDecision, PermissionRuleScope,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct SessionApprovalHistoryPermissionPolicyService {
    rules_service: Arc<dyn IAgentPermissionRulesService>,
}

impl SessionApprovalHistoryPermissionPolicyService {
    pub fn new(rules_service: Arc<dyn IAgentPermissionRulesService>) -> Self {
        Self { rules_service }
    }
}

impl PermissionPolicy for SessionApprovalHistoryPermissionPolicyService {
    fn name(&self) -> &str {
        "session-approval-history"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = self.rules_service.session_approval_rule_patterns().into_iter().find_map(
            |pattern| {
                let match_ = match_permission_rule(PermissionRuleMatchInput {
                    rule: PermissionRule {
                        decision: PermissionRuleDecision::Allow,
                        scope: PermissionRuleScope::SessionRuntime,
                        pattern,
                        reason: Some("approve for session".to_string()),
                    },
                    tool_name: &context.base.tool_call.name,
                    execution: &context.execution,
                })?;
                let mut reason = PermissionDecisionReason::new();
                reason.insert("has_rule_args".to_string(), Value::Bool(match_.has_rule_args));
                reason.insert(
                    "match_strategy".to_string(),
                    Value::String(
                        match match_.strategy {
                            PermissionRuleMatchStrategy::ToolNameOnly => "tool_name_only",
                            PermissionRuleMatchStrategy::MatchesRule => "matches_rule",
                        }
                        .to_string(),
                    ),
                );
                Some(PermissionPolicyResult::Approve {
                    reason: Some(reason),
                    execution_metadata: None,
                })
            },
        );
        async move { Ok(result) }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
    records: &[crate::agent::permission_rules::matches_rule::CompiledRule],
    restored: &[crate::agent::permission_rules::matches_rule::CompiledRule],
) -> Option<crate::permission::LocalPermissionVerdict> {
    let rule = [records, restored].into_iter().find_map(|rules| {
        crate::agent::permission_rules::matches_rule::matches_any_rule(
            rules,
            context.tool_lower,
            context.subject,
            context.rule_subjects,
        )
    })?;
    Some(crate::permission::LocalPermissionVerdict {
        decision: crate::permission::VerdictDecision::Allow,
        policy_name: "SessionApprovalHistory".into(),
        session_approval_rule: None,
        reason: Some(
            crate::i18n::LocalizedText::with_params(
                "engine.permission.approvedBySessionHistory",
                crate::i18n::i18n_params!["rule" => rule],
            )
            .render(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::{Value, json};

    #[test]
    fn an_engine_recorded_approval_stops_the_next_ask() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        assert_eq!(
            engine
                .evaluate("Write", &json!({ "path": "notes.txt" }))
                .decision,
            VerdictDecision::Ask
        );

        assert!(engine.remember_session_approval("Write(notes.txt)"));

        let verdict = engine.evaluate("Write", &json!({ "path": "notes.txt" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");
        assert_eq!(
            engine.session_approval_rules(),
            vec!["Write(notes.txt)".to_string()]
        );
    }

    #[test]
    fn a_recorded_rule_covers_its_own_pattern_and_nothing_else() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        assert!(engine.remember_session_approval("Write(src/*.rs)"));

        assert!(
            engine
                .evaluate("Write", &json!({ "path": "src/lib.rs" }))
                .is_allow(),
            "the approved pattern is allowed"
        );
        assert_eq!(
            engine
                .evaluate("Write", &json!({ "path": "notes.txt" }))
                .decision,
            VerdictDecision::Ask,
            "approving one path must not approve the tool"
        );
        assert_eq!(
            engine
                .evaluate("Read", &json!({ "path": "src/lib.rs" }))
                .decision,
            VerdictDecision::Allow,
            "Read is allowed by the default policy, not by the Write rule"
        );
    }

    #[test]
    fn a_rule_is_recorded_once_and_only_when_it_can_mean_something() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Bash(cargo test)".into()],
            ..Default::default()
        });

        assert!(
            !engine.remember_session_approval("   "),
            "an empty rule grants nothing"
        );
        assert!(
            !engine.remember_session_approval("Bash(cargo test)"),
            "a rule the snapshot already carries is not re-recorded"
        );
        assert!(engine.remember_session_approval("Bash(cargo build)"));
        assert!(
            !engine.remember_session_approval("Bash(cargo build)"),
            "recording twice is a no-op"
        );
        assert_eq!(
            engine.session_approval_rules(),
            vec!["Bash(cargo build)".to_string()]
        );
    }

    #[test]
    fn test_session_approval_history() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            session_approvals: vec!["Write(src/*.rs)".into(), "Bash(cargo test)".into()],
            ..Default::default()
        });

        let verdict = engine.evaluate("Write", &json!({ "path": "src/lib.rs" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");
        assert_eq!(
            verdict.reason,
            Some("Approved by session history rule: Write(src/*.rs)".into())
        );
        assert!(verdict.is_allow());

        let verdict = engine.evaluate("Bash", &json!({ "command": "cargo test" }));
        assert_eq!(verdict.decision, VerdictDecision::Allow);
        assert_eq!(verdict.policy_name, "SessionApprovalHistory");
        assert_eq!(
            verdict.reason,
            Some("Approved by session history rule: Bash(cargo test)".into())
        );

        let verdict_miss = engine.evaluate("Write", &json!({ "path": "tests/test.rs" }));
        assert_eq!(verdict_miss.decision, VerdictDecision::Ask);
        assert_eq!(verdict_miss.policy_name, "FallbackAsk");
        assert_eq!(
            verdict_miss.reason,
            Some("Tool execution requires approval: Write".into())
        );
    }
}
