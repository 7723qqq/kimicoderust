#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GatingMode {
    Encode,
    Retrieve,
    Consolidate,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GatingInput {
    pub same_cue_repeated: bool,
    pub unstable_recall: bool,
    pub new_scope_or_topic: bool,
    pub positive_feedback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GatingResult {
    pub mode: GatingMode,
    pub expected_uncertainty: f64,
    pub unexpected_uncertainty: f64,
    pub boost: f64,
}

pub fn classify_gating(input: GatingInput) -> GatingResult {
    let expected_uncertainty = if input.same_cue_repeated && input.unstable_recall {
        1.0
    } else {
        0.0
    };
    let unexpected_uncertainty = if input.new_scope_or_topic { 1.0 } else { 0.0 };
    let mode = if unexpected_uncertainty == 1.0 {
        GatingMode::Encode
    } else if expected_uncertainty == 1.0 && !input.positive_feedback {
        GatingMode::Consolidate
    } else {
        GatingMode::Retrieve
    };
    let boost = if input.positive_feedback {
        1.2
    } else if unexpected_uncertainty == 1.0 {
        1.1
    } else {
        1.0
    };
    GatingResult {
        mode,
        expected_uncertainty,
        unexpected_uncertainty,
        boost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(
        same_cue_repeated: bool,
        unstable_recall: bool,
        new_scope_or_topic: bool,
        positive_feedback: bool,
    ) -> GatingInput {
        GatingInput {
            same_cue_repeated,
            unstable_recall,
            new_scope_or_topic,
            positive_feedback,
        }
    }

    #[test]
    fn unexpected_input_forces_encode_with_the_higher_boost() {
        let result = classify_gating(input(false, false, true, false));
        assert_eq!(result.mode, GatingMode::Encode);
        assert_eq!(result.unexpected_uncertainty, 1.0);
        assert_eq!(result.expected_uncertainty, 0.0);
        assert_eq!(result.boost, 1.1);
    }

    #[test]
    fn repeated_and_unstable_consolidates_unless_feedback_is_positive() {
        let consolidating = classify_gating(input(true, true, false, false));
        assert_eq!(consolidating.mode, GatingMode::Consolidate);
        assert_eq!(consolidating.expected_uncertainty, 1.0);
        assert_eq!(consolidating.boost, 1.0);

        let rescued = classify_gating(input(true, true, false, true));
        assert_eq!(rescued.mode, GatingMode::Retrieve);
        assert_eq!(rescued.boost, 1.2);
    }

    #[test]
    fn the_default_is_plain_retrieval() {
        let result = classify_gating(input(false, false, false, false));
        assert_eq!(result.mode, GatingMode::Retrieve);
        assert_eq!(result.boost, 1.0);
        assert_eq!(result.expected_uncertainty, 0.0);
        assert_eq!(result.unexpected_uncertainty, 0.0);
    }
}
