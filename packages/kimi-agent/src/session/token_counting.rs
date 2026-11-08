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
    pub fn record_turn(&mut self, history: &[LLMMessage]) {
        let length = history.len();
        let tokens = self.status_size(history);
        if self.anchors.iter().any(|anchor| anchor.length == length) {
            self.tokens = tokens;
            return;
        }
        self.anchors.retain(|anchor| anchor.length < length);
        self.anchors.push(TokenAnchor {
            length,
            tokens,
            measured: false,
        });
        self.tokens = tokens;
    }

    /// The context size of `history`: the newest applicable anchor's measured
    /// prefix plus an estimate for everything after it (v2 `get`).
    pub fn context_size(&self, history: &[LLMMessage]) -> u64 {
        let Some(anchor) = self.anchor_for(history.len()) else {
            return estimate_messages(history);
        };
        let measured = anchor.tokens;
        let tail: u64 = history[anchor.length.min(history.len())..]
            .iter()
            .map(estimate_message)
            .sum();
        measured + tail
    }

    /// The gauge figure for the `measured+estimated` strategy (v2
    /// `statusSize`): the composed size, floored by the newest measurement.
    pub fn status_size(&self, history: &[LLMMessage]) -> u64 {
        self.context_size(history).max(self.latest_measured())
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
/// compaction trigger agree about the tail.
pub fn estimate_message(message: &LLMMessage) -> u64 {
    crate::compaction::estimate_tokens(&message.content) as u64
}

/// The estimator over a message list.
pub fn estimate_messages(messages: &[LLMMessage]) -> u64 {
    messages.iter().map(estimate_message).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> LLMMessage {
        LLMMessage {
            role: role.into(),
            content: content.into(),
            ..Default::default()
        }
    }

    /// Without a measurement the size is a pure estimate and never reports a
    /// phantom measured total.
    #[test]
    fn an_unmeasured_state_is_all_estimate() {
        let history = vec![message("user", &"a".repeat(400))];
        let state = TokenCountingState::default();
        assert_eq!(state.latest_measured(), 0);
        assert_eq!(state.status_size(&history), estimate_messages(&history));
        assert_eq!(state.status_size(&history), 100, "400 bytes ≈ 100 tokens");
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
        assert_eq!(state.context_size(&history), 250 + 50);
        assert_eq!(state.status_size(&history), 300);
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
        assert_eq!(state.status_size(&history), 800);
    }

    /// A turn-end recording at the same length as an existing turn anchor
    /// updates it in place instead of stacking duplicates.
    #[test]
    fn turn_recordings_do_not_stack_at_the_same_length() {
        let history = vec![message("user", "hello")];
        let mut state = TokenCountingState::default();
        state.record_turn(&history);
        let first = state.tokens;
        state.record_turn(&history);
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

        state.record_turn(&history);

        assert_eq!(state.anchors.len(), 1);
        assert!(
            state.anchors[0].measured,
            "the anchor keeps its measurement"
        );
        assert_eq!(state.anchors[0].tokens, 200);
        assert_eq!(state.latest_measured(), 200);
        assert_eq!(state.status_size(&history), 200);
        assert_eq!(state.tokens, 200);
    }
}
