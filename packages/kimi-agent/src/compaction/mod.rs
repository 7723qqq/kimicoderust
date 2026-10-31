//! Turn-internal context compaction.
//!
//! When the message history approaches the model's context window, the
//! oldest messages are replaced by a summary placeholder so the turn can
//! continue instead of failing on a context overflow. Mirrors the
//! windowing strategy of
//! `packages/agent-core-v2/src/agent/fullCompaction/strategy.ts` (and its
//! native twin `packages/kimi-native-tools/src/compaction.rs`): the system
//! prompt is always preserved, the oldest messages are dropped up to a
//! split point that cannot orphan a tool exchange, and the most recent
//! tail is kept verbatim.

pub mod micro;

use crate::turn_loop::retry::{RetryConfig, retry_delay};
use crate::turn_loop::turn_step::{retry_after_hint, step_delay};
use crate::turn_loop::types::{ContentBlock, LLM, LLMChatParams, LLMMessage};
use tokio_util::sync::CancellationToken;

/// Default context window (tokens) assumed when the engine has no model
/// capability data. Mirrors `DEFAULT_COMPACTION_MAX_COMPLETION_TOKENS` in
/// `packages/kimi-native-tools/src/compaction.rs`.
pub const DEFAULT_MAX_CONTEXT_TOKENS: u32 = 128 * 1024;

/// Total requests one compaction round may issue when the host configures no
/// cap (v2 #3750 `MAX_COMPACTION_RETRY_ATTEMPTS`). The fork takes upstream's
/// 5 rather than the step-retry default of 10: a summarizer that failed five
/// times is not going to succeed on the tenth, and every attempt re-sends the
/// whole omitted prefix.
pub const DEFAULT_COMPACTION_MAX_ATTEMPTS: u32 = 5;

/// Token accounting for the summary request's own scaffolding (v2
/// `requestTokens([])`), subtracted from the pre-shrink budget.
const SUMMARY_REQUEST_TOKENS: u32 = 1_000;

/// v2 `OVERFLOW_CONTEXT_SAFETY_RATIO` (fullCompactionService.ts:78): leave
/// headroom under the window so the token estimate's own error cannot tip the
/// request back over it.
const OVERFLOW_CONTEXT_SAFETY_RATIO: f64 = 0.85;

/// v2 `DEFAULT_COMPACTION_CONFIG.maxOverflowCompactionAttempts`
/// (strategy.ts:23): one turn may compact-and-retry after a context overflow
/// this many times in a row before the turn is failed. v2's companion
/// `maxCompactionPerTurn` is `Infinity` by default and has no production setter,
/// so this is the only overflow brake that actually fires.
pub const DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS: u32 = 3;

/// Knobs for the compaction algorithm, mirroring `DEFAULT_COMPACTION_CONFIG`
/// in `packages/agent-core-v2/src/agent/fullCompaction/strategy.ts`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionConfig {
    /// Context window in tokens; compaction triggers once the estimated
    /// history reaches `trigger_ratio * max_context_tokens` or leaves less
    /// than `reserved_context_size` tokens of headroom.
    pub max_context_tokens: u32,
    /// Fraction of the window that triggers compaction.
    pub trigger_ratio: f64,
    /// Headroom (tokens) to keep free below the window; compaction also
    /// triggers when `used + reserved >= max`.
    pub reserved_context_size: u32,
    /// How many trailing messages to keep verbatim.
    pub max_recent_messages: u32,
    /// How many trailing user messages to keep verbatim (`u32::MAX` =
    /// unlimited, mirroring the TS `Infinity` default).
    pub max_recent_user_messages: u32,
    /// Fraction of the window the recent tail may occupy.
    pub max_recent_size_ratio: f64,
    /// Total requests one compaction round may issue (v2 #3750
    /// `loopControl.compactionMaxAttempts`); `None` keeps
    /// [`DEFAULT_COMPACTION_MAX_ATTEMPTS`].
    pub max_attempts: Option<u32>,
    /// How many times in a row one turn may recover from a context overflow by
    /// compacting and retrying the step (v2 `maxOverflowCompactionAttempts`,
    /// `strategy.ts:23`). The counter resets as soon as a step completes, so
    /// this bounds only the case the recovery cannot fix: a context that stays
    /// above the window no matter how it is shrunk.
    pub max_overflow_compaction_attempts: u32,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_context_tokens: DEFAULT_MAX_CONTEXT_TOKENS,
            trigger_ratio: 0.85,
            reserved_context_size: 50_000,
            max_recent_messages: 4,
            max_recent_user_messages: u32::MAX,
            max_recent_size_ratio: 0.2,
            max_attempts: None,
            max_overflow_compaction_attempts: DEFAULT_MAX_OVERFLOW_COMPACTION_ATTEMPTS,
        }
    }
}

/// Config for the window the host resolved for the active model. Every other
/// knob stays at its default; a missing or non-positive window falls back to
/// [`DEFAULT_MAX_CONTEXT_TOKENS`].
pub fn config_for_window(max_context_tokens: Option<u32>) -> CompactionConfig {
    match max_context_tokens {
        Some(tokens) if tokens > 0 => CompactionConfig {
            max_context_tokens: tokens,
            ..CompactionConfig::default()
        },
        _ => CompactionConfig::default(),
    }
}

/// Character-based token-count estimates for messages, tools, and content parts,
/// mirroring `packages/kosong/src/tokens.ts` (`tsEstimateTokens`).
/// ASCII ≈ 4 chars/token, non-ASCII (CJK/Unicode) ≈ 1 token/char.
pub fn estimate_tokens(text: &str) -> u32 {
    let mut ascii_count = 0usize;
    let mut non_ascii_count = 0usize;
    for ch in text.chars() {
        if (ch as u32) <= 127 {
            ascii_count += 1;
        } else {
            non_ascii_count += 1;
        }
    }
    (ascii_count.div_ceil(4) + non_ascii_count) as u32
}

/// Estimate tokens for JSON-serialized content. The multiplier compensates
/// for the heuristic's under-counting of JSON's dense punctuation (matching kosong `JSON_TOKEN_MULTIPLIER = 1.3`).
pub fn estimate_tokens_for_json(text: &str) -> u32 {
    ((estimate_tokens(text) as f64) * 1.3).ceil() as u32
}

/// Flat token cost assigned to media parts, mirroring kosong `MEDIA_TOKEN_ESTIMATE = 2000`.
pub const MEDIA_TOKEN_ESTIMATE: u32 = 2000;

/// Rough token estimate for a single message: text content, multimodal
/// blocks, tool call names/arguments, and the tool call id.
pub fn estimate_message_tokens(message: &LLMMessage) -> u32 {
    let mut tokens = estimate_tokens(&message.content);
    for block in &message.blocks {
        tokens += match block {
            ContentBlock::Text { text } => estimate_tokens(text),
            ContentBlock::Image { .. }
            | ContentBlock::MediaRef { .. }
            | ContentBlock::ImageUrl { .. }
            | ContentBlock::AudioUrl { .. }
            | ContentBlock::VideoUrl { .. } => MEDIA_TOKEN_ESTIMATE,
            ContentBlock::Think { think, .. } => estimate_tokens(think),
        };
    }
    for call in &message.tool_calls {
        tokens += estimate_tokens(&call.name);
        tokens += estimate_tokens_for_json(&call.arguments.to_string());
    }
    if let Some(id) = &message.tool_call_id {
        tokens += estimate_tokens(id);
    }
    tokens
}

/// Total estimated tokens of a message list.
pub fn estimate_messages_tokens(messages: &[LLMMessage]) -> u32 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Whether the estimated history size should trigger compaction. Mirrors
/// `shouldCompact` in strategy.ts, including the reserved-context rule.
pub fn should_compact(used_size: u32, config: &CompactionConfig) -> bool {
    if config.max_context_tokens == 0 {
        return false;
    }
    let max_size = config.max_context_tokens as f64;
    used_size as f64 >= max_size * config.trigger_ratio
        || (config.reserved_context_size > 0
            && config.reserved_context_size < config.max_context_tokens
            && used_size.saturating_add(config.reserved_context_size) >= config.max_context_tokens)
}

/// [`should_compact`] *and* a safe split point exists — what the turn loop
/// must check **before** it announces `compaction.started`.
///
/// Crossing the threshold only says the history is too long; it says nothing
/// about whether [`compute_compact_count`] can find anywhere to cut, and a
/// count of 0 makes [`force_compact_messages_with_summary_report`] a no-op
/// that still reports `tokensAfter == tokensBefore` with an empty summary.
/// Announced from the turn loop that reads as a `compaction.completed`
/// success that changed nothing — and because the trigger is unchanged, the
/// next step would announce it again.
///
/// The empty case is not exotic: the compacted head is a run of user-shaped
/// messages (kept user input + elision + summary + continuation) and
/// [`can_split_after`] refuses to cut after a user message, so a second
/// compaction arriving before the model has written another assistant turn
/// has nowhere to split.
pub fn should_compact_auto(
    messages: &[LLMMessage],
    used_size: u32,
    config: &CompactionConfig,
) -> bool {
    should_compact(used_size, config) && compute_compact_count(messages, config) > 0
}

/// Compact `messages`, replacing the oldest non-system messages with a
/// summary placeholder.
///
/// Returns a new list where `messages[1..count]` is replaced by a summary
/// placeholder and `messages[count..]` is preserved verbatim. The system
/// prompt (index 0) is never touched. When the estimated history is below
/// the trigger threshold, or no safe split point exists, the input is
/// returned unchanged.
pub fn compact_messages(messages: &[LLMMessage], config: &CompactionConfig) -> Vec<LLMMessage> {
    if !should_compact(estimate_messages_tokens(messages), config) {
        return messages.to_vec();
    }
    force_compact_messages(messages, config)
}

/// Unconditionally compact `messages` if a safe split point exists, bypassing the
/// `should_compact` threshold estimate. Used for runtime context overflow recovery.
pub fn force_compact_messages(
    messages: &[LLMMessage],
    config: &CompactionConfig,
) -> Vec<LLMMessage> {
    apply_compaction(messages, compute_compact_count(messages, config))
}

/// Manual compaction (`POST :compact`): the count comes from the tail scan
/// below instead of the auto window, so it compacts far more of the history
/// (v2 `computeCompactCount(..., 'manual')`, strategy.ts:182-189).
pub fn force_compact_messages_manual(
    messages: &[LLMMessage],
    config: &CompactionConfig,
) -> Vec<LLMMessage> {
    apply_compaction(messages, compute_compact_count_manual(messages, config))
}

/// Project `count` leading messages into a summary placeholder, keeping the
/// system message (index 0) and the tail untouched.
fn apply_compaction(messages: &[LLMMessage], count: u32) -> Vec<LLMMessage> {
    if count == 0 {
        return messages.to_vec();
    }
    let mut compacted = Vec::with_capacity(messages.len() - count as usize + 2);
    compacted.push(messages[0].clone());
    compacted.push(LLMMessage {
        role: "user".into(),
        content: summary_placeholder(count as usize - 1),
        ..Default::default()
    });
    compacted.extend_from_slice(&messages[count as usize..]);
    compacted
}

/// Manual split-point search: walk from the tail and take the *deepest* safe
/// split, so a manual compaction keeps only the smallest safe tail
/// (v2 `computeCompactCount` manual branch).
pub fn compute_compact_count_manual(messages: &[LLMMessage], config: &CompactionConfig) -> u32 {
    let n = messages.len();
    if n <= 1 {
        return 0;
    }
    for index in (1..n).rev() {
        if can_split_after(messages, index) {
            let count = fit_compact_count_to_window(messages, (index + 1) as u32, config);
            // A count of 1 would mean compacting only the header (index 0),
            // which is never allowed — the same floor the auto branch
            // applies. Without it the "compaction" would insert a summary
            // without removing anything.
            return if count <= 1 { 0 } else { count };
        }
    }
    0
}

/// Status codes whose overflow wording v2 trusts. `isContextOverflowStatusError`
/// (human/llm/errors.ts:279-283) refuses every other code before it looks at the
/// message, so a 500 whose body happens to contain `max_tokens` is a server
/// fault rather than an overflow.
const OVERFLOW_KEYWORD_STATUSES: [u16; 3] = [400, 413, 422];

/// How full the window must already be before a bare 413 is still read as an
/// overflow. v2 `OVERFLOW_STATUS_RECOVERY_RATIO` (fullCompactionService.ts:79).
const OVERFLOW_STATUS_RECOVERY_RATIO: f64 = 0.5;

/// Observed effective windows per model (v2 `observedMaxContextTokensByModel`,
/// fullCompactionService.ts:215). The fork's window is host-resolved per turn;
/// this in-process cache lowers it after an observed overflow so the next
/// request — and the auto-compaction trigger — use the conservative value
/// instead of re-overflowing every turn. v2 persists the map through
/// `defineState`; the fork's engine is per-process, so an observation is
/// relearned after a restart (recorded difference). Keyed by model name: the
/// real window is a property of the provider/model, not of the session.
static OBSERVED_MAX_TOKENS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::OnceLock::new();

fn observed_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    OBSERVED_MAX_TOKENS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// v2 `observeContextOverflow` (fullCompactionService.ts:320-331): after an
/// observed overflow, remember `max(1, floor(estimated × 0.85))` as the
/// model's effective window — only ever lowering it, and never below what
/// the host already configured.
pub fn observe_context_overflow(
    model: &str,
    estimated_request_tokens: u32,
    configured: Option<u32>,
) {
    if model.is_empty() || estimated_request_tokens == 0 {
        return;
    }
    let observed = ((f64::from(estimated_request_tokens)) * OVERFLOW_CONTEXT_SAFETY_RATIO)
        .floor()
        .max(1.0) as u64;
    let current = effective_max_tokens(model, configured).unwrap_or(0);
    if current > 0 && observed >= u64::from(current) {
        return;
    }
    observed_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(model.to_string(), observed);
}

/// v2 `getEffectiveMaxContextTokens` (fullCompactionService.ts:258-267):
/// `min(configured, observed)`; the configured value stands when nothing was
/// observed, and a lone observation (no configured window) stands on its own.
pub fn effective_max_tokens(model: &str, configured: Option<u32>) -> Option<u32> {
    let observed = observed_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(model)
        .copied();
    match (configured, observed) {
        (Some(configured), Some(observed)) => Some(configured.min(observed as u32)),
        (Some(configured), None) => Some(configured),
        (None, Some(observed)) => Some(observed as u32),
        (None, None) => None,
    }
}

/// Classify whether an LLM error string indicates that the context length / window was exceeded.
///
/// Mirrors `CONTEXT_OVERFLOW_MESSAGE_PATTERNS` in `agent-core-v2` / `kosong`,
/// behind the status gate v2 puts in front of it (`isContextOverflowStatusError`):
/// the wording table is consulted only for a 400/413/422, or for an error that
/// carries no status at all — an in-stream provider error such as OpenAI's
/// `context_length_exceeded`, which v2 classifies by code and stamps with a
/// synthetic 400 (`errorFromOpenAIResponsesEvent`). Any other status is
/// rejected outright.
pub fn is_context_overflow_error(error: &str) -> bool {
    if let Some(code) = crate::llm::http::llm_http_status(error)
        && !OVERFLOW_KEYWORD_STATUSES.contains(&code)
    {
        return false;
    }
    let lower = error.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("context_length")
        || lower.contains("context length")
        || lower.contains("context window")
        || lower.contains("maximum context")
        || lower.contains("max_tokens")
        || lower.contains("model token limit")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || lower.contains("input token count")
        || lower.contains("exceeds the maximum size")
}

/// Whether a failed LLM call should trigger emergency compaction.
///
/// Mirrors v2 `shouldRecoverFromContextOverflow` (fullCompactionService.ts:305-318),
/// minus the branches the Rust engine cannot reach: v2's first branch tests a
/// coded `context.overflow` error, and there is no typed error channel here.
/// Two ways in remain —
///
/// 1. wording that names a context overflow (see
///    [`is_context_overflow_error`]); or
/// 2. an HTTP 413 whose request already filled at least
///    [`OVERFLOW_STATUS_RECOVERY_RATIO`] of the model's window. This is what
///    covers a gateway that answers 413 with an opaque body: the size, not the
///    text, is the evidence.
///
/// A 413 for a small request is a proxy rejecting it (a body limit, an auth
/// wrapper), and compacting for that would only throw context away — which is
/// why the size gate exists instead of accepting every 413. With no known
/// window there is nothing to compare against, so only branch 1 applies.
pub fn should_recover_from_context_overflow(
    error: &str,
    estimated_request_tokens: u32,
    max_context_tokens: Option<u32>,
) -> bool {
    if is_context_overflow_error(error) {
        return true;
    }
    if crate::llm::http::llm_http_status(error) != Some(413) {
        return false;
    }
    let Some(max) = max_context_tokens.filter(|max| *max > 0) else {
        return false;
    };
    let threshold = (f64::from(max) * OVERFLOW_STATUS_RECOVERY_RATIO) as u32;
    estimated_request_tokens >= threshold
}

/// Placeholder text standing in for the compacted prefix, matching the TS
/// `createCompactionSummaryMessage` marker.
///
/// Only the synchronous legacy helpers insert this marker. Summary-producing
/// APIs propagate failures without replacing history.
pub(crate) fn summary_placeholder(omitted: usize) -> String {
    format!(
        "[Earlier conversation compacted: {omitted} messages were summarized away \
         to fit the context window. Continue from the most recent context.]"
    )
}

/// The compaction summary prefix (v2 `compaction-summary-prefix.md`,
/// `COMPACTION_SUMMARY_PREFIX`). Tells the model the summary is its own
/// working notes and that earlier user messages are preserved verbatim in
/// this context — a promise the kept head/tail below fulfills.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation so far has been compacted to free up context. What follows is your own working summary of this task — use it to continue your train of thought rather than starting over. Treat it as notes, not proof: where it says a step was done, tests passed, or a fix worked, verify that yourself before relying on it. Any user messages earlier in this context are preserved verbatim from the compacted conversation; where a system-reminder note among them marks an omitted middle section, the user messages it replaced are covered by this summary. The summary records which earlier requests were already addressed.";

/// Head budget for user messages kept verbatim across compaction (v2
/// `COMPACT_USER_MESSAGE_HEAD_TOKENS`).
const COMPACT_USER_MESSAGE_HEAD_TOKENS: u32 = 2_000;
/// Tail budget for user messages kept verbatim across compaction (v2
/// `COMPACT_USER_MESSAGE_MAX_TOKENS`).
const COMPACT_USER_MESSAGE_MAX_TOKENS: u32 = 20_000;

/// The elision note inserted between the kept head and tail (v2
/// `buildCompactionElisionText`).
fn compaction_elision_text(omitted_tokens: u32) -> String {
    crate::injection::wrap_system_reminder(&format!(
        "Some of this conversation's user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and roughly {omitted_tokens} tokens in between were dropped. The omitted content is covered by the compaction summary at the end of the conversation."
    ))
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

/// The kept-verbatim user messages v2's `selectCompactionUserMessages`
/// picks from the compacted range, returned as the assembled v2 kept
/// sequence: the head fills `head_tokens` (oldest input, possibly
/// truncated — one truncating message ends the head), then the elision
/// note (when anything was omitted), then the tail fits `max_tokens` (the
/// newest input, possibly suffix-truncated at the boundary).
fn select_kept_user_messages(user_messages: &[LLMMessage]) -> Vec<LLMMessage> {
    let total_tokens: u32 = user_messages.iter().map(estimate_message_tokens).sum();
    if total_tokens <= COMPACT_USER_MESSAGE_MAX_TOKENS {
        return user_messages.to_vec();
    }

    let head_budget = COMPACT_USER_MESSAGE_HEAD_TOKENS.min(COMPACT_USER_MESSAGE_MAX_TOKENS);
    let tail_budget = COMPACT_USER_MESSAGE_MAX_TOKENS - head_budget;

    let mut tail: Vec<LLMMessage> = Vec::new();
    let mut tail_remaining = tail_budget;
    let mut head_end_exclusive = user_messages.len();
    let mut tail_boundary_prefix: Option<String> = None;
    for index in (0..user_messages.len()).rev() {
        // v2 `i >= 0 && tailRemaining > 0`: once the tail budget is spent the
        // loop stops before straddling — the next message belongs whole to
        // the head side, and no empty suffix message is emitted.
        if tail_remaining == 0 {
            break;
        }
        let message = &user_messages[index];
        let tokens = estimate_message_tokens(message);
        if tokens <= tail_remaining {
            tail.push(message.clone());
            tail_remaining -= tokens;
            head_end_exclusive = index;
            continue;
        }
        let full_text = message.content.clone();
        let kept_suffix = truncate_text_to_tokens_from_end(&full_text, tail_remaining);
        let mut kept = message.clone();
        kept.content = kept_suffix.to_string();
        tail.push(kept);
        head_end_exclusive = index;
        let dropped = &full_text[..full_text.len() - kept_suffix.len()];
        if !dropped.is_empty() {
            let mut prefix = message.clone();
            prefix.content = dropped.to_string();
            tail_boundary_prefix = Some(prefix.content);
        }
        break;
    }
    tail.reverse();

    let mut head: Vec<LLMMessage> = Vec::new();
    let mut head_remaining = head_budget;
    let head_candidates_len = head_end_exclusive + usize::from(tail_boundary_prefix.is_some());
    #[allow(clippy::needless_range_loop)] // indexes user_messages and the boundary prefix
    for index in 0..head_candidates_len {
        if head_remaining == 0 {
            break;
        }
        if tail_boundary_prefix.is_some() && index == head_end_exclusive {
            // v2 order: truncate the head-side half to the remaining budget
            // FIRST (truncateUserMessage), then spend exactly that many
            // tokens — the truncation itself must not be driven to empty by
            // a budget the message is about to consume.
            let text = tail_boundary_prefix.as_deref().unwrap_or_default();
            let kept_text = truncate_text_to_tokens(text, head_remaining);
            head_remaining = head_remaining.saturating_sub(estimate_tokens(kept_text));
            let mut head_message = user_messages[index].clone();
            head_message.content = kept_text.to_string();
            head.push(head_message);
        } else {
            let message = &user_messages[index];
            let tokens = estimate_message_tokens(message);
            if tokens <= head_remaining {
                head.push(message.clone());
                head_remaining -= tokens;
            } else {
                let mut truncated = message.clone();
                truncated.content =
                    truncate_text_to_tokens(&message.content, head_remaining).to_string();
                head.push(truncated);
                // v2: one truncated message ends the head — without this the
                // budget is never spent and every further candidate is
                // truncated and kept too.
                break;
            }
        }
    }

    let kept_tokens: u32 = head
        .iter()
        .chain(tail.iter())
        .map(estimate_message_tokens)
        .sum();
    let omitted = total_tokens.saturating_sub(kept_tokens);
    let elision = LLMMessage {
        role: "user".into(),
        content: compaction_elision_text(omitted),
        ..Default::default()
    };
    head.push(elision);
    head.extend(tail);
    head
}

/// Continuation reminder appended after context compaction, matching upstream #3537.
pub const COMPACTION_CONTINUATION_TEXT: &str = "<system-reminder>\nContext compaction is complete — continue the work that was in progress when it began.\n</system-reminder>";

/// Build the continuation message that anchors context compaction resumption.
pub fn compaction_continuation_message() -> LLMMessage {
    LLMMessage {
        role: "user".into(),
        content: COMPACTION_CONTINUATION_TEXT.into(),
        ..Default::default()
    }
}

/// v2's handoff template — `agent/fullCompaction/compaction-instruction.md`,
/// byte-identical (SHA256 `9578d8c2088f64d0b58f2ec0f10b4a0cf2a70caa9b08d46c847a8d12e6ec6545`)
/// to the copy `human/compaction/` ships. Model input, so it stays English like
/// the rest of the prompt surface. The trailing `${custom_instruction_block}` is
/// filled by [`render_compaction_instruction`].
const COMPACTION_INSTRUCTION_TEMPLATE: &str = include_str!("compaction-instruction.md");

/// v2 `renderCompactionInstruction` (`fullCompaction/compactionInstruction.ts`)
/// and `human/compaction/summarize.ts`'s `compactionInstructionText`: the
/// template always ships in full, and a caller instruction is appended *inside*
/// it rather than substituted for it. An absent or blank instruction leaves the
/// placeholder empty. Mirrors v2's `.trimEnd()` on the rendered result.
fn render_compaction_instruction(instruction: Option<&str>) -> String {
    let block = match instruction
        .map(str::trim)
        .filter(|custom| !custom.is_empty())
    {
        Some(custom) => format!("\nOptional user instruction:\n{custom}\n"),
        None => String::new(),
    };
    COMPACTION_INSTRUCTION_TEMPLATE
        .replace("${custom_instruction_block}", &block)
        .trim_end()
        .to_string()
}

/// Build the prompt messages for the summarizer LLM call.
///
/// The system message instructs the model to summarize; the user message
/// carries the omitted conversation as a flat `role: content` transcript,
/// *followed by* the handoff instruction. v2 appends that instruction as the
/// last message after the history (`fullCompactionService.ts`: `[...
/// messagesToCompact, createUserMessage(instruction)]`), which is also the
/// only position where the template's own "This message is a direct task, not
/// part of the above conversation" reads correctly. Tool calls are serialized
/// inline so the summarizer can see what was done.
fn summarization_prompt(omitted: &[LLMMessage], instruction: Option<&str>) -> Vec<LLMMessage> {
    let mut transcript = String::new();
    for m in omitted {
        if !transcript.is_empty() {
            transcript.push('\n');
        }
        transcript.push_str(&m.role);
        transcript.push_str(": ");
        transcript.push_str(&m.content);
        for call in &m.tool_calls {
            transcript.push_str(&format!(" [tool_call: {}({})]", call.name, call.arguments));
        }
    }

    let rendered = render_compaction_instruction(instruction);
    let user_content = format!("{transcript}\n\n{rendered}");

    vec![
        LLMMessage::system(
            "You are a conversation summarizer. Produce a concise, self-contained summary.",
        ),
        LLMMessage::user(user_content),
    ]
}

/// The compaction's failure modes. `Display` renders through the engine i18n
/// layer (`LocalizedText`), so a host that installed a locale sees its own
/// language — hand-written instead of `thiserror` for exactly that reason;
/// `Provider` keeps `transparent` semantics (its text and `source()` are the
/// wrapped error's).
#[derive(Debug)]
pub enum CompactionError {
    Cancelled,
    EmptySummary,
    Provider(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for CompactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(
                f,
                "{}",
                crate::i18n::LocalizedText::new("engine.compaction.cancelled").render()
            ),
            Self::EmptySummary => write!(
                f,
                "{}",
                crate::i18n::LocalizedText::new("engine.compaction.emptySummary").render()
            ),
            Self::Provider(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Provider(error) => Some(error.as_ref()),
            Self::Cancelled | Self::EmptySummary => None,
        }
    }
}

/// Generate a usable summary before any history is replaced. Empty responses
/// retry with the oldest message and leading tool results removed, as in v2.
pub async fn summarize_with_llm(
    omitted: &[LLMMessage],
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    max_attempts: Option<u32>,
) -> Result<String, CompactionError> {
    summarize_with_llm_budgeted(omitted, llm, instruction, cancel, max_attempts, None).await
}

/// [`summarize_with_llm`] with the effective model window, so the summarizer's
/// **first** request is pre-shrunk to fit instead of being sent whole.
///
/// v2 #3911 `preShrinkHistoryToWindowBudget` (fullCompactionService.ts:818-841):
/// without this, a compaction whose omitted history already exceeds the model
/// window sends the whole thing, overflows, and only then starts dropping one
/// message per empty-summary retry — so a turn that switched to a
/// smaller-window model burns the whole retry budget and fails. The budget
/// reserves room for the summary itself and applies the same safety ratio the
/// overflow path uses.
pub async fn summarize_with_llm_budgeted(
    omitted: &[LLMMessage],
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    max_attempts: Option<u32>,
    effective_max_tokens: Option<u32>,
) -> Result<String, CompactionError> {
    let pre_shrunk = pre_shrink_to_window_budget(omitted, instruction, effective_max_tokens);
    // The overflow shrink below replaces the working set, so it owns its
    // buffer; the common path borrows the caller's slice untouched.
    let mut current: std::borrow::Cow<'_, [LLMMessage]> = match pre_shrunk {
        Some(shrunken) => std::borrow::Cow::Owned(shrunken),
        None => std::borrow::Cow::Borrowed(omitted),
    };
    let retry_config = RetryConfig {
        max_attempts: max_attempts.unwrap_or(DEFAULT_COMPACTION_MAX_ATTEMPTS),
        ..RetryConfig::default()
    };
    let infinite = crate::turn_loop::retry::infinite_retry_enabled();
    let mut attempt: u32 = 0;
    let mut overflow_shrink_count: u32 = 0;
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(CompactionError::Cancelled);
        }
        attempt += 1;
        let mut history: &[LLMMessage] = &current;
        let params = LLMChatParams {
            messages: std::sync::Arc::from(summarization_prompt(history, instruction)),
            tools: std::sync::Arc::from(Vec::new()),
            cancel: cancel.cloned(),
        };
        let response = match cancel {
            Some(token) => tokio::select! {
                biased;
                _ = token.cancelled() => return Err(CompactionError::Cancelled),
                response = llm.chat(params) => response,
            },
            None => llm.chat(params).await,
        };
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(CompactionError::Cancelled);
        }
        match response {
            Ok(response) => {
                let trimmed = response.content.trim();
                if !trimmed.is_empty() {
                    return Ok(trimmed.to_string());
                }
                if attempt >= retry_config.max_attempts || history.len() <= 1 {
                    return Err(CompactionError::EmptySummary);
                }
                history = &history[1..];
                while history
                    .first()
                    .is_some_and(|message| message.role == "tool")
                {
                    history = &history[1..];
                }
                if history.is_empty() {
                    return Err(CompactionError::EmptySummary);
                }
            }
            Err(err) => {
                let err_str = err.to_string();
                if crate::llm::http::is_cancelled_error(&err_str) {
                    return Err(CompactionError::Cancelled);
                }
                // v2's overflow recovery inside the compaction request itself
                // (fullCompactionService.ts:690-710): the pre-shrink is an
                // estimate, and a provider that counts tokens differently can
                // still refuse the summarization request — shrink the history
                // by this attempt's ratio and retry instead of failing the
                // compaction.
                let prompt = summarization_prompt(&current, instruction);
                let estimated_request_tokens = prompt
                    .iter()
                    .map(estimate_message_tokens)
                    .fold(0u32, u32::saturating_add);
                if should_recover_from_context_overflow(
                    &err_str,
                    estimated_request_tokens,
                    effective_max_tokens,
                ) {
                    // v2 `observeContextOverflow` (fullCompactionService.ts:695):
                    // the summarization request that just overflowed is
                    // evidence the real window is smaller than configured —
                    // remember it so the next turn uses the conservative value.
                    observe_context_overflow(
                        llm.model_name(),
                        estimated_request_tokens,
                        effective_max_tokens,
                    );
                    overflow_shrink_count += 1;
                    if overflow_shrink_count > MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS
                        || attempt >= retry_config.max_attempts
                        || current.len() <= 1
                    {
                        return Err(CompactionError::Provider(err));
                    }
                    let ratio =
                        COMPACTION_OVERFLOW_SHRINK_RATIOS[(overflow_shrink_count - 1) as usize];
                    let total: u32 = current
                        .iter()
                        .map(estimate_message_tokens)
                        .fold(0, u32::saturating_add);
                    let shrunk =
                        take_recent_within_budget(&current, (f64::from(total) * ratio) as u32);
                    if shrunk.is_empty() {
                        return Err(CompactionError::Provider(err));
                    }
                    current = std::borrow::Cow::Owned(shrunk);
                    continue;
                }
                if !llm.is_retryable_error(&err_str)
                    || (!infinite && attempt >= retry_config.max_attempts)
                {
                    return Err(CompactionError::Provider(err));
                }
                let delay = step_delay(
                    retry_delay(attempt, &retry_config),
                    retry_after_hint(&err_str),
                );
                match cancel {
                    Some(token) => tokio::select! {
                        biased;
                        _ = token.cancelled() => return Err(CompactionError::Cancelled),
                        _ = tokio::time::sleep(delay) => {},
                    },
                    None => tokio::time::sleep(delay).await,
                }
            }
        }
    }
}

/// Shrink the summarizer's history to what the model window can actually take,
/// or `None` when it already fits.
///
/// v2 #3911 `preShrinkHistoryToWindowBudget` (fullCompactionService.ts:818-841):
/// reserve room for the summary (`compaction_max_output_size`, capped at an
/// eighth of the window), apply the safety ratio, subtract the request's own
/// tokens, and keep the most recent messages that fit. Without this a
/// compaction whose omitted history already exceeds the window sends the whole
/// thing, overflows, and only then starts dropping one message per
/// empty-summary retry — so switching to a smaller-window model burns the
/// retry budget and fails the turn.
///
/// `None` means "send what you had": no window to respect, the history already
/// fits, or nothing would survive.
/// v2 `MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS` (fullCompactionService.ts:80):
/// how many times one compaction may shrink its own history after the
/// summarization request itself overflowed.
const MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS: u32 = 3;

/// v2 `COMPACTION_OVERFLOW_SHRINK_RATIOS` (fullCompactionService.ts:81): the
/// share of the history a retry after an overflowed compaction request keeps,
/// by shrink attempt.
const COMPACTION_OVERFLOW_SHRINK_RATIOS: [f64; 3] = [0.7, 0.5, 0.35];

fn pre_shrink_to_window_budget(
    history: &[LLMMessage],
    instruction: Option<&str>,
    effective_max_tokens: Option<u32>,
) -> Option<Vec<LLMMessage>> {
    let effective_max_tokens = effective_max_tokens.filter(|tokens| *tokens > 0)?;
    let output_reserve = effective_max_tokens / 8;
    let message_budget =
        ((effective_max_tokens - output_reserve) as f64 * OVERFLOW_CONTEXT_SAFETY_RATIO) as u32;
    let message_budget = message_budget.saturating_sub(SUMMARY_REQUEST_TOKENS);
    if message_budget == 0 {
        return None;
    }

    let instruction_tokens = instruction.map(estimate_tokens).unwrap_or(0);
    let total: u32 = history
        .iter()
        .map(estimate_message_tokens)
        .fold(instruction_tokens, u32::saturating_add);
    if total <= message_budget {
        return None;
    }

    let shrunken = take_recent_within_budget(history, message_budget);
    if shrunken.is_empty() {
        None
    } else {
        Some(shrunken)
    }
}

/// Keep the newest messages that fit `budget`, then drop leading tool results —
/// a tool result without its call is not a sequence a provider accepts.
/// v2 `takeRecentMessagesWithinTokenBudget` (fullCompactionService.ts:933-950).
fn take_recent_within_budget(history: &[LLMMessage], budget: u32) -> Vec<LLMMessage> {
    let mut start = history.len();
    let mut tokens: u32 = 0;
    for (index, message) in history.iter().enumerate().rev() {
        let message_tokens = estimate_message_tokens(message);
        if tokens.saturating_add(message_tokens) > budget {
            break;
        }
        tokens = tokens.saturating_add(message_tokens);
        start = index;
    }
    // v2 `if (start === 0) start = 1;` — everything fit, so keep all but the
    // oldest. When nothing fits, `start` stays at `length` and the slice is
    // empty; `pre_shrink_to_window_budget` reads that as "send what you had"
    // rather than an empty prompt.
    if start == 0 {
        start = 1;
    }
    let mut slice = history[start..].to_vec();
    while slice.first().is_some_and(|message| message.role == "tool") {
        slice.remove(0);
    }
    slice
}

/// v2 `postProcessSummary` (fullCompactionService.ts:844-851): when the
/// session has todos, the compaction summary carries the rendered list so the
/// model's working notes keep the todo state once the history is folded away.
/// `todos` is the rendered list; `None` (an empty list) leaves the summary
/// untouched — v2 returns it untrimmed in that case too.
pub fn post_process_summary(summary: String, todos: Option<String>) -> String {
    match todos {
        None => summary,
        Some(list) => format!("{}\n\n{list}", summary.trim()),
    }
}

/// The rendered todo list a compaction summary carries: `None` when the raw
/// domain value is missing or holds no items (v2's `todos.length === 0`
/// guard). Shared by the host-bridge read and the server's own state store,
/// so both seams append the same bytes.
pub fn todo_list_for_summary(raw: Option<&serde_json::Value>) -> Option<String> {
    let todos = crate::tools::todo_item::read_todo_items(raw?);
    if todos.is_empty() {
        None
    } else {
        Some(crate::tools::todo_item::render_todo_list_with_title(
            &todos,
            "## TODO List",
        ))
    }
}

/// Read the session's todos through the host state bridge and render the
/// list [`post_process_summary`] appends. A host without the state bridge or
/// without todos degrades to `None` (a plain summary) instead of erroring
/// the compaction.
pub async fn read_todos_for_summary(
    callbacks: &dyn crate::callbacks::HostCallbacks,
) -> Option<String> {
    let response = callbacks
        .state_read(crate::rpc::types::StateReadRequest {
            domain: "todo".into(),
            key: "todo".into(),
            turn_id: String::new(),
            tool_call_id: String::new(),
        })
        .await
        .ok()?;
    todo_list_for_summary(Some(&response.value))
}

/// Unconditionally compact `messages` only after a usable LLM summary succeeds.
///
/// Like [`force_compact_messages`] but the compacted prefix is replaced by a
/// user message carrying the LLM-generated summary instead of a fixed
/// placeholder.
pub async fn force_compact_messages_with_summary(
    messages: &[LLMMessage],
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<Vec<LLMMessage>, CompactionError> {
    force_compact_messages_with_summary_budgeted(messages, config, llm, instruction, cancel, None)
        .await
}

/// [`force_compact_messages_with_summary`] with the model's effective window,
/// so the summarizer's first request is pre-shrunk to fit it (v2 #3911).
/// Callers that know the window pass it — the turn loop on both the threshold
/// and the overflow path, the REST `:compact` route, and the NAPI manual
/// compaction; the convenience wrappers keep the unbudgeted behaviour.
pub async fn force_compact_messages_with_summary_budgeted(
    messages: &[LLMMessage],
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    effective_max_tokens: Option<u32>,
) -> Result<Vec<LLMMessage>, CompactionError> {
    let tokens_before = estimate_messages_tokens(messages);
    Ok(force_compact_messages_with_summary_report(
        messages,
        config,
        llm,
        instruction,
        cancel,
        effective_max_tokens,
        tokens_before,
        None,
    )
    .await?
    .0)
}

/// What one compaction did, for the host's `compaction.completed` event (v2
/// `full_compaction.completed`). The engine folds a prefix into an LLM-written
/// summary; the host renders a transcript card from these numbers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    pub summary: String,
    pub compacted_count: u32,
    pub tokens_before: u32,
    pub tokens_after: u32,
}

/// [`force_compact_messages_with_summary_budgeted`] that also returns the
/// report the host's `compaction.completed` event carries. Additive: the
/// existing entry point delegates here and discards the report, so callers that
/// only need the messages are untouched. `tokens_before` is the caller's own
/// estimate for the pre-compaction history (the threshold path already has it;
/// the overflow-recovery path passes its own). `todos` is the rendered todo
/// list from [`read_todos_for_summary`] — v2 `postProcessSummary` appends it
/// to the written summary; `None` skips the suffix.
#[allow(clippy::too_many_arguments)]
pub async fn force_compact_messages_with_summary_report(
    messages: &[LLMMessage],
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    effective_max_tokens: Option<u32>,
    tokens_before: u32,
    todos: Option<String>,
) -> Result<(Vec<LLMMessage>, CompactionReport), CompactionError> {
    let count = compute_compact_count(messages, config);
    if count == 0 {
        return Ok((
            messages.to_vec(),
            CompactionReport {
                summary: String::new(),
                compacted_count: 0,
                tokens_before,
                tokens_after: tokens_before,
            },
        ));
    }
    let omitted = &messages[1..count as usize];
    let summary = summarize_with_llm_budgeted(
        omitted,
        llm,
        instruction,
        cancel,
        config.max_attempts,
        effective_max_tokens,
    )
    .await?;
    let summary = post_process_summary(summary, todos);
    let compacted = apply_compaction_with_summary(messages, count, summary.clone());
    let tokens_after = estimate_messages_tokens(&compacted);
    Ok((
        compacted,
        CompactionReport {
            summary,
            compacted_count: count,
            tokens_before,
            tokens_after,
        },
    ))
}

/// Manual compaction (`POST :compact`) with a real LLM summary.
///
/// Like [`force_compact_messages_manual`] but the compacted prefix is
/// replaced by the LLM-written summary (honoring the caller's `instruction`).
/// The raw summary rides back next to the compacted history (the split
/// [`force_compact_messages_with_summary_report`] makes): the summary message
/// inside the history carries the `COMPACTION_SUMMARY_PREFIX` preamble, which
/// is not what a host-facing report should show. `effective_max_tokens` (the
/// model's effective window) pre-shrinks the summarizer's first request the
/// same way the automatic paths do (v2 #3911). `todos` follows
/// [`force_compact_messages_with_summary_report`]: the rendered list v2's
/// `postProcessSummary` appends to the summary, `None` for no suffix.
pub async fn force_compact_messages_manual_with_summary(
    messages: &[LLMMessage],
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    effective_max_tokens: Option<u32>,
    todos: Option<String>,
) -> Result<(Vec<LLMMessage>, String), CompactionError> {
    let count = compute_compact_count_manual(messages, config);
    if count == 0 {
        return Ok((messages.to_vec(), String::new()));
    }
    let omitted = &messages[1..count as usize];
    let summary = summarize_with_llm_budgeted(
        omitted,
        llm,
        instruction,
        cancel,
        config.max_attempts,
        effective_max_tokens,
    )
    .await?;
    let summary = post_process_summary(summary, todos);
    let compacted = apply_compaction_with_summary(messages, count, summary.clone());
    Ok((compacted, summary))
}

/// Threshold-gated compaction with a real LLM summary.
///
/// Like [`compact_messages`] but uses [`force_compact_messages_with_summary`]
/// when the trigger fires. Returns `None` when the trigger did not fire, so
/// the common no-compaction path allocates nothing instead of cloning the
/// whole history.
pub async fn compact_messages_with_summary(
    messages: &[LLMMessage],
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<Option<Vec<LLMMessage>>, CompactionError> {
    compact_messages_with_summary_at(
        messages,
        estimate_messages_tokens(messages),
        config,
        llm,
        instruction,
        cancel,
    )
    .await
}

/// [`compact_messages_with_summary`] with a caller-supplied token estimate, so
/// an incremental estimator can pass the running total instead of forcing a
/// full rescan here.
pub async fn compact_messages_with_summary_at(
    messages: &[LLMMessage],
    used_tokens: u32,
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
) -> Result<Option<Vec<LLMMessage>>, CompactionError> {
    Ok(compact_messages_with_summary_at_report(
        messages,
        used_tokens,
        config,
        llm,
        instruction,
        cancel,
        None,
        None,
    )
    .await?
    .map(|(messages, _)| messages))
}

/// [`compact_messages_with_summary_at`] that also returns the report, so the
/// turn loop can emit `compaction.completed` with the summary and token counts.
/// `effective_max_tokens` (the model's effective window) pre-shrinks the
/// summarizer's first request to fit it (v2 #3911); pass `None` when the
/// caller does not know the window.
#[allow(clippy::too_many_arguments)]
pub async fn compact_messages_with_summary_at_report(
    messages: &[LLMMessage],
    used_tokens: u32,
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    effective_max_tokens: Option<u32>,
    todos: Option<String>,
) -> Result<Option<(Vec<LLMMessage>, CompactionReport)>, CompactionError> {
    if !should_compact(used_tokens, config) {
        return Ok(None);
    }
    Ok(Some(
        force_compact_messages_with_summary_report(
            messages,
            config,
            llm,
            instruction,
            cancel,
            effective_max_tokens,
            used_tokens,
            todos,
        )
        .await?,
    ))
}

/// Project `count` leading messages into a summary, keeping the system
/// message (index 0) and the tail untouched. Like [`apply_compaction`] but
/// uses the provided `summary` text instead of [`summary_placeholder`].
pub(crate) fn apply_compaction_with_summary(
    messages: &[LLMMessage],
    count: u32,
    summary: String,
) -> Vec<LLMMessage> {
    if count == 0 {
        return messages.to_vec();
    }
    // v2 `buildContextCompactionShape`: the compacted range's real user input
    // survives verbatim — head (2k tokens) + elision note + tail (20k tokens)
    // — then the prefixed summary and the continuation note close the block.
    // The system message (index 0) and the uncompacted tail are untouched.
    let user_in_range: Vec<LLMMessage> = messages[1..count as usize]
        .iter()
        .filter(|m| m.role == "user")
        .cloned()
        .collect();
    let kept = select_kept_user_messages(&user_in_range);
    let summary_text = format!(
        "{COMPACTION_SUMMARY_PREFIX}
{}",
        summary.trim()
    );
    let mut compacted = Vec::with_capacity(1 + kept.len() + 2 + (messages.len() - count as usize));
    compacted.push(messages[0].clone());
    compacted.extend(kept);
    compacted.push(LLMMessage {
        role: "user".into(),
        content: summary_text,
        ..Default::default()
    });
    compacted.push(compaction_continuation_message());
    compacted.extend_from_slice(&messages[count as usize..]);
    compacted
}

/// Decide how many leading messages to compact.
///
/// Returns N where `messages[0..N]` is replaced by a summary placeholder
/// and `messages[N..]` is preserved. The system message (index 0) is never
/// compacted, so N is either 0 (no compaction) or >= 2. Mirrors the auto
/// path of `computeCompactCount` in strategy.ts.
pub fn compute_compact_count(messages: &[LLMMessage], config: &CompactionConfig) -> u32 {
    let n = messages.len();
    if n <= 1 {
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
        if m.role == "user" {
            recent_user_messages += 1;
        }
        recent_size = recent_size.saturating_add(estimate_message_tokens(m));

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

    let count = fit_compact_count_to_window(messages, best_n.unwrap_or(0), config);
    // A count of 1 would mean compacting only the system prompt, which is
    // never allowed — treat it as "nothing to compact".
    if count <= 1 { 0 } else { count }
}

/// Shrink `compacted_count` so the compacted prefix fits within the
/// context window. Mirrors `fitCompactCountToWindow` in strategy.ts.
fn fit_compact_count_to_window(
    messages: &[LLMMessage],
    compacted_count: u32,
    config: &CompactionConfig,
) -> u32 {
    if config.max_context_tokens == 0 || compacted_count == 0 {
        return compacted_count;
    }
    let mut compacted_size: u32 = messages
        .iter()
        .take(compacted_count as usize)
        .map(estimate_message_tokens)
        .sum();
    if compacted_size <= config.max_context_tokens {
        return compacted_count;
    }
    let mut best_n: Option<u32> = None;
    for n in (1..compacted_count as usize).rev() {
        compacted_size = compacted_size.saturating_sub(estimate_message_tokens(&messages[n]));
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
/// `messages[index]`. Mirrors `canSplitAfter` in strategy.ts.
///
/// A split is safe only when:
///   - `messages[index]` is not a user message and not an assistant
///     message with pending tool calls (cutting either off from what
///     follows would break the conversation), AND
///   - the next message is not a tool result (its owning assistant would
///     be in the compacted prefix, orphaning the result), AND
///   - the compacted prefix itself does not end with an unresolved tool
///     exchange (pending tool results must stay in the tail).
pub fn can_split_after(messages: &[LLMMessage], index: usize) -> bool {
    let m = match messages.get(index) {
        Some(m) => m,
        None => return false,
    };
    if m.role == "user" {
        return false;
    }
    if m.role == "assistant" && !m.tool_calls.is_empty() {
        return false;
    }
    if messages.get(index + 1).is_some_and(|m| m.role == "tool") {
        return false;
    }
    if prefix_ends_with_open_tool_exchange(messages, index) {
        return false;
    }
    true
}

/// Whether the prefix `messages[0..=index]` ends with an unresolved tool
/// exchange — a trailing tool result whose owning assistant issued more
/// calls than the trailing results satisfy.
fn prefix_ends_with_open_tool_exchange(messages: &[LLMMessage], index: usize) -> bool {
    let m = match messages.get(index) {
        Some(m) => m,
        None => return false,
    };
    if m.role != "tool" {
        return false;
    }

    let mut tool_result_count = 0u32;
    for i in (0..=index).rev() {
        let msg = match messages.get(i) {
            Some(m) => m,
            None => return false,
        };
        if msg.role == "tool" {
            tool_result_count += 1;
            continue;
        }
        return msg.role == "assistant" && msg.tool_calls.len() as u32 > tool_result_count;
    }
    false
}

#[cfg(test)]
mod tests;
