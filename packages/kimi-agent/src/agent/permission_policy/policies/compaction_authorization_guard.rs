use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};
use serde_json::Value;

use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::permission_policy::types::{
    PermissionDecisionReason, PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::wire::record::WireAsyncError;

pub(crate) static IRREVERSIBLE_COMMAND: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)\b(git\s+push|git\s+merge|git\s+reset\s+--hard|gh\s+pr\s+merge|gh\s+api\b[\s\S]*?-X\s+(PUT|DELETE|PATCH)|branch\s+protection|ruleset|deploy|kubectl\s+delete|rm\s+-rf|DROP\s+(TABLE|DATABASE)|TRUNCATE\b|DELETE\s+FROM)\b",
        )
        .expect("the irreversible-command regex is a compile-time constant")
    });

#[derive(Clone)]
pub struct CompactionAuthorizationGuardPermissionPolicyService {
    context: Arc<dyn IAgentContextMemoryService>,
}

impl CompactionAuthorizationGuardPermissionPolicyService {
    pub fn new(context: Arc<dyn IAgentContextMemoryService>) -> Self {
        Self { context }
    }
}

impl PermissionPolicy for CompactionAuthorizationGuardPermissionPolicyService {
    fn name(&self) -> &str {
        "compaction-authorization-guard"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let compacted = self.context.get().iter().any(|message| {
            message
                .origin
                .as_ref()
                .is_some_and(|origin| origin.kind() == "compaction_summary")
        });
        let command = context
            .base
            .args
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string);
        let result = (compacted
            && context.base.tool_call.name == "Bash"
            && command
                .as_deref()
                .is_some_and(|command| IRREVERSIBLE_COMMAND.is_match(command)))
            .then_some({
                let mut reason = PermissionDecisionReason::new();
                reason.insert("compaction_authorization_required".to_string(), Value::Bool(true));
                PermissionPolicyResult::Ask {
                    reason: Some(reason),
                    resolve_approval: None,
                    resolve_error: None,
                    resolve_non_interactive: None,
                }
            });
        async move { Ok(result) }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    (context.has_compaction_summary
        && context.tool_lower == "bash"
        && context
            .args
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| IRREVERSIBLE_COMMAND.is_match(command)))
        .then_some(crate::permission::LocalPermissionVerdict {
            decision: crate::permission::VerdictDecision::Ask,
            policy_name: "CompactionAuthorizationGuard".into(),
            session_approval_rule: None,
            reason: Some(
                crate::i18n::LocalizedText::new(
                    "engine.permission.compactionAuthorizationRequired",
                )
                .render(),
            ),
        })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use crate::turn_loop::types::LLMMessage;
    use serde_json::{Value, json};

    #[test]
    fn compaction_authorization_guard_asks_only_for_irreversible_bash_after_compaction() {
        let engine = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });
        assert_eq!(
            engine
                .evaluate("Bash", &json!({ "command": "git push origin main" }))
                .policy_name,
            "YoloModeApprove",
            "a session without a compaction summary is untouched"
        );

        let mut summary = LLMMessage::new("user", "the conversation was compacted");
        summary.origin = Some(json!({ "kind": "compaction_summary" }));
        engine.set_conversation_provider(std::sync::Arc::new(move || vec![summary.clone()]));

        let verdict = engine.evaluate("Bash", &json!({ "command": "git push origin main" }));
        assert_eq!(verdict.decision, VerdictDecision::Ask);
        assert_eq!(verdict.policy_name, "CompactionAuthorizationGuard");
        assert!(verdict.reason.is_some());

        for command in [
            "git reset --hard HEAD~1",
            "rm -rf build",
            "gh pr merge 12",
            "kubectl delete pod x",
        ] {
            assert_eq!(
                engine
                    .evaluate("Bash", &json!({ "command": command }))
                    .policy_name,
                "CompactionAuthorizationGuard",
                "{command} is irreversible"
            );
        }

        assert_eq!(
            engine
                .evaluate("Bash", &json!({ "command": "cargo test" }))
                .policy_name,
            "YoloModeApprove",
            "an ordinary command is not affected"
        );
        assert_eq!(
            engine
                .evaluate("Read", &json!({ "path": "src/main.rs" }))
                .policy_name,
            "YoloModeApprove",
            "the guard is Bash-only"
        );
    }
}
