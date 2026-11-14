use std::sync::Arc;

use serde_json::{Map, Value};

use super::plan::IAgentPlanService;
use crate::agent::interaction::approval::{ApprovalDecision, ApprovalResponse};
use crate::agent::interaction::review_decisions::{ReviewDecision, review_decision_from_label};
use crate::agent::permission_policy::types::{PermissionPolicyResolution, PermissionPolicyResult};
use crate::agent::tool_approval::tool_approval::IAgentToolApprovalService;
use crate::agent::tool_executor::tool_executor::ExecutableToolResult;
use crate::agent::tool_executor::tool_hooks::{BeforeExecuteDecision, ResolvedToolExecutionHookContext};
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::tool_input_display::{PlanReviewOption, ToolInputDisplay};
use crate::wire::record::WireAsyncError;

pub const PLAN_REVIEW_NON_INTERACTIVE_MESSAGE: &str = "Nobody can review this plan in a non-interactive run, so plan mode stays active for the rest of this run and you cannot edit files other than the plan file.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewDecision {
    Reject,
    RejectAndExit,
    Revise,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReviewOutcome {
    pub is_error: bool,
    pub stop_turn: bool,
    pub output: String,
    pub telemetry_outcome: &'static str,
    pub has_feedback: bool,
}

#[derive(Debug, Clone)]
struct PlanReviewDisplay {
    plan: String,
    path: Option<String>,
    options: Option<Vec<PlanReviewOption>>,
}

#[derive(Clone)]
pub struct ExitPlanModeReview {
    plan: Arc<dyn IAgentPlanService>,
    tool_approval: Arc<dyn IAgentToolApprovalService>,
    telemetry: Arc<dyn ITelemetryService>,
}

impl ExitPlanModeReview {
    pub fn new(
        plan: Arc<dyn IAgentPlanService>,
        tool_approval: Arc<dyn IAgentToolApprovalService>,
        telemetry: Arc<dyn ITelemetryService>,
    ) -> Self {
        Self {
            plan,
            tool_approval,
            telemetry,
        }
    }

    pub async fn request_approval(
        &self,
        context: ResolvedToolExecutionHookContext,
    ) -> Result<Option<BeforeExecuteDecision>, WireAsyncError> {
        let Some(ToolInputDisplay::PlanReview { plan, path, options }) =
            context.execution.display.clone()
        else {
            return Ok(None);
        };
        if trim_js(&plan).is_empty() {
            return Ok(None);
        }
        let display = PlanReviewDisplay { plan, path, options };
        self.track_plan_telemetry(
            "plan_submitted",
            Map::from_iter([(
                "has_options".to_string(),
                Value::Bool(
                    display
                        .options
                        .as_ref()
                        .is_some_and(|options| options.len() >= 2),
                ),
            )]),
        );
        let review = self.clone();
        let approval_display = display.clone();
        let resolve_approval = Arc::new(move |result: ApprovalResponse| {
            review.approval_result(&result, &approval_display)
        });
        self.tool_approval
            .request_tool_approval(
                context,
                PermissionPolicyResult::Ask {
                    reason: Some(Map::from_iter([(
                        "has_options".to_string(),
                        Value::Bool(display.options.is_some()),
                    )])),
                    resolve_approval: Some(resolve_approval),
                    resolve_error: None,
                    resolve_non_interactive: Some(Arc::new(|| PermissionPolicyResolution::Deny {
                        reason: None,
                        message: Some(PLAN_REVIEW_NON_INTERACTIVE_MESSAGE.to_string()),
                    })),
                },
                "exit-plan-mode-review-ask".to_string(),
            )
            .await
    }

    fn approval_result(
        &self,
        result: &ApprovalResponse,
        display: &PlanReviewDisplay,
    ) -> Option<PermissionPolicyResolution> {
        if result.decision != ApprovalDecision::Approved {
            return Some(self.rejected_approval_result(result));
        }
        let selected = selected_exit_plan_mode_option(
            display.options.as_deref(),
            result.selected_label.as_deref(),
        );
        let decision = review_decision_from_label(result.selected_label.as_deref());
        self.plan.exit(None);
        if let Some(label) = result
            .selected_label
            .as_deref()
            .filter(|label| !label.is_empty())
        {
            self.track_plan_telemetry(
                "plan_resolved",
                Map::from_iter([
                    ("outcome".to_string(), Value::String("approved".to_string())),
                    (
                        "chosen_option".to_string(),
                        Value::String(
                            decision
                                .map(|decision| decision.as_str().to_string())
                                .unwrap_or_else(|| label.to_string()),
                        ),
                    ),
                ]),
            );
        } else {
            self.track_plan_telemetry(
                "plan_resolved",
                Map::from_iter([(
                    "outcome".to_string(),
                    Value::String("approved".to_string()),
                )]),
            );
        }
        Some(PermissionPolicyResolution::Result {
            result: ExecutableToolResult {
                output: approved_output(&display.plan, display.path.as_deref(), selected).into(),
                is_error: Some(false),
                ..Default::default()
            },
        })
    }

    fn rejected_approval_result(&self, result: &ApprovalResponse) -> PermissionPolicyResolution {
        self.track_rejected_plan_resolution(result);
        if result.decision == ApprovalDecision::Cancelled {
            return review_outcome(dismissed_outcome());
        }
        let decision = rejected_decision(result.selected_label.as_deref());
        if decision == Some(PlanReviewDecision::RejectAndExit) {
            self.plan.exit(None);
        }
        review_outcome(rejected_outcome(
            decision.unwrap_or(PlanReviewDecision::Reject),
            result.feedback.as_deref().unwrap_or(""),
        ))
    }

    fn track_rejected_plan_resolution(&self, result: &ApprovalResponse) {
        let outcome = if result.decision == ApprovalDecision::Cancelled {
            dismissed_outcome()
        } else {
            rejected_outcome(
                rejected_decision(result.selected_label.as_deref())
                    .unwrap_or(PlanReviewDecision::Reject),
                result.feedback.as_deref().unwrap_or(""),
            )
        };
        let mut properties = Map::from_iter([(
            "outcome".to_string(),
            Value::String(outcome.telemetry_outcome.to_string()),
        )]);
        if outcome.telemetry_outcome == "revise" {
            properties.insert("has_feedback".to_string(), Value::Bool(outcome.has_feedback));
        }
        self.track_plan_telemetry("plan_resolved", properties);
    }

    fn track_plan_telemetry(&self, event: &str, properties: Map<String, Value>) {
        self.telemetry.track2(event, Some(properties));
    }
}

fn rejected_decision(label: Option<&str>) -> Option<PlanReviewDecision> {
    match review_decision_from_label(label) {
        Some(ReviewDecision::Reject) => Some(PlanReviewDecision::Reject),
        Some(ReviewDecision::RejectAndExit) => Some(PlanReviewDecision::RejectAndExit),
        Some(ReviewDecision::Revise) => Some(PlanReviewDecision::Revise),
        _ => None,
    }
}

fn review_outcome(outcome: PlanReviewOutcome) -> PermissionPolicyResolution {
    PermissionPolicyResolution::Result {
        result: ExecutableToolResult {
            output: outcome.output.into(),
            is_error: Some(outcome.is_error),
            stop_turn: outcome.stop_turn.then_some(true),
            ..Default::default()
        },
    }
}

pub fn selected_exit_plan_mode_option<'a>(
    options: Option<&'a [PlanReviewOption]>,
    label: Option<&str>,
) -> Option<&'a PlanReviewOption> {
    let label = label?;
    options?.iter().find(|option| option.label == label)
}

pub fn approved_output(
    plan: &str,
    path: Option<&str>,
    selected: Option<&PlanReviewOption>,
) -> String {
    let option_prefix = selected
        .map(|option| {
            format!(
                "Selected approach: {}\nExecute ONLY the selected approach. Do not execute any unselected alternatives.\n\n",
                option.label
            )
        })
        .unwrap_or_default();
    let saved_to = path
        .map(|path| format!("Plan saved to: {path}\n\n"))
        .unwrap_or_default();
    format!(
        "Exited plan mode. {option_prefix}Plan mode deactivated. All tools are now available.\n{saved_to}## Approved Plan:\n{plan}"
    )
}

pub fn rejected_outcome(decision: PlanReviewDecision, feedback: &str) -> PlanReviewOutcome {
    match decision {
        PlanReviewDecision::RejectAndExit => PlanReviewOutcome {
            is_error: true,
            stop_turn: true,
            output: "Plan rejected by user. Plan mode deactivated.".to_string(),
            telemetry_outcome: "rejected_and_exited",
            has_feedback: false,
        },
        PlanReviewDecision::Revise => PlanReviewOutcome {
            is_error: false,
            stop_turn: false,
            output: if feedback.is_empty() {
                "User requested revisions. Plan mode remains active.".to_string()
            } else {
                format!("User rejected the plan. Feedback:\n\n{feedback}")
            },
            telemetry_outcome: "revise",
            has_feedback: !feedback.is_empty(),
        },
        PlanReviewDecision::Reject if !feedback.is_empty() => PlanReviewOutcome {
            is_error: false,
            stop_turn: false,
            output: format!("User rejected the plan. Feedback:\n\n{feedback}"),
            telemetry_outcome: "revise",
            has_feedback: true,
        },
        PlanReviewDecision::Reject => PlanReviewOutcome {
            is_error: true,
            stop_turn: true,
            output: "Plan rejected by user. Plan mode remains active.".to_string(),
            telemetry_outcome: "rejected",
            has_feedback: false,
        },
    }
}

pub fn dismissed_outcome() -> PlanReviewOutcome {
    PlanReviewOutcome {
        is_error: false,
        stop_turn: false,
        output: "Plan approval dismissed. Plan mode remains active.".to_string(),
        telemetry_outcome: "dismissed",
        has_feedback: false,
    }
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}
