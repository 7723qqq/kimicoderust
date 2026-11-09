//! The compaction window algorithm.
//!
//! One implementation backs both consumers: the turn loop passes real
//! [`LLMMessage`]s, the napi surface passes lightweight projections
//! (`CompactionMessageMeta` / `HandoffMessageMeta`) through [`CompactMessage`].
//! Keeping the split-point search and the kept-user-message selection here is
//! what stops the two paths from drifting.
//!
//! Mirrors `packages/agent-core-v2/src/agent/fullCompaction/strategy.ts` and
//! `agent/contextMemory/compactionHandoff.ts`.

use super::{CompactionConfig, estimate_message_tokens};
use crate::turn_loop::types::LLMMessage;

/// The message fields the window algorithm inspects.
///
/// The engine's own messages compute [`Self::tokens`] from their content; napi
/// callers pass a pre-computed count for the same field.
pub(crate) trait CompactMessage {
    fn role(&self) -> &str;
    fn tool_calls_count(&self) -> u32;
    fn tokens(&self) -> u32;
    fn text(&self) -> &str;
}

impl CompactMessage for LLMMessage {
    fn role(&self) -> &str {
        &self.role
    }

    fn tool_calls_count(&self) -> u32 {
        self.tool_calls.len() as u32
    }

    fn tokens(&self) -> u32 {
        estimate_message_tokens(self)
    }

    fn text(&self) -> &str {
        &self.content
    }
}

/// Decide how many leading messages to compact.
///
/// Returns N where `messages[0..N]` is compacted and `messages[N..]` is
/// preserved. 0 means no compaction possible (no valid split point).
///
/// `is_manual` selects the manual path (scan backward for the first safe
/// split); otherwise the auto path runs (grow the recent tail until a cap is
/// reached, recording the last valid split point).
///
/// Callers that keep a system prompt at index 0 must apply their own
/// `count <= 1` floor — this function only reports what the window allows.
pub(crate) fn compute_compact_count<T: CompactMessage>(
    messages: &[T],
    config: &CompactionConfig,
    is_manual: bool,
) -> u32 {
    let n = messages.len();
    if n == 0 {
        return 0;
    }

    if is_manual {
        for i in (1..n).rev() {
            if can_split_after(messages, i) {
                return fit_compact_count_to_window(messages, (i + 1) as u32, config);
            }
        }
        return 0;
    }

    let max_size = config.max_context_tokens as f64;
    let mut recent_messages = 1usize;
    let mut recent_user_messages = 0u32;
    let mut recent_size = 0u32;
    let mut best_n: Option<u32> = None;

    while recent_messages < n {
        let m_idx = n - recent_messages;
        let m = &messages[m_idx];
        if m.role() == "user" {
            recent_user_messages += 1;
        }
        recent_size = recent_size.saturating_add(m.tokens());

        let split_index = m_idx - 1;
        if can_split_after(messages, split_index) {
            best_n = Some((split_index + 1) as u32);
        }

        let reaches_max_count = (recent_messages as u32) >= config.max_recent_messages;
        let reaches_max_user = recent_user_messages >= config.max_recent_user_messages;
        let reaches_max_size = (recent_size as f64) >= max_size * config.max_recent_size_ratio;
        if (reaches_max_count || reaches_max_user || reaches_max_size) && best_n.is_some() {
            break;
        }
        recent_messages += 1;
    }

    fit_compact_count_to_window(messages, best_n.unwrap_or(0), config)
}

/// Find a split point when the LLM throws a context overflow error.
///
/// Walks backward from the tail accumulating tokens until the reduced size
/// reaches `min_overflow_reduction_ratio * max_size`, returning the first
/// valid split point that satisfies the threshold. Falls back to the last
/// valid split point found, or `messages.len()` if none.
///
/// The knob is a parameter rather than a [`CompactionConfig`] field: the
/// engine's own overflow recovery does not use this path, only the napi
/// surface does (v2 `minOverflowReductionRatio`).
pub(crate) fn reduce_compact_on_overflow<T: CompactMessage>(
    messages: &[T],
    max_size: u32,
    min_overflow_reduction_ratio: f64,
) -> u32 {
    let n = messages.len();
    if n <= 2 {
        return n as u32;
    }

    let min_reduced_size = ((max_size as f64) * min_overflow_reduction_ratio)
        .ceil()
        .max(1.0) as u32;

    let mut reduced_size = 0u32;
    let mut best_n: Option<u32> = None;

    for i in (1..n - 1).rev() {
        reduced_size = reduced_size.saturating_add(messages[i + 1].tokens());
        if can_split_after(messages, i) {
            best_n = Some((i + 1) as u32);
            if reduced_size >= min_reduced_size {
                return (i + 1) as u32;
            }
        }
    }

    best_n.unwrap_or(n as u32)
}

/// Shrink `compacted_count` so the compacted prefix fits within the window.
///
/// Walks `n` backward from `compacted_count - 1` to `1`, subtracting
/// `messages[n]`'s tokens at each step. The first `n` where
/// `can_split_after(messages, n-1)` holds and the running size fits the
/// window is returned. Falls back to the last valid split point, or the
/// original `compacted_count` if no valid split exists.
pub(super) fn fit_compact_count_to_window<T: CompactMessage>(
    messages: &[T],
    compacted_count: u32,
    config: &CompactionConfig,
) -> u32 {
    if config.max_context_tokens == 0 || compacted_count == 0 {
        return compacted_count;
    }

    let mut compacted_size: u32 = messages
        .iter()
        .take(compacted_count as usize)
        .map(CompactMessage::tokens)
        .sum();
    if compacted_size <= config.max_context_tokens {
        return compacted_count;
    }

    let mut best_n: Option<u32> = None;
    for n in (1..compacted_count as usize).rev() {
        compacted_size = compacted_size.saturating_sub(messages[n].tokens());
        if !can_split_after(messages, n - 1) {
            continue;
        }
        best_n = Some(n as u32);
        if compacted_size <= config.max_context_tokens {
            return n as u32;
        }
    }

    best_n.unwrap_or(compacted_count)
}

/// Whether a compaction split is safe to place immediately after
/// `messages[index]`.
///
/// A split is safe only when:
///   - `messages[index]` is not a user message and not an assistant
///     message with pending tool calls (cutting either off from what
///     follows would break the conversation), AND
///   - the next message is not a tool result (its owning assistant would
///     be in the compacted prefix, orphaning the result), AND
///   - the compacted prefix itself does not end with an unresolved tool
///     exchange (pending tool results must stay in the tail).
pub(crate) fn can_split_after<T: CompactMessage>(messages: &[T], index: usize) -> bool {
    let m = match messages.get(index) {
        Some(m) => m,
        None => return false,
    };
    if m.role() == "user" {
        return false;
    }
    if m.role() == "assistant" && m.tool_calls_count() > 0 {
        return false;
    }
    if messages
        .get(index + 1)
        .map(|m| m.role() == "tool")
        .unwrap_or(false)
    {
        return false;
    }
    if prefix_ends_with_open_tool_exchange(messages, index) {
        return false;
    }
    true
}

/// Whether the prefix `messages[0..=index]` ends with an unresolved
/// tool exchange — i.e. the last message is a tool result whose owning
/// assistant had more tool calls than the trailing results can satisfy.
pub(crate) fn prefix_ends_with_open_tool_exchange<T: CompactMessage>(
    messages: &[T],
    index: usize,
) -> bool {
    let m = match messages.get(index) {
        Some(m) => m,
        None => return false,
    };
    if m.role() != "tool" {
        return false;
    }

    let mut tool_result_count = 0u32;
    for i in (0..=index).rev() {
        let msg = match messages.get(i) {
            Some(m) => m,
            None => return false,
        };
        if msg.role() == "tool" {
            tool_result_count += 1;
            continue;
        }
        return msg.role() == "assistant" && msg.tool_calls_count() > tool_result_count;
    }
    false
}

/// Which user messages compaction keeps verbatim.
///
/// `head_indices` is ascending; `tail_indices` is newest-first (the order the
/// tail pass discovers them in), so the napi contract can expose it unchanged.
/// A truncation is recorded as `(index, kept bytes)`.
pub(crate) struct UserMessageSelection {
    pub head_indices: Vec<u32>,
    pub tail_indices: Vec<u32>,
    pub head_truncate: Option<(u32, u32)>,
    pub tail_truncate: Option<(u32, u32)>,
    pub elided: bool,
    pub omitted_tokens: u32,
}

/// Select user messages compaction keeps verbatim, with head/tail split.
///
/// When total tokens fit, all go to tail. Otherwise: tail takes newest messages
/// (boundary truncated keeping its END), head takes oldest (boundary truncated
/// keeping its BEGINNING).
pub(crate) fn select_user_messages<T: CompactMessage>(
    messages: &[T],
    max_tokens: u32,
    head_tokens: u32,
) -> UserMessageSelection {
    let total_tokens: u32 = messages.iter().map(CompactMessage::tokens).sum();
    if total_tokens == 0 || total_tokens <= max_tokens {
        return UserMessageSelection {
            head_indices: Vec::new(),
            tail_indices: (0..messages.len() as u32).collect(),
            head_truncate: None,
            tail_truncate: None,
            elided: false,
            omitted_tokens: 0,
        };
    }

    let head_budget = head_tokens.min(max_tokens);
    let tail_budget = max_tokens - head_budget;

    // ── Tail pass: newest → oldest ─────────────────────────────────────
    let mut tail_indices: Vec<u32> = Vec::new();
    let mut tail_remaining = tail_budget;
    let mut head_end_exclusive = messages.len();
    let mut tail_truncate: Option<(u32, u32)> = None;

    for index in (0..messages.len()).rev() {
        if tail_remaining == 0 {
            break;
        }
        let message = &messages[index];
        if message.tokens() <= tail_remaining {
            tail_remaining -= message.tokens();
            head_end_exclusive = index;
            tail_indices.push(index as u32);
            continue;
        }
        let kept_len =
            truncate_text_to_tokens_from_end(message.text(), tail_remaining).len() as u32;
        head_end_exclusive = index;
        tail_truncate = Some((index as u32, kept_len));
        tail_indices.push(index as u32);
        break;
    }

    // The dropped beginning of a truncated tail boundary re-enters as a head
    // candidate at the same index (v2 `tailBoundaryDroppedPrefix`).
    let boundary_begin: Option<u32> = tail_truncate.and_then(|(index, kept_len)| {
        let full_len = messages[index as usize].text().len() as u32;
        (full_len > kept_len).then_some(index)
    });

    // ── Head pass: oldest → newest ─────────────────────────────────────
    let mut head_indices: Vec<u32> = Vec::new();
    let mut head_remaining = head_budget;
    let mut head_truncate: Option<(u32, u32)> = None;
    let candidate_count = head_end_exclusive as u32 + boundary_begin.is_some() as u32;

    for index in 0..candidate_count {
        if head_remaining == 0 {
            break;
        }
        if boundary_begin == Some(index) {
            let kept_len = tail_truncate.map_or(0, |(_, kept_len)| kept_len) as usize;
            let full = messages[index as usize].text();
            let dropped = &full[..full.len() - kept_len];
            let kept = truncate_text_to_tokens(dropped, head_remaining).len() as u32;
            head_indices.push(index);
            head_truncate = Some((index, kept));
            break;
        }
        let message = &messages[index as usize];
        if message.tokens() <= head_remaining {
            head_remaining -= message.tokens();
            head_indices.push(index);
        } else {
            let kept = truncate_text_to_tokens(message.text(), head_remaining).len() as u32;
            head_indices.push(index);
            head_truncate = Some((index, kept));
            break;
        }
    }

    head_indices.sort_unstable();
    head_indices.dedup();

    let mut kept_tokens: u32 = 0;
    for &index in &head_indices {
        kept_tokens += messages[index as usize].tokens();
    }
    for &index in &tail_indices {
        if !head_indices.contains(&index) {
            kept_tokens += messages[index as usize].tokens();
        }
    }

    UserMessageSelection {
        head_indices,
        tail_indices,
        head_truncate,
        tail_truncate,
        elided: true,
        omitted_tokens: total_tokens.saturating_sub(kept_tokens),
    }
}

/// Truncate text to at most `max_tokens` estimated tokens, keeping the END
/// (v2 `truncateTextToTokensFromEnd`).
fn truncate_text_to_tokens_from_end(text: &str, max_tokens: u32) -> &str {
    if max_tokens == 0 {
        return "";
    }
    let mut ascii_count: u32 = 0;
    let mut non_ascii_count: u32 = 0;
    let mut start = text.len();
    for (byte_offset, ch) in text.char_indices().rev() {
        if ch.is_ascii() {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
        if ascii_count.div_ceil(4) + non_ascii_count > max_tokens {
            break;
        }
        start = byte_offset;
    }
    &text[start..]
}

/// Truncate text to at most `max_tokens` estimated tokens, keeping the HEAD
/// (v2 `truncateTextToTokens`).
fn truncate_text_to_tokens(text: &str, max_tokens: u32) -> &str {
    if max_tokens == 0 {
        return "";
    }
    let mut ascii_count: u32 = 0;
    let mut non_ascii_count: u32 = 0;
    let mut end = text.len();
    for (byte_offset, ch) in text.char_indices() {
        if ch.is_ascii() {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
        if ascii_count.div_ceil(4) + non_ascii_count > max_tokens {
            break;
        }
        end = byte_offset + ch.len_utf8();
    }
    &text[..end]
}
