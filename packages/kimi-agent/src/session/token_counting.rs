//! Measured + estimated context sizing (v2 `TokenCountingAgentModel`).
//!
//! A provider reports the exact size of the request it just served; messages
//! appended after that request are estimated until the next measurement. The
//! status gauge, the ACP `usage_update` and the session's context figure read
//! [`TokenCountingState::status_size`], which never reports less than the
//! newest measured total, so the gauge cannot drift below what the model
//! actually consumed.

use serde::{Deserialize, Serialize};

use crate::turn_loop::types::LLMMessage;

/// How the context gauge is derived (v2 `TokenCountingStrategy`,
/// `agent/tokenCounting/configSection.ts`). `MeasuredAndEstimated` is the
/// historical behaviour and the default: the newest measured prefix plus an
/// estimate of everything appended after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TokenCountingStrategy {
    #[default]
    MeasuredAndEstimated,
    /// Only what the provider reported; `0` before the first measurement.
    Measured,
    /// The estimator over the whole history, ignoring anchors.
    Estimated,
}

impl TokenCountingStrategy {
    /// The environment binding (v2 `TOKEN_COUNTING_STRATEGY_ENV`); it wins
    /// over `[tokenCounting].strategy`.
    pub const ENV: &'static str = "KIMI_TOKEN_COUNTING_STRATEGY";

    /// The accepted spellings (v2 `TOKEN_COUNTING_STRATEGIES`).
    pub const VALUES: [&'static str; 3] = ["measured+estimated", "measured", "estimated"];

    /// v2 `parseStrategy`: trim and lowercase, then require an exact match.
    /// `None` is an invalid value, not a request for the default.
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

/// One `length → tokens` point on the conversation: the first `length`
/// messages cost `tokens`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenAnchor {
    /// Message count the anchor covers (the conversation prefix).
    pub length: usize,
    pub tokens: u64,
    /// Whether `tokens` came from a provider response (a measurement) or from
    /// the estimator (a turn-end recording).
    #[serde(default)]
    pub measured: bool,
}

/// The conversation's token-counting state, persisted with the session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenCountingState {
    #[serde(default)]
    pub anchors: Vec<TokenAnchor>,
    /// The newest total (v2 `state.tokens`).
    #[serde(default)]
    pub tokens: u64,
    /// Tokens the active tool schemas (plus the system prompt's fixed
    /// scaffolding) add to every request — v2 `countRequestTokens` overhead.
    /// The gauge adds it to estimate-based figures, and the compaction trigger
    /// uses the same figure, so "how full is the context" has one basis.
    #[serde(default)]
    pub tool_schema_tokens: u64,
}

impl TokenCountingState {
    /// Record one measured request (v2 `TokenCountingMeasured`): the prefix
    /// `length` is exactly `tokens`. Anchors at or beyond that length describe
    /// a prefix this measurement supersedes.
    pub fn measured(&mut self, length: usize, tokens: u64) {
        self.anchors.retain(|anchor| anchor.length < length);
        self.anchors.push(TokenAnchor {
            length,
            tokens,
            measured: true,
        });
        self.tokens = tokens;
    }

    /// Record a turn-end anchor when the turn produced no measurement (v2
    /// `TokenCountingTurnRecorded`). The size is the current status figure.
    ///
    /// v2's rule is narrower than "record a point": when *any* anchor already
    /// covers this length — a measurement or an earlier pin — the turn end
    /// only advances the reported total and leaves the anchor alone.
    /// Overwriting it would downgrade a measurement into an estimate and drop
    /// the [`Self::latest_measured`] floor the status gauge relies on.
    pub fn record_turn(&mut self, history: &[LLMMessage], strategy: TokenCountingStrategy) {
        let length = history.len();
        let tokens = self.status_size(history, strategy);
        if self.anchors.iter().any(|anchor| anchor.length == length) {
            self.tokens = tokens;
            return;
        }
        // Anchors are message-only: `context_size` adds the tool-schema
        // overhead on top, so a stored anchor must not already carry it.
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

    /// The message-only size of `history`: the newest applicable anchor's
    /// prefix plus an estimate for everything after it. A measured anchor's
    /// `tokens` came from the provider and therefore already includes the
    /// tool-schema overhead, so it is removed here to keep the basis uniform.
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

    /// The context size of `history`: the message-only figure plus the request
    /// overhead every call carries (system prompt scaffolding + tool schemas).
    pub fn context_size(&self, history: &[LLMMessage]) -> u64 {
        self.message_size(history) + self.tool_schema_tokens
    }

    /// The gauge figure for the configured strategy (v2 `statusSize`):
    /// - `measured+estimated` (default): the composed size, floored by the
    ///   newest measurement;
    /// - `measured`: the newest measured total only;
    /// - `estimated`: the estimator over the whole history, anchors ignored.
    pub fn status_size(&self, history: &[LLMMessage], strategy: TokenCountingStrategy) -> u64 {
        match strategy {
            TokenCountingStrategy::MeasuredAndEstimated => {
                self.context_size(history).max(self.latest_measured())
            }
            TokenCountingStrategy::Measured => self.latest_measured(),
            // v2 `statusSize('estimated')` = `estimateTokensForMessages(context)`:
            // messages only, no measured anchor and no request overhead.
            TokenCountingStrategy::Estimated => estimate_messages(history),
        }
    }

    /// The newest measured total (v2 `latestMeasured`), `0` before any
    /// measurement.
    pub fn latest_measured(&self) -> u64 {
        self.anchors
            .iter()
            .rev()
            .find(|anchor| anchor.measured)
            .map_or(0, |anchor| anchor.tokens)
    }

    /// A truncation (undo) cut the transcript at `cut`: anchors describing
    /// messages beyond it no longer apply (v2 `recordTruncation`).
    pub fn truncate(&mut self, history: &[LLMMessage], cut: usize) {
        if !self.anchors.iter().any(|anchor| anchor.length > cut) {
            return;
        }
        let cut = cut.min(history.len());
        self.anchors.retain(|anchor| anchor.length <= cut);
        self.tokens = self.context_size(&history[..cut]);
    }

    /// A rebase replaced the transcript with a new prefix (`length` messages
    /// worth `tokens`), e.g. a compaction summary (v2 `rebase`).
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

/// The estimator for one message, shared with compaction so the gauge and the
/// compaction trigger agree about the tail. It counts the role and every
/// content block / tool call (v2 `estimateTokensForMessage`), not just the
/// flattened text.
pub fn estimate_message(message: &LLMMessage) -> u64 {
    u64::from(crate::compaction::estimate_message_tokens(message))
}

/// The estimator over a message list.
pub fn estimate_messages(messages: &[LLMMessage]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes tests that read or write `KIMI_TOKEN_COUNTING_STRATEGY`.
    /// Both the config resolver and the engine read the process environment,
    /// so a parallel pair of tests would otherwise observe each other's value.
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

    /// D10: the default strategy must reproduce the pre-config gauge exactly —
    /// the composed size floored by the newest measurement.
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

    /// `measured` reports only provider measurements; before the first one it
    /// is zero rather than an estimate.
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

    /// `estimated` is the estimator over the whole history: anchors and the
    /// request overhead are ignored, matching v2 `estimateTokensForMessages`.
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

    /// The turn-end recording uses the configured strategy for the reported
    /// total, exactly like v2 `recordTurn(turnId, strategy)`.
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

    /// v2 `parseStrategy`: trim, lowercase, exact match; anything else is
    /// invalid (the caller reports it instead of silently substituting).
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

    /// Without a measurement the size is a pure estimate and never reports a
    /// phantom measured total.
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

    /// A measurement anchors its prefix; messages appended after it are
    /// estimated on top, and the gauge never drops below the measurement.
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

    /// A newer measurement supersedes anchors at or beyond its length — a
    /// shorter measurement means the context it measured was truncated, so the
    /// longer anchor no longer describes it (v2 `TokenCountingMeasured`).
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
        // Anchors strictly below the newest length stay (they describe a
        // shorter prefix the newest one does not cover).
        assert_eq!(state.anchors.len(), 2);
        assert_eq!(state.latest_measured(), 400);
        assert_eq!(state.context_size(&history), 400);
    }

    /// Undo drops anchors beyond the cut and re-derives the size of what
    /// remains; nothing else changes.
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
        // Truncating above every anchor is a no-op.
        let before = state.tokens;
        state.truncate(&history, 3);
        assert_eq!(state.tokens, before);
    }

    /// A rebase (compaction summary) replaces the anchors with one entry.
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

    /// The tail estimator must see everything the request carries, not just
    /// the flattened `content` string: v2 `estimateTokensForMessage` counts the
    /// role, each content block and every tool call's name/arguments.
    #[test]
    fn the_tail_estimate_counts_role_blocks_and_tool_calls() {
        let mut msg = message("assistant", "hello"); // role 3 + content 2
        msg.blocks
            .push(crate::turn_loop::types::ContentBlock::Text {
                text: "1234".into(), // 1 token
                meta: None,
            });
        msg.tool_calls.push(crate::turn_loop::types::ToolCall {
            id: "call-1".into(),
            name: "read".into(),                      // 1 token
            arguments: serde_json::json!({ "a": 1 }), // 7 chars -> 2 tokens
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

    /// A turn-end recording at the same length as an existing turn anchor
    /// updates it in place instead of stacking duplicates.
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

    /// v2 `TokenCountingTurnRecorded`: a turn end at a length an anchor already
    /// covers only advances the reported total. In particular it must not
    /// rewrite a measured anchor into an estimate — that would zero
    /// `latest_measured` and let the gauge drift below the real size.
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

    /// The tool schemas are part of every request, so the gauge counts them.
    /// A measurement already covers them (the provider charged the whole
    /// request), so they must not be added a second time on that prefix.
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

    /// A turn-end anchor stores the message-only figure, so the overhead is
    /// not baked into the anchor and then added again by the gauge.
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
