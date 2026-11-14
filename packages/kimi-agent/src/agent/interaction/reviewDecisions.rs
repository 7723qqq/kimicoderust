use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
    RejectAndExit,
    Revise,
}

impl ReviewDecision {
    pub const ALL: [Self; 4] = [
        Self::Approve,
        Self::Reject,
        Self::RejectAndExit,
        Self::Revise,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::RejectAndExit => "reject_and_exit",
            Self::Revise => "revise",
        }
    }
}

pub type PlanReviewDecision = ReviewDecision;
pub type SpecReviewDecision = ReviewDecision;

pub fn review_decision_from_label(label: Option<&str>) -> Option<ReviewDecision> {
    let label = label.filter(|label| !label.is_empty())?;
    for decision in ReviewDecision::ALL {
        if decision.as_str() == label {
            return Some(decision);
        }
    }
    match label {
        "Reject and Exit" => Some(ReviewDecision::RejectAndExit),
        "Revise" => Some(ReviewDecision::Revise),
        "Reject" => Some(ReviewDecision::Reject),
        "Approve" => Some(ReviewDecision::Approve),
        _ => None,
    }
}

pub const REVIEW_DECISION_LABELS: [(&str, &str); 4] = [
    ("approve", "Approve"),
    ("reject", "Reject"),
    ("reject_and_exit", "Reject and Exit"),
    ("revise", "Revise"),
];

pub fn review_decision_label(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approve => "Approve",
        ReviewDecision::Reject => "Reject",
        ReviewDecision::RejectAndExit => "Reject and Exit",
        ReviewDecision::Revise => "Revise",
    }
}
