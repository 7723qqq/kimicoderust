use std::sync::Arc;

use futures_util::future::{BoxFuture, FutureExt};

use crate::agent::permission_mode::permission_mode::IAgentPermissionModeService;
use crate::agent::permission_policy::types::{
    PermissionMode, PermissionPolicy, PermissionPolicyContext, PermissionPolicyResult,
};
use crate::wire::record::WireAsyncError;

#[derive(Clone)]
pub struct AutoModeAskUserQuestionDenyPermissionPolicyService {
    mode_service: Arc<dyn IAgentPermissionModeService>,
}

impl AutoModeAskUserQuestionDenyPermissionPolicyService {
    pub fn new(mode_service: Arc<dyn IAgentPermissionModeService>) -> Self {
        Self { mode_service }
    }
}

impl PermissionPolicy for AutoModeAskUserQuestionDenyPermissionPolicyService {
    fn name(&self) -> &str {
        "auto-mode-ask-user-question-deny"
    }

    fn evaluate(
        &self,
        context: PermissionPolicyContext,
    ) -> BoxFuture<'static, Result<Option<PermissionPolicyResult>, WireAsyncError>> {
        let result = (self.mode_service.mode() == PermissionMode::Auto
            && context.base.tool_call.name == "AskUserQuestion")
            .then_some(PermissionPolicyResult::Deny {
                reason: None,
                message: Some(
                    "AskUserQuestion is disabled while auto permission mode is active; decide and continue."
                        .to_string(),
                ),
            });
        async move { Ok(result) }.boxed()
    }
}

pub(crate) fn evaluate_legacy(
    context: &crate::permission::LegacyPolicyContext<'_>,
) -> Option<crate::permission::LocalPermissionVerdict> {
    (context.mode == PermissionMode::Auto
        && matches!(context.tool_lower, "askuserquestion" | "ask_user_question"))
        .then_some(crate::permission::LocalPermissionVerdict {
            decision: crate::permission::VerdictDecision::Deny,
            policy_name: "AutoModeAskUserQuestionDeny".into(),
            session_approval_rule: None,
            reason: Some(
                crate::i18n::LocalizedText::new("engine.permission.autoModeCannotAsk").render(),
            ),
        })
}

#[cfg(test)]
mod tests {
    use crate::permission::{PermissionEngine, PermissionMode, PolicySnapshot, VerdictDecision};
    use serde_json::json;

    #[test]
    fn test_auto_mode_ask_user_question_denied() {
        let engine_auto = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Auto,
            ..Default::default()
        });

        let verdict = engine_auto.evaluate(
            "AskUserQuestion",
            &json!({ "question": "Should I proceed with delete?" }),
        );
        assert_eq!(verdict.decision, VerdictDecision::Deny);
        assert_eq!(verdict.policy_name, "AutoModeAskUserQuestionDeny");
        assert_eq!(
            verdict.reason,
            Some("Auto mode cannot ask interactive questions".into())
        );
        assert!(!verdict.is_allow());

        let verdict_snake = engine_auto.evaluate(
            "ask_user_question",
            &json!({ "question": "Which option do you prefer?" }),
        );
        assert_eq!(verdict_snake.decision, VerdictDecision::Deny);
        assert_eq!(verdict_snake.policy_name, "AutoModeAskUserQuestionDeny");
        assert_eq!(
            verdict_snake.reason,
            Some("Auto mode cannot ask interactive questions".into())
        );

        let engine_manual = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Manual,
            ..Default::default()
        });
        let verdict_manual = engine_manual.evaluate("AskUserQuestion", &json!({}));
        assert_eq!(verdict_manual.decision, VerdictDecision::Allow);
        assert_eq!(verdict_manual.policy_name, "DefaultToolApprove");
        assert_eq!(verdict_manual.reason, None);

        let engine_yolo = PermissionEngine::new(PolicySnapshot {
            mode: PermissionMode::Yolo,
            ..Default::default()
        });
        let verdict_yolo = engine_yolo.evaluate("AskUserQuestion", &json!({}));
        assert_eq!(verdict_yolo.decision, VerdictDecision::Allow);
        assert_eq!(verdict_yolo.policy_name, "YoloModeApprove");
        assert_eq!(verdict_yolo.reason, None);
    }
}
