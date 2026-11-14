pub struct GoalPauseReason;

impl GoalPauseReason {
    pub const RATE_LIMIT: &'static str = "Paused after provider rate limit";
    pub const PROVIDER_CONNECTION: &'static str = "Paused after provider connection error";
    pub const PROVIDER_AUTH: &'static str = "Paused after provider authentication error";
    pub const PROVIDER_API: &'static str = "Paused after provider API error";
    pub const MODEL_CONFIG: &'static str = "Paused after model configuration error";
    pub const RUNTIME: &'static str = "Paused after runtime error";
    pub const CONTINUATION_FAILURE: &'static str = "Paused after goal continuation failure";
    pub const PROVIDER_FILTERED: &'static str = "Paused after provider safety policy block";
    pub const INTERRUPTION: &'static str = "Paused after interruption";
}

pub const GOAL_PAUSE_REASONS_WITH_DETAIL: [&str; 6] = [
    GoalPauseReason::PROVIDER_CONNECTION,
    GoalPauseReason::PROVIDER_AUTH,
    GoalPauseReason::PROVIDER_API,
    GoalPauseReason::MODEL_CONFIG,
    GoalPauseReason::RUNTIME,
    GoalPauseReason::CONTINUATION_FAILURE,
];

const PAUSE_REASON_PREFIXES: [&str; 9] = [
    GoalPauseReason::RATE_LIMIT,
    GoalPauseReason::PROVIDER_CONNECTION,
    GoalPauseReason::PROVIDER_AUTH,
    GoalPauseReason::PROVIDER_API,
    GoalPauseReason::MODEL_CONFIG,
    GoalPauseReason::RUNTIME,
    GoalPauseReason::CONTINUATION_FAILURE,
    GoalPauseReason::PROVIDER_FILTERED,
    GoalPauseReason::INTERRUPTION,
];

pub fn goal_pause_reason_with_detail(reason: &str, detail: &str) -> String {
    format!("{reason}: {detail}")
}

pub fn is_goal_pause_reason(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| {
        PAUSE_REASON_PREFIXES
            .iter()
            .any(|prefix| reason.starts_with(prefix))
    })
}

pub const SUBAGENT_CANCELLATION_MESSAGES: [&str; 4] = [
    "Aborted by the user",
    "The user manually interrupted this subagent batch.",
    "The user manually interrupted this subagent batch before this subagent finished.",
    "The user manually interrupted this subagent batch before this subagent was started.",
];

pub fn is_subagent_cancellation_message(output: &str) -> bool {
    let trimmed = output.trim_matches(is_js_whitespace);
    SUBAGENT_CANCELLATION_MESSAGES
        .iter()
        .any(|message| *message == trimmed)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}
