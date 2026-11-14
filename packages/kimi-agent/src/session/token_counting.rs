use serde::{Deserialize, Serialize};

use crate::turn_loop::types::LLMMessage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenCountingStrategy {
    #[default]
    MeasuredAndEstimated,

    Measured,

    Estimated,
}

impl TokenCountingStrategy {
    pub const ENV: &'static str = "KIMI_TOKEN_COUNTING_STRATEGY";

    pub const VALUES: [&'static str; 3] = ["measured+estimated", "measured", "estimated"];

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "measured+estimated" => Some(Self::MeasuredAndEstimated),
            "measured" => Some(Self::Measured),
            "estimated" => Some(Self::Estimated),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MeasuredAndEstimated => "measured+estimated",
            Self::Measured => "measured",
            Self::Estimated => "estimated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAnchor {
    pub length: usize,
    pub tokens: u64,

    #[serde(default)]
    pub measured: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenCountingState {
    #[serde(default)]
    pub anchors: Vec<TokenAnchor>,

    #[serde(default)]
    pub tokens: u64,

    #[serde(default)]
    pub tool_schema_tokens: u64,
}

impl TokenCountingState {
    pub fn measured(&mut self, length: usize, tokens: u64) {
        self.anchors.retain(|anchor| anchor.length < length);
        self.anchors.push(TokenAnchor {
            length,
            tokens,
            measured: true,
        });
        self.tokens = tokens;
    }

    pub fn record_turn(&mut self, history: &[LLMMessage], strategy: TokenCountingStrategy) {
        let length = history.len();
        let tokens = self.status_size(history, strategy);
        if self.anchors.iter().any(|anchor| anchor.length == length) {
            self.tokens = tokens;
            return;
        }

        let anchor_tokens = self
            .context_size(history)
            .saturating_sub(self.tool_schema_tokens);
        self.anchors.retain(|anchor| anchor.length < length);
        self.anchors.push(TokenAnchor {
            length,
            tokens: anchor_tokens,
            measured: false,
        });
        self.tokens = tokens;
    }

    fn message_size(&self, history: &[LLMMessage]) -> u64 {
        let Some(anchor) = self.anchor_for(history.len()) else {
            return estimate_messages(history);
        };
        let tail: u64 = history[anchor.length.min(history.len())..]
            .iter()
            .map(estimate_message)
            .sum();
        let basis = if anchor.measured {
            anchor.tokens.saturating_sub(self.tool_schema_tokens)
        } else {
            anchor.tokens
        };
        basis + tail
    }

    pub fn context_size(&self, history: &[LLMMessage]) -> u64 {
        self.message_size(history) + self.tool_schema_tokens
    }

    pub fn status_size(&self, history: &[LLMMessage], strategy: TokenCountingStrategy) -> u64 {
        match strategy {
            TokenCountingStrategy::MeasuredAndEstimated => {
                self.context_size(history).max(self.latest_measured())
            }
            TokenCountingStrategy::Measured => self.latest_measured(),

            TokenCountingStrategy::Estimated => estimate_messages(history),
        }
    }

    pub fn latest_measured(&self) -> u64 {
        self.anchors
            .iter()
            .rev()
            .find(|anchor| anchor.measured)
            .map_or(0, |anchor| anchor.tokens)
    }

    pub fn truncate(&mut self, history: &[LLMMessage], cut: usize) {
        if !self.anchors.iter().any(|anchor| anchor.length > cut) {
            return;
        }
        let cut = cut.min(history.len());
        self.anchors.retain(|anchor| anchor.length <= cut);
        self.tokens = self.context_size(&history[..cut]);
    }

    pub fn rebase(&mut self, length: usize, tokens: u64, measured: bool) {
        self.anchors = vec![TokenAnchor {
            length,
            tokens,
            measured,
        }];
        self.tokens = tokens;
    }

    fn anchor_for(&self, length: usize) -> Option<&TokenAnchor> {
        self.anchors
            .iter()
            .rev()
            .find(|anchor| anchor.length <= length)
    }
}

pub fn estimate_message(message: &LLMMessage) -> u64 {
    u64::from(crate::compaction::estimate_message_tokens(message))
}

pub fn estimate_messages(messages: &[LLMMessage]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn message(role: &str, content: &str) -> LLMMessage {
        LLMMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_default_strategy_is_the_historical_measured_plus_estimated() {
        let history = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        state.tool_schema_tokens = 700;
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            state.context_size(&history).max(state.latest_measured())
        );
        state.measured(1, 950);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            950
        );
        let mut longer = history.clone();
        longer.push(message("assistant", &"b".repeat(200)));
        assert_eq!(
            state.status_size(&longer, TokenCountingStrategy::default()),
            950 + 53
        );
    }

    #[test]
    fn the_measured_strategy_ignores_estimates() {
        let history = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::Measured),
            0
        );
        state.measured(1, 250);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::Measured),
            250
        );
        let mut longer = history.clone();
        longer.push(message("assistant", &"b".repeat(200)));
        assert_eq!(
            state.status_size(&longer, TokenCountingStrategy::Measured),
            250,
            "the tail is not estimated"
        );
    }

    #[test]
    fn the_estimated_strategy_ignores_anchors_and_overhead() {
        let history = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        state.tool_schema_tokens = 700;
        state.measured(1, 950);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::Estimated),
            estimate_messages(&history)
        );
        assert_eq!(estimate_messages(&history), 101);
    }

    #[test]
    fn a_turn_end_records_the_configured_strategy() {
        let history = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        state.measured(1, 950);
        state.record_turn(&history, TokenCountingStrategy::Estimated);
        assert_eq!(state.tokens, 101);
        state.record_turn(&history, TokenCountingStrategy::Measured);
        assert_eq!(state.tokens, 950);
    }

    #[test]
    fn strategy_spellings_parse_strictly() {
        assert_eq!(
            TokenCountingStrategy::parse("  MEASURED+Estimated "),
            Some(TokenCountingStrategy::MeasuredAndEstimated)
        );
        assert_eq!(
            TokenCountingStrategy::parse("measured"),
            Some(TokenCountingStrategy::Measured)
        );
        assert_eq!(
            TokenCountingStrategy::parse("estimated"),
            Some(TokenCountingStrategy::Estimated)
        );
        for invalid in ["", "estimate", "measured+estimated2", "both"] {
            assert_eq!(TokenCountingStrategy::parse(invalid), None, "{invalid:?}");
        }
        for value in TokenCountingStrategy::VALUES {
            assert_eq!(TokenCountingStrategy::parse(value).unwrap().as_str(), value);
        }
    }

    #[test]
    fn an_unmeasured_state_is_all_estimate() {
        let history = vec![message("user", &"a".repeat(400))];
        let state = TokenCountingState::default();
        assert_eq!(state.latest_measured(), 0);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            estimate_messages(&history)
        );
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            101,
            "role (1) + 400 bytes ≈ 101 tokens"
        );
    }

    #[test]
    fn a_measurement_anchors_the_prefix_and_the_tail_is_estimated() {
        let prefix = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        state.measured(prefix.len(), 250);

        assert_eq!(state.context_size(&prefix), 250);
        let mut history = prefix.clone();
        history.push(message("assistant", &"b".repeat(200)));
        assert_eq!(state.context_size(&history), 250 + 53);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            303
        );
        assert_eq!(state.latest_measured(), 250);
    }

    #[test]
    fn a_newer_measurement_supersedes_anchors_at_or_beyond_it() {
        let history: Vec<LLMMessage> = (0..4)
            .map(|index| message("user", &format!("m{index}")))
            .collect();
        let mut state = TokenCountingState::default();
        state.measured(3, 300);
        state.measured(2, 120);
        assert_eq!(state.latest_measured(), 120);
        assert_eq!(state.anchors.len(), 1, "the shorter measurement wins");
        state.measured(4, 400);

        assert_eq!(state.anchors.len(), 2);
        assert_eq!(state.latest_measured(), 400);
        assert_eq!(state.context_size(&history), 400);
    }

    #[test]
    fn truncation_drops_anchors_beyond_the_cut() {
        let history: Vec<LLMMessage> = (0..4)
            .map(|index| message("user", &format!("m{index}")))
            .collect();
        let mut state = TokenCountingState::default();
        state.measured(2, 100);
        state.measured(4, 200);
        state.truncate(&history, 2);
        assert_eq!(state.latest_measured(), 100);
        assert_eq!(state.context_size(&history[..2]), 100);
        assert!(state.anchors.iter().all(|anchor| anchor.length <= 2));

        let before = state.tokens;
        state.truncate(&history, 3);
        assert_eq!(state.tokens, before);
    }

    #[test]
    fn rebase_replaces_the_state_with_one_anchor() {
        let mut state = TokenCountingState::default();
        state.measured(4, 200);
        state.rebase(1, 800, false);
        assert_eq!(state.anchors.len(), 1);
        assert_eq!(state.tokens, 800);
        assert_eq!(state.latest_measured(), 0, "a rebase is not a measurement");
        let history = vec![message("user", "summary")];
        assert_eq!(state.context_size(&history), 800);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            800
        );
    }

    #[test]
    fn the_tail_estimate_counts_role_blocks_and_tool_calls() {
        let mut msg = message("assistant", "hello");
        msg.blocks
            .push(crate::turn_loop::types::ContentBlock::Text {
                text: "1234".into(),
                meta: None,
            });
        msg.tool_calls.push(crate::turn_loop::types::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "a": 1 }),
            extras: None,
            args_parse_error: None,
        });
        assert_eq!(estimate_message(&msg), 3 + 2 + 1 + 1 + 2);

        let plain = message("user", "1234");
        assert_eq!(
            estimate_message(&plain),
            1 + 1,
            "the role counts even when there is no block or tool call"
        );
    }

    #[test]
    fn turn_recordings_do_not_stack_at_the_same_length() {
        let history = vec![message("user", "hello")];
        let mut state = TokenCountingState::default();
        state.record_turn(&history, TokenCountingStrategy::default());
        let first = state.tokens;
        state.record_turn(&history, TokenCountingStrategy::default());
        assert_eq!(state.tokens, first);
        assert_eq!(state.anchors.len(), 1);
    }

    #[test]
    fn a_turn_end_never_downgrades_a_measurement() {
        let history: Vec<LLMMessage> = (0..6)
            .map(|index| message("user", &format!("m{index}")))
            .collect();
        let mut state = TokenCountingState::default();
        state.measured(6, 200);

        state.record_turn(&history, TokenCountingStrategy::default());

        assert_eq!(state.anchors.len(), 1);
        assert!(
            state.anchors[0].measured,
            "the anchor keeps its measurement"
        );
        assert_eq!(state.anchors[0].tokens, 200);
        assert_eq!(state.latest_measured(), 200);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            200
        );
        assert_eq!(state.tokens, 200);
    }

    #[test]
    fn the_tool_schema_is_counted_once_on_the_gauge() {
        let history = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        state.tool_schema_tokens = 700;
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            801,
            "the estimate path adds the schema overhead"
        );

        state.measured(1, 950);
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            950,
            "a measurement already includes the schema, so it is not added again"
        );

        let mut longer = history.clone();
        longer.push(message("assistant", &"b".repeat(200)));
        assert_eq!(
            state.status_size(&longer, TokenCountingStrategy::default()),
            950 + 53,
            "only the tail is estimated on top of the measurement"
        );
    }

    #[test]
    fn a_turn_end_anchor_does_not_double_count_the_schema() {
        let history = vec![message("user", &"a".repeat(400))];
        let mut state = TokenCountingState::default();
        state.tool_schema_tokens = 700;
        state.record_turn(&history, TokenCountingStrategy::default());
        assert_eq!(state.anchors[0].tokens, 101, "the anchor is message-only");
        assert_eq!(
            state.status_size(&history, TokenCountingStrategy::default()),
            801
        );
    }
}
