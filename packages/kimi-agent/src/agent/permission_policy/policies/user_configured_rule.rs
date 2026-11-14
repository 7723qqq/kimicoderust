use crate::agent::permission_policy::types::PermissionPolicyResult;
use crate::agent::permission_rules::matches_rule::{
    PermissionRuleMatch, PermissionRuleMatchInput, match_permission_rule,
};
use crate::agent::permission_rules::permission_rules::{
    IAgentPermissionRulesService, PermissionRuleDecision, PermissionRuleScope,
};
use crate::agent::tool_executor::tool_hooks::ResolvedToolExecutionHookContext;

pub fn evaluate_user_configured_rule(
    context: &ResolvedToolExecutionHookContext,
    decision: PermissionRuleDecision,
    rules_service: &dyn IAgentPermissionRulesService,
) -> Option<PermissionPolicyResult> {
    let match_ = first_matching_rule(context, decision, rules_service)?;
    match decision {
        PermissionRuleDecision::Deny => Some(PermissionPolicyResult::Deny {
            reason: None,
            message: Some(default_permission_rule_deny_message(
                &context.base.tool_call.name,
                match_.rule.reason.as_deref(),
            )),
        }),
        PermissionRuleDecision::Ask => Some(PermissionPolicyResult::Ask {
            reason: None,
            resolve_approval: None,
            resolve_error: None,
            resolve_non_interactive: None,
        }),
        PermissionRuleDecision::Allow => Some(PermissionPolicyResult::Approve {
            reason: None,
            execution_metadata: None,
        }),
    }
}

fn default_permission_rule_deny_message(tool: &str, reason: Option<&str>) -> String {
    let suffix = reason
        .filter(|reason| !reason.is_empty())
        .map(|reason| format!(" Reason: {reason}"))
        .unwrap_or_default();
    format!("Tool \"{tool}\" was denied by permission rule.{suffix}")
}

pub(crate) fn first_matching_legacy_rule(
    context: &crate::permission::LegacyPolicyContext<'_>,
    rules: &[crate::agent::permission_rules::matches_rule::CompiledRule],
) -> Option<String> {
    crate::agent::permission_rules::matches_rule::matches_any_rule(
        rules,
        context.tool_lower,
        context.subject,
        context.rule_subjects,
    )
}

fn first_matching_rule(
    context: &ResolvedToolExecutionHookContext,
    decision: PermissionRuleDecision,
    rules_service: &dyn IAgentPermissionRulesService,
) -> Option<PermissionRuleMatch> {
    for rule in rules_service.rules().into_iter().filter(|rule| {
        matches!(
            rule.scope,
            PermissionRuleScope::TurnOverride | PermissionRuleScope::Project | PermissionRuleScope::User
        )
    }) {
        if rule.decision != decision {
            continue;
        }
        if let Some(match_) = match_permission_rule(PermissionRuleMatchInput {
            rule,
            tool_name: &context.base.tool_call.name,
            execution: &context.execution,
        }) {
            return Some(match_);
        }
    }
    None
}
