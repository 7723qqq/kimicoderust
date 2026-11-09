//! napi projections of the compaction window algorithm.
//!
//! The algorithm itself lives in [`crate::compaction::window`]; this module
//! only defines the objects that cross the napi boundary and forwards to it.
//! The TS side builds `CompactionMessageMeta[]` from `Message[]` (one pass,
//! using the cached `estimateTokensForMessage`) and passes it across the
//! boundary; Rust only sees the lightweight projection.

#[cfg(feature = "napi")]
use napi_derive::napi;

use crate::compaction::CompactionConfig;
use crate::compaction::window::{self, CompactMessage, UserMessageSelection};

/// Lightweight projection of a `Message` for the compaction algorithm.
///
/// Only the fields the windowing logic actually inspects cross the
/// boundary: `role` (for split-safety checks), `tool_calls_count` (to
/// detect pending tool exchanges), and `tokens` (pre-computed by TS via
/// the cached `estimateTokensForMessage`).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "napi", napi(object))]
pub struct CompactionMessageMeta {
    pub role: String,
    pub tool_calls_count: u32,
    pub tokens: u32,
}

/// Knobs for the compaction algorithm.
///
/// `max_recent_user_messages` uses `u32::MAX` as a stand-in for TS
/// `Infinity` (the `DEFAULT_COMPACTION_CONFIG` default). The threshold
/// `recent_user_messages >= max_recent_user_messages` then never fires
/// for realistic message counts, matching the TS behavior.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "napi", napi(object))]
pub struct CompactionConfigMeta {
    pub max_size: u32,
    pub max_recent_messages: u32,
    pub max_recent_user_messages: u32,
    pub max_recent_size_ratio: f64,
    pub min_overflow_reduction_ratio: f64,
}

impl CompactMessage for CompactionMessageMeta {
    fn role(&self) -> &str {
        &self.role
    }

    fn tool_calls_count(&self) -> u32 {
        self.tool_calls_count
    }

    fn tokens(&self) -> u32 {
        self.tokens
    }

    fn text(&self) -> &str {
        ""
    }
}

impl From<CompactionConfigMeta> for CompactionConfig {
    fn from(config: CompactionConfigMeta) -> Self {
        Self {
            max_context_tokens: config.max_size,
            max_recent_messages: config.max_recent_messages,
            max_recent_user_messages: config.max_recent_user_messages,
            max_recent_size_ratio: config.max_recent_size_ratio,
            ..Self::default()
        }
    }
}

/// A user message projected for handoff selection. Mirrors the TS
/// `MessageLike` subset the handoff algorithm inspects. TS extracts text
/// from `content` (skipping non-text parts) and pre-estimates tokens;
/// Rust only needs the projection.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "napi", napi(object))]
pub struct HandoffMessageMeta {
    pub role: String,
    pub text: String,
    pub tokens: u32,
}

impl CompactMessage for HandoffMessageMeta {
    fn role(&self) -> &str {
        &self.role
    }

    fn tool_calls_count(&self) -> u32 {
        0
    }

    fn tokens(&self) -> u32 {
        self.tokens
    }

    fn text(&self) -> &str {
        &self.text
    }
}

/// Result of `select_compaction_user_messages`. Indices reference the input
/// `Vec<HandoffMessageMeta>` (user messages only, in original order).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "napi", napi(object))]
pub struct CompactionUserSelection {
    pub head_indices: Vec<u32>,
    pub tail_indices: Vec<u32>,
    pub head_truncate_chars: Option<u32>,
    pub tail_truncate_chars: Option<u32>,
    pub elided: bool,
    pub omitted_tokens: u32,
}

impl From<UserMessageSelection> for CompactionUserSelection {
    fn from(selection: UserMessageSelection) -> Self {
        Self {
            head_indices: selection.head_indices,
            tail_indices: selection.tail_indices,
            head_truncate_chars: selection.head_truncate.map(|(_, kept)| kept),
            tail_truncate_chars: selection.tail_truncate.map(|(_, kept)| kept),
            elided: selection.elided,
            omitted_tokens: selection.omitted_tokens,
        }
    }
}

/// Decide how many leading messages to compact.
///
/// Returns N where `messages[0..N]` is compacted and `messages[N..]` is
/// preserved. 0 means no compaction possible (no valid split point).
pub(crate) fn compute_compact_count(
    messages: &[CompactionMessageMeta],
    config: &CompactionConfigMeta,
    is_manual: bool,
) -> u32 {
    window::compute_compact_count(messages, &CompactionConfig::from(*config), is_manual)
}

/// Find a split point when the LLM throws a context overflow error.
pub(crate) fn reduce_compact_on_overflow(
    messages: &[CompactionMessageMeta],
    config: &CompactionConfigMeta,
) -> u32 {
    window::reduce_compact_on_overflow(
        messages,
        config.max_size,
        config.min_overflow_reduction_ratio,
    )
}

/// Whether a compaction split is safe to place immediately after
/// `messages[index]`.
pub(crate) fn can_split_after(messages: &[CompactionMessageMeta], index: usize) -> bool {
    window::can_split_after(messages, index)
}

/// Select user messages compaction keeps verbatim, with head/tail split.
pub(crate) fn select_compaction_user_messages(
    messages: &[HandoffMessageMeta],
    max_tokens: u32,
    head_tokens: u32,
) -> CompactionUserSelection {
    window::select_user_messages(messages, max_tokens, head_tokens).into()
}

#[cfg(test)]
fn prefix_ends_with_open_tool_exchange(messages: &[CompactionMessageMeta], index: usize) -> bool {
    window::prefix_ends_with_open_tool_exchange(messages, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, tokens: u32, tool_calls: u32) -> CompactionMessageMeta {
        CompactionMessageMeta {
            role: role.to_string(),
            tool_calls_count: tool_calls,
            tokens,
        }
    }

    // Mirror of testCompactionStrategy() in strategy.test.ts.
    fn default_config(max_size: u32) -> CompactionConfigMeta {
        CompactionConfigMeta {
            max_size,
            max_recent_messages: 10,
            max_recent_user_messages: u32::MAX,
            max_recent_size_ratio: 0.2,
            min_overflow_reduction_ratio: 0.05,
        }
    }

    // ---- compute_compact_count: auto path ----

    #[test]
    fn keeps_oversized_trailing_user_as_recent() {
        // user, assistant, user(oversized)
        let messages = vec![
            msg("user", 10, 0),
            msg("assistant", 10, 0),
            msg("user", 1_200, 0),
        ];
        // Split after assistant (index 1) → compact first 2.
        assert_eq!(
            compute_compact_count(&messages, &default_config(1_000), false),
            2
        );
    }

    #[test]
    fn keeps_consecutive_oversized_trailing_users_as_recent() {
        let messages = vec![
            msg("user", 10, 0),
            msg("assistant", 10, 0),
            msg("user", 1_200, 0),
            msg("user", 1_200, 0),
        ];
        assert_eq!(
            compute_compact_count(&messages, &default_config(1_000), false),
            2
        );
    }

    #[test]
    fn compacts_prefix_when_trailing_exchange_is_oversized() {
        // user, assistant, user, assistant(oversized)
        let messages = vec![
            msg("user", 10, 0),
            msg("assistant", 10, 0),
            msg("user", 10, 0),
            msg("assistant", 1_200, 0),
        ];
        assert_eq!(
            compute_compact_count(&messages, &default_config(1_000), false),
            2
        );
    }

    #[test]
    fn returns_zero_when_nothing_to_compact() {
        assert_eq!(compute_compact_count(&[], &default_config(1_000), false), 0);
        let single = vec![msg("user", 10, 0)];
        assert_eq!(
            compute_compact_count(&single, &default_config(1_000), false),
            0
        );
        let all_users = vec![msg("user", 10, 0), msg("user", 10, 0), msg("user", 10, 0)];
        assert_eq!(
            compute_compact_count(&all_users, &default_config(1_000), false),
            0
        );
    }

    #[test]
    fn returns_zero_when_last_message_is_unsplittable() {
        // user, assistant with pending tool call
        let messages = vec![msg("user", 10, 0), msg("assistant", 10, 1)];
        assert_eq!(
            compute_compact_count(&messages, &default_config(1_000), false),
            0
        );
    }

    #[test]
    fn does_not_split_inside_parallel_tool_exchange() {
        // user, assistant, user, assistant(2 calls), tool_a, tool_b, user
        let messages = vec![
            msg("user", 10, 0),
            msg("assistant", 10, 0),
            msg("user", 10, 0),
            msg("assistant", 10, 2),
            msg("tool", 10, 0),
            msg("tool", 10, 0),
            msg("user", 10, 0),
        ];
        // Valid split: after the first "old assistant" (index 1) → compact 2.
        // Splitting after tool_a (index 4) would orphan tool_b.
        assert_eq!(
            compute_compact_count(&messages, &default_config(1_000), false),
            2
        );
    }

    #[test]
    fn shrinks_auto_compaction_to_fit_window() {
        // 30 assistant messages, each 100 tokens (oversized for max_size=1000).
        let messages: Vec<_> = (0..30).map(|_| msg("assistant", 100, 0)).collect();
        let count = compute_compact_count(&messages, &default_config(1_000), false);
        assert!(count > 0, "expected positive count, got {count}");
        assert!(count < 30, "expected shrink, got {count}");
        let total: u32 = messages[..count as usize].iter().map(|m| m.tokens).sum();
        assert!(total <= 1_000, "compacted prefix exceeds window: {total}");
        let next_total: u32 = messages[..count as usize + 1]
            .iter()
            .map(|m| m.tokens)
            .sum();
        assert!(next_total > 1_000, "could have compacted one more");
    }

    // ---- compute_compact_count: manual path ----

    #[test]
    fn shrinks_manual_compaction_to_fit_window() {
        let messages: Vec<_> = (0..30).map(|_| msg("assistant", 100, 0)).collect();
        let count = compute_compact_count(&messages, &default_config(1_000), true);
        assert!(count > 0, "expected positive count, got {count}");
        assert!(count < 30, "expected shrink, got {count}");
        let total: u32 = messages[..count as usize].iter().map(|m| m.tokens).sum();
        assert!(total <= 1_000, "compacted prefix exceeds window: {total}");
        let next_total: u32 = messages[..count as usize + 1]
            .iter()
            .map(|m| m.tokens)
            .sum();
        assert!(next_total > 1_000, "could have compacted one more");
    }

    // ---- reduce_compact_on_overflow ----

    #[test]
    fn reduce_compact_finds_overflow_split() {
        // 5 messages, each 100 tokens. max_size=1000, ratio=0.05 → min_reduced=50.
        // Walking backward from i=3: reduced_size=messages[4].tokens=100 >= 50 → return i+1=4.
        let messages: Vec<_> = (0..5).map(|_| msg("assistant", 100, 0)).collect();
        let count = reduce_compact_on_overflow(&messages, &default_config(1_000));
        assert_eq!(count, 4);
    }

    #[test]
    fn reduce_compact_returns_full_length_when_no_split() {
        // All user messages → no valid split → return n.
        let messages = vec![
            msg("user", 100, 0),
            msg("user", 100, 0),
            msg("user", 100, 0),
            msg("user", 100, 0),
        ];
        let count = reduce_compact_on_overflow(&messages, &default_config(1_000));
        assert_eq!(count, 4);
    }

    #[test]
    fn reduce_compact_handles_tiny_arrays() {
        assert_eq!(reduce_compact_on_overflow(&[], &default_config(1_000)), 0);
        let one = vec![msg("user", 10, 0)];
        assert_eq!(reduce_compact_on_overflow(&one, &default_config(1_000)), 1);
        let two = vec![msg("assistant", 10, 0), msg("user", 10, 0)];
        assert_eq!(reduce_compact_on_overflow(&two, &default_config(1_000)), 2);
    }

    // ---- canSplitAfter rejection cases ----

    #[test]
    fn can_split_after_rejects_user_message() {
        let messages = vec![msg("user", 10, 0), msg("assistant", 10, 0)];
        assert!(!can_split_after(&messages, 0));
    }

    #[test]
    fn can_split_after_rejects_assistant_with_tool_calls() {
        let messages = vec![msg("assistant", 10, 1), msg("user", 10, 0)];
        assert!(!can_split_after(&messages, 0));
    }

    #[test]
    fn can_split_after_rejects_trailing_tool_result() {
        // assistant (no tool calls), then tool result — splitting would orphan the tool.
        let messages = vec![
            msg("assistant", 10, 0),
            msg("tool", 10, 0),
            msg("user", 10, 0),
        ];
        assert!(!can_split_after(&messages, 0));
    }

    #[test]
    fn can_split_after_rejects_open_tool_exchange_in_prefix() {
        // assistant(2 calls), tool_a, [split here], tool_b
        // Splitting after tool_a would leave tool_b orphaned in the tail
        // because the prefix ends with an open exchange (1 result < 2 calls).
        let messages = vec![
            msg("assistant", 10, 2),
            msg("tool", 10, 0),
            msg("tool", 10, 0),
        ];
        assert!(!can_split_after(&messages, 1));
    }

    #[test]
    fn can_split_after_accepts_safe_split() {
        // assistant (no tool calls), then user — safe to split.
        let messages = vec![msg("assistant", 10, 0), msg("user", 10, 0)];
        assert!(can_split_after(&messages, 0));
    }

    #[test]
    fn can_split_after_returns_false_for_out_of_bounds() {
        let messages = vec![msg("user", 10, 0)];
        assert!(!can_split_after(&messages, 5));
    }

    // ---- prefix_ends_with_open_tool_exchange ----

    #[test]
    fn open_exchange_detected_when_results_fewer_than_calls() {
        // assistant(2 calls), tool_a — 1 result < 2 calls → open.
        let messages = vec![msg("assistant", 10, 2), msg("tool", 10, 0)];
        assert!(prefix_ends_with_open_tool_exchange(&messages, 1));
    }

    #[test]
    fn open_exchange_not_detected_when_results_match_calls() {
        // assistant(1 call), tool_a — 1 result == 1 call → resolved.
        let messages = vec![msg("assistant", 10, 1), msg("tool", 10, 0)];
        assert!(!prefix_ends_with_open_tool_exchange(&messages, 1));
    }

    #[test]
    fn open_exchange_not_detected_for_non_tool_prefix() {
        let messages = vec![msg("assistant", 10, 0), msg("user", 10, 0)];
        assert!(!prefix_ends_with_open_tool_exchange(&messages, 1));
    }

    // ── Helpers for handoff tests ────────────────────────────────────────

    fn handoff_msg(role: &str, text: &str, tokens: u32) -> HandoffMessageMeta {
        HandoffMessageMeta {
            role: role.to_string(),
            text: text.to_string(),
            tokens,
        }
    }

    // ---- select_compaction_user_messages --------------------------------

    #[test]
    fn handoff_all_fit_in_budget() {
        let messages = vec![
            handoff_msg("user", "hello", 10),
            handoff_msg("user", "world", 10),
        ];
        let result = select_compaction_user_messages(&messages, 100, 20);
        assert!(!result.elided);
        assert_eq!(result.omitted_tokens, 0);
        assert_eq!(result.head_indices, Vec::<u32>::new());
        assert_eq!(result.tail_indices, vec![0, 1]);
    }

    #[test]
    fn handoff_splits_head_tail() {
        // 3 messages, 30 tokens each (90 total). max=12, head=4.
        // tail_budget=8 fits 0 whole; head_budget=4 fits 0 whole.
        // Middle message (index 1) is fully dropped → omitted_tokens > 0.
        let messages = vec![
            handoff_msg("user", &"a".repeat(120), 30),
            handoff_msg("user", &"b".repeat(120), 30),
            handoff_msg("user", &"c".repeat(120), 30),
        ];
        let result = select_compaction_user_messages(&messages, 12, 4);
        assert!(result.elided);
        assert!(result.omitted_tokens > 0);
        // Newest in tail, oldest in head.
        assert!(result.tail_indices.contains(&2));
        assert!(result.head_indices.contains(&0));
    }

    #[test]
    fn handoff_empty_input() {
        let result = select_compaction_user_messages(&[], 100, 20);
        assert!(!result.elided);
        assert_eq!(result.omitted_tokens, 0);
    }

    #[test]
    fn handoff_single_message() {
        let messages = vec![handoff_msg("user", "only", 10)];
        let result = select_compaction_user_messages(&messages, 100, 20);
        assert!(!result.elided);
        assert_eq!(result.tail_indices, vec![0]);
    }

    #[test]
    fn handoff_head_tail_overlap_at_boundary() {
        // A single boundary message truncated in the tail (its END kept)
        // whose dropped BEGINNING is also kept by the head pass. The same
        // index then appears in BOTH head_indices and tail_indices — the
        // kept-token accounting must not double-count it.
        //
        // msg0: 10 tokens, msg1: 100 tokens. max=60, head=20.
        //   head_budget = 20, tail_budget = 40.
        //   Tail: msg1 (100) > 40 -> truncate keeping END -> head_end=1.
        //   boundary_begin = 1 (dropped BEGINNING of msg1 is a head candidate).
        //   Head: msg0 (10) fits, then msg1 (100) > 10 -> truncate keeping
        //         BEGINNING -> head_indices = [0, 1].
        //   tail_indices = [1]. Overlap at index 1.
        let messages = vec![
            handoff_msg("user", &"y".repeat(40), 10),
            handoff_msg("user", &"x".repeat(400), 100),
        ];
        let result = select_compaction_user_messages(&messages, 60, 20);
        assert!(result.elided);
        assert!(result.head_indices.contains(&1));
        assert!(result.tail_indices.contains(&1));
        assert!(result.head_truncate_chars.is_some());
        assert!(result.tail_truncate_chars.is_some());
        // The overlapping index is counted once in kept_tokens.
        assert!(result.omitted_tokens <= 100);
    }
}
