use crate::tool_input_display::PlanReviewOption;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecReviewDecision {
    Reject,
    RejectAndExit,
    Revise,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecReviewOutcome {
    pub is_error: bool,
    pub stop_turn: bool,
    pub output: String,
    pub telemetry_outcome: &'static str,
    pub has_feedback: bool,
}

pub fn selected_spec_option<'a>(
    options: Option<&'a [PlanReviewOption]>,
    label: Option<&str>,
) -> Option<&'a PlanReviewOption> {
    let label = label?;
    options?.iter().find(|option| option.label == label)
}

pub fn approved_output(dir: &str, selected: Option<&PlanReviewOption>) -> String {
    let option_prefix = selected
        .map(|option| {
            format!(
                "Selected approach: {}\nExecute ONLY the selected approach. Do not execute any unselected alternatives.\n\n",
                option.label
            )
        })
        .unwrap_or_default();
    format!(
        "Exited spec mode. {option_prefix}Spec saved to: {dir}\n\nWork through {dir}/tasks.md, recording progress in {dir}/progress.md as you go."
    )
}

pub fn rejected_outcome(decision: SpecReviewDecision, feedback: &str) -> SpecReviewOutcome {
    match decision {
        SpecReviewDecision::RejectAndExit => SpecReviewOutcome {
            is_error: true,
            stop_turn: true,
            output: "Spec rejected by user. Spec mode deactivated.".to_string(),
            telemetry_outcome: "rejected_and_exited",
            has_feedback: false,
        },
        SpecReviewDecision::Revise => SpecReviewOutcome {
            is_error: false,
            stop_turn: false,
            output: if feedback.is_empty() {
                "User requested revisions. Spec mode remains active.".to_string()
            } else {
                format!("User rejected the spec. Feedback:\n\n{feedback}")
            },
            telemetry_outcome: "revise",
            has_feedback: !feedback.is_empty(),
        },
        SpecReviewDecision::Reject if !feedback.is_empty() => SpecReviewOutcome {
            is_error: false,
            stop_turn: false,
            output: format!("User rejected the spec. Feedback:\n\n{feedback}"),
            telemetry_outcome: "revise",
            has_feedback: true,
        },
        SpecReviewDecision::Reject => SpecReviewOutcome {
            is_error: true,
            stop_turn: true,
            output: "Spec rejected by user. Spec mode remains active.".to_string(),
            telemetry_outcome: "rejected",
            has_feedback: false,
        },
    }
}

pub fn dismissed_outcome() -> SpecReviewOutcome {
    SpecReviewOutcome {
        is_error: false,
        stop_turn: false,
        output: "Spec approval dismissed. Spec mode remains active.".to_string(),
        telemetry_outcome: "dismissed",
        has_feedback: false,
    }
}
