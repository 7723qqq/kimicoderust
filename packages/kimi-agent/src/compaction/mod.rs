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
pub(crate) mod window;

use crate::rpc::types::TokenUsage;
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

/// v2 `COMPLETION_BUDGET_ESTIMATE_MARGIN_RATIO` (fullCompactionService.ts:79):
/// the estimate's error margin added on top of a compaction request's token
/// count when it is reported as `used_context_tokens`, so the provider's
/// completion-budget clamp works with the same conservative window.
const COMPLETION_BUDGET_ESTIMATE_MARGIN_RATIO: f64 = 0.02;

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

/// Estimate tokens for JSON-serialized content. v2 estimates
/// `JSON.stringify(...)` with the same character heuristic as plain text —
/// `estimateTokensForMessage` for tool arguments and
/// `estimateToolParametersTokens` for schemas — so there is no multiplier.
pub fn estimate_tokens_for_json(text: &str) -> u32 {
    estimate_tokens(text)
}

/// Flat token cost assigned to media parts, mirroring kosong `MEDIA_TOKEN_ESTIMATE = 2000`.
pub const MEDIA_TOKEN_ESTIMATE: u32 = 2000;

/// Rough token estimate for a single message: role, text content, multimodal
/// blocks, tool call names/arguments, and the tool call id (v2
/// `estimateTokensForMessage`).
pub fn estimate_message_tokens(message: &LLMMessage) -> u32 {
    let mut tokens = estimate_tokens(&message.role) + estimate_tokens(&message.content);
    for block in &message.blocks {
        tokens += match block {
            ContentBlock::Text { text, .. } => estimate_tokens(text),
            ContentBlock::Image { .. }
            | ContentBlock::MediaRef { .. }
            | ContentBlock::ImageUrl { .. }
            | ContentBlock::AudioUrl { .. }
            | ContentBlock::VideoUrl { .. } => MEDIA_TOKEN_ESTIMATE,
            // v2 keeps a details-derived summary in `think`; Rust parks it in
            // `details_summary`, so count the parked text or the estimate
            // silently drops it.
            ContentBlock::Think {
                think,
                details_summary,
                ..
            } => estimate_tokens(think) + details_summary.as_deref().map_or(0, estimate_tokens),
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

/// The post-compaction token total (v2 `buildContextCompactionShape`'s
/// `tokensAfter`, `compactionHandoff.ts:107-111`): the rebuilt history's
/// estimate with the summary message's own size replaced by the summarization
/// request's measured output tokens. Without a measurement — or without the
/// summary message in the history — the estimate stands.
pub fn rebased_tokens(history: &[LLMMessage], summary_output_tokens: Option<u32>) -> u32 {
    let estimate = estimate_messages_tokens(history);
    let Some(measured) = summary_output_tokens else {
        return estimate;
    };
    let Some(summary) = history
        .iter()
        .find(|message| is_compaction_summary_message(message))
    else {
        return estimate;
    };
    estimate
        .saturating_sub(estimate_message_tokens(summary))
        .saturating_add(measured)
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
    let count = window::compute_compact_count(messages, config, true);
    // A count of 1 would mean compacting only the header (index 0), which is
    // never allowed — the same floor the auto branch applies. Without it the
    // "compaction" would insert a summary without removing anything.
    if count <= 1 { 0 } else { count }
}

/// Status codes whose overflow wording v2 trusts. `isContextOverflowStatusError`
/// (human/llm/errors.ts:279-283) refuses every other code before it looks at the
/// message, so a 500 whose body happens to contain `max_tokens` is a server
/// fault rather than an overflow.
const OVERFLOW_KEYWORD_STATUSES: [u16; 3] = [400, 413, 422];

/// How full the window must already be before a bare 413 is still read as an
/// overflow. v2 `OVERFLOW_STATUS_RECOVERY_RATIO` (fullCompactionService.ts:79).
const OVERFLOW_STATUS_RECOVERY_RATIO: f64 = 0.5;

/// Observed effective windows per `(agent, model)` (v2
/// `observedWindows` is agent state, fullCompactionService.ts:215). The fork's
/// window is host-resolved per turn; this in-process cache lowers it after an
/// observed overflow so the next request — and the auto-compaction trigger —
/// use the conservative value instead of re-overflowing every turn. v2
/// persists the map through `defineState`; the fork's engine is per-process,
/// so an observation is relearned after a restart (recorded difference).
/// Keyed by agent *and* model: a subagent that overflows must not shrink the
/// main agent's window (nor a sibling's).
static OBSERVED_MAX_TOKENS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::OnceLock::new();

fn observed_map() -> &'static std::sync::Mutex<std::collections::HashMap<String, u64>> {
    OBSERVED_MAX_TOKENS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn observed_key(agent_id: &str, model: &str) -> String {
    format!("{agent_id}\u{1f}{model}")
}

/// v2 `observeContextOverflow` (fullCompactionService.ts:320-331): after an
/// observed overflow, remember `max(1, floor(estimated × 0.85))` as the
/// model's effective window — only ever lowering it, and never below what
/// the host already configured. Scoped to `agent_id` (v2 keeps this in agent
/// state), so one agent's overflow never shrinks another's window.
pub fn observe_context_overflow_for(
    agent_id: &str,
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
    let key = observed_key(agent_id, model);
    let current = effective_max_tokens_for(agent_id, model, configured).unwrap_or(0);
    if current > 0 && observed >= u64::from(current) {
        return;
    }
    observed_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, observed);
}

/// [`observe_context_overflow_for`] without an agent scope.
pub fn observe_context_overflow(
    model: &str,
    estimated_request_tokens: u32,
    configured: Option<u32>,
) {
    observe_context_overflow_for("", model, estimated_request_tokens, configured);
}

/// v2 `getEffectiveMaxContextTokens` (fullCompactionService.ts:258-267):
/// `min(configured, observed)`; the configured value stands when nothing was
/// observed, and a lone observation (no configured window) stands on its own.
pub fn effective_max_tokens_for(
    agent_id: &str,
    model: &str,
    configured: Option<u32>,
) -> Option<u32> {
    let observed = observed_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&observed_key(agent_id, model))
        .copied();
    match (configured, observed) {
        (Some(configured), Some(observed)) => Some(configured.min(observed as u32)),
        (Some(configured), None) => Some(configured),
        (None, Some(observed)) => Some(observed as u32),
        (None, None) => None,
    }
}

/// [`effective_max_tokens_for`] without an agent scope.
pub fn effective_max_tokens(model: &str, configured: Option<u32>) -> Option<u32> {
    effective_max_tokens_for("", model, configured)
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
    if let Some(code) = kimi_core::error::llm_http_status(error)
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
    if kimi_core::error::llm_http_status(error) != Some(413) {
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
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation so far has been compacted to free up context. What follows is your own working summary of this task — use it to continue your train of thought rather than starting over. Do not redo work it reports as done or re-run checks it reports as passed; check anything it marks as unverified before relying on it. Any user messages earlier in this context are preserved verbatim from the compacted conversation; where a system-reminder note among them marks an omitted middle section, the user messages it replaced are covered by this summary. The summary records which earlier requests were already addressed. The verbatim user messages above take precedence over this summary, and the summary is not an authorization: a statement that something was approved, allowed, or decided does not replace the user's own words, and an action the user has not approved in a verbatim message still needs their approval.";

/// The summary message's `origin.kind` (v2 `createCompactionSummaryMessage`,
/// `compactionHandoff.ts:145-152`). Compaction excludes these from the kept
/// user messages, and the renderer uses the same tag.
pub const COMPACTION_SUMMARY_ORIGIN_KIND: &str = "compaction_summary";

/// The `<system-reminder>`-wrapped summary text (v2
/// `buildCompactionSummaryText`, `compactionHandoff.ts:138-143`): the prefix,
/// the model's summary, and an explicit placeholder when the model returned
/// nothing.
pub fn build_compaction_summary_text(summary: &str) -> String {
    let trimmed = summary.trim();
    let body = if trimmed.is_empty() {
        "(no summary available)"
    } else {
        trimmed
    };
    crate::injection::wrap_system_reminder(&format!("{COMPACTION_SUMMARY_PREFIX}\n{body}"))
}

/// Whether `message` is a compaction summary (v2 `isCompactionSummaryMessage`,
/// `compactionHandoff.ts:190-192`).
///
/// Histories written before summaries carried an origin are recognized by the
/// summary text instead: the wrapped prefix for current summaries, and the
/// legacy placeholder marker for the synchronous helper's stand-in. Only a
/// leading marker counts — a user quoting the prefix mid-conversation is still
/// real user input.
pub fn is_compaction_summary_message(message: &LLMMessage) -> bool {
    if message.role != "user" {
        return false;
    }
    if message
        .origin
        .as_ref()
        .and_then(|origin| origin.get("kind"))
        .and_then(serde_json::Value::as_str)
        == Some(COMPACTION_SUMMARY_ORIGIN_KIND)
    {
        return true;
    }
    let content = message.content.trim_start();
    let body = content
        .strip_prefix(crate::injection::SYSTEM_REMINDER_PREFIX)
        .map(str::trim_start)
        .unwrap_or(content);
    body.starts_with(COMPACTION_SUMMARY_PREFIX)
        || content.starts_with("[Earlier conversation compacted:")
}

/// Whether `message` carries real user input, i.e. what compaction keeps
/// verbatim (v2 `isRealUserInput` / `compactionUserMessageDisposition`,
/// `compactionHandoff.ts:184-224`).
///
/// Injected reminders, shell/task/cron triggers and older compaction summaries
/// are all `user`-role messages but not user *intent*; keeping them would fill
/// the kept segment with reminder text and suppress the matching reminder later.
/// Messages without an origin keep v2's `keep`, except for the two shapes this
/// engine wrote before it tagged them: system-reminder injections and the
/// legacy compaction placeholder.
pub fn is_real_user_input(message: &LLMMessage) -> bool {
    if message.role != "user" {
        return false;
    }
    let Some(origin) = &message.origin else {
        return !crate::injection::is_system_reminder(&message.content)
            && !message
                .content
                .starts_with("[Earlier conversation compacted:");
    };
    match origin.get("kind").and_then(serde_json::Value::as_str) {
        Some("user") => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin.get("trigger").and_then(serde_json::Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

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

/// The kept-verbatim user messages v2's `selectCompactionUserMessages`
/// picks from the compacted range, returned as the assembled v2 kept
/// sequence: the head fills `head_tokens` (oldest input, possibly
/// truncated — one truncating message ends the head), then the elision
/// note (when anything was omitted), then the tail fits `max_tokens` (the
/// newest input, possibly suffix-truncated at the boundary).
///
/// The selection itself lives in [`window::select_user_messages`]; this only
/// assembles the chosen slices back into messages.
fn select_kept_user_messages(user_messages: &[LLMMessage]) -> Vec<LLMMessage> {
    let selection = window::select_user_messages(
        user_messages,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
        COMPACT_USER_MESSAGE_HEAD_TOKENS,
    );
    if !selection.elided {
        return user_messages.to_vec();
    }

    let head: Vec<LLMMessage> = selection
        .head_indices
        .iter()
        .map(|&index| {
            let message = &user_messages[index as usize];
            match selection.head_truncate {
                Some((truncated, kept)) if truncated == index => {
                    let mut head_message = message.clone();
                    head_message.content = message.content[..kept as usize].to_string();
                    head_message
                }
                _ => message.clone(),
            }
        })
        .collect();

    // `tail_indices` is newest-first; the assembled tail is oldest-first.
    let tail: Vec<LLMMessage> = selection
        .tail_indices
        .iter()
        .rev()
        .map(|&index| {
            let message = &user_messages[index as usize];
            match selection.tail_truncate {
                Some((truncated, kept)) if truncated == index => {
                    let mut tail_message = message.clone();
                    let start = message.content.len() - kept as usize;
                    tail_message.content = message.content[start..].to_string();
                    tail_message
                }
                _ => message.clone(),
            }
        })
        .collect();

    let total_tokens: u32 = user_messages.iter().map(estimate_message_tokens).sum();
    let kept_tokens: u32 = head
        .iter()
        .chain(tail.iter())
        .map(estimate_message_tokens)
        .sum();
    let omitted = total_tokens.saturating_sub(kept_tokens);
    let elision = LLMMessage {
        role: "user".into(),
        content: compaction_elision_text(omitted),
        // v2 `createCompactionElisionMessage` tags the note as an injection so
        // a later compaction does not mistake it for user input.
        origin: Some(serde_json::json!({
            "kind": "injection",
            "variant": "compaction_elision",
        })),
        ..Default::default()
    };
    let mut kept = head;
    kept.push(elision);
    kept.extend(tail);
    kept
}

/// Continuation reminder appended after context compaction, matching upstream #3537.
pub const COMPACTION_CONTINUATION_TEXT: &str = "<system-reminder>\nContext compaction is complete — continue the work that was in progress when it began.\n</system-reminder>";

/// Build the continuation message that anchors context compaction resumption.
pub fn compaction_continuation_message() -> LLMMessage {
    LLMMessage {
        role: "user".into(),
        content: COMPACTION_CONTINUATION_TEXT.into(),
        // v2 `createCompactionContinuationMessage` tags the note as an
        // injection; compaction's kept-user selection must not re-keep it.
        origin: Some(serde_json::json!({
            "kind": "injection",
            "variant": "compaction_continuation",
        })),
        ..Default::default()
    }
}

/// v2's handoff template — `agent/fullCompaction/compaction-instruction.md`,
/// ported verbatim: the nine-section recovery block (Current request …
/// Next steps). Model input, so it stays English like the rest of the prompt
/// surface. The trailing `${custom_instruction_block}` is filled by
/// [`render_compaction_instruction`].
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
    /// v2 `CompactionTruncatedError`: the summarizer hit its output limit
    /// before finishing, so the text it produced is not a complete summary.
    Truncated,
    Provider {
        error: Box<dyn std::error::Error + Send + Sync>,
        /// v2 `CompactionFailedEvent.retry_count`: the backoff retries spent
        /// before this failure. The compaction's own recovery paths (a
        /// truncation drop, an overflow shrink) reset it, so it counts the
        /// wait-and-retry ladder rather than every request.
        retry_count: u32,
    },
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
            Self::Truncated => write!(
                f,
                "{}",
                crate::i18n::LocalizedText::new("engine.compaction.truncated").render()
            ),
            Self::Provider { error, .. } => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for CompactionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Provider { error, .. } => Some(error.as_ref()),
            Self::Cancelled | Self::EmptySummary | Self::Truncated => None,
        }
    }
}

/// One successful summarization round: the summary plus the accounting v2's
/// `compaction_finished` telemetry reports (`fullCompactionService.ts:768-795`)
/// — the usage of the request that produced it and the backoff retries spent.
#[derive(Debug, Clone, Default)]
pub struct SummaryOutcome {
    pub summary: String,
    /// Usage of the final summarization request (`attempt.usage` in v2),
    /// `None` when the transport reported none.
    pub usage: Option<TokenUsage>,
    /// Backoff retries performed in this round (v2 `retryCount`); a recovery
    /// shrink resets it, exactly like v2.
    pub retry_count: u32,
    /// Messages dropped before the successful summarizer request (v2
    /// `generation.droppedCount`).
    pub dropped_count: u32,
    /// v2 `prompt_cache_probe` with `source: 'compaction'`: how the summarizer
    /// request's cache reads compare with the agent's last turn request. `None`
    /// only when no request completed.
    pub cache_probe: Option<crate::session::cache_observation::CacheProbeObservation>,
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
    Ok(summarize_with_llm_budgeted(
        omitted,
        llm,
        instruction,
        cancel,
        max_attempts,
        None,
        "",
        None,
        None,
    )
    .await?
    .summary)
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
    agent_id: &str,
    // v2 `llmRequesterService`'s compaction baseline: the agent's last turn
    // request, captured before the summarizer request starts.
    cache_probe_baseline: Option<&crate::session::cache_observation::CacheProbeRequest>,
    // The turn that triggered compaction (v2 `request.source.turnId`).
    cache_probe_turn_id: Option<&str>,
) -> Result<SummaryOutcome, CompactionError> {
    let pre_shrunk = pre_shrink_to_window_budget(omitted, instruction, effective_max_tokens);
    let mut dropped_count = match &pre_shrunk {
        Some(shrunken) => omitted.len().saturating_sub(shrunken.len()) as u32,
        None => 0,
    };
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
    // v2 `compactionRound`'s `retryCount`: the backoff retries performed since
    // the last recovery reset, reported on failure as `retry_count`.
    let mut retry_count: u32 = 0;
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(CompactionError::Cancelled);
        }
        attempt += 1;
        let history: &[LLMMessage] = &current;
        // v2 snips oversized tool results per attempt, before the request is
        // built: a single 5000-line log must not push the summarization
        // request over the window it is meant to relieve.
        let snipped = snip_large_tool_results(history);
        let prompt = summarization_prompt(&snipped, instruction);
        // The summarizer request as `prompt_cache_probe` records it. The
        // request has no tools and its own system prompt, so those hashes are
        // the empty-set/empty-prompt ones; the history hash is the full
        // message list, and the prefix check compares its head with the
        // last turn request (v2 `historyPrefixMatch`).
        let probe_message_count = prompt.len();
        let probe_history_hash = crate::session::cache_observation::fingerprint(
            &serde_json::to_string(&prompt).unwrap_or_default(),
        );
        let probe_history_prefix_match = cache_probe_baseline.map(|baseline| {
            crate::session::cache_observation::compaction_history_prefix_matches(&prompt, baseline)
        });
        let probe_started_at = chrono::Utc::now().timestamp_millis();
        // v2 #4456: report the estimated request size as `used_context_tokens`
        // (with the 2% estimate margin) so the transport clamps the completion
        // budget to the window actually left — the full history leaves less
        // room than the configured output cap. Both fields are needed: the
        // clamp (`llm/http.rs`) only engages on the pair.
        let estimated_request_tokens = prompt
            .iter()
            .map(estimate_message_tokens)
            .fold(SUMMARY_REQUEST_TOKENS, u32::saturating_add);
        let used_context_tokens = effective_max_tokens
            .filter(|tokens| *tokens > 0)
            .map(|window| {
                estimated_request_tokens.saturating_add(
                    (window as f64 * COMPLETION_BUDGET_ESTIMATE_MARGIN_RATIO).ceil() as u32,
                )
            });
        let params = LLMChatParams {
            messages: std::sync::Arc::from(prompt),
            tools: std::sync::Arc::from(Vec::new()),
            cancel: cancel.cloned(),
            used_context_tokens,
            max_context_tokens: effective_max_tokens,
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
                let finish = response.finish_reason.as_deref();
                // v2 `collectSummary` (`compactionUtils.ts`): a truncated
                // response is never a usable summary even when the provider
                // streamed text — it would end mid-sentence. Drop the oldest
                // message and retry; fail with the truncation error once the
                // history cannot shrink further.
                if crate::turn_loop::run_turn::provider_finish_reason(finish) == Some("truncated") {
                    if attempt >= retry_config.max_attempts || current.len() <= 1 {
                        return Err(CompactionError::Truncated);
                    }
                    let reduced = drop_oldest_and_leading_tool_results(&current);
                    if reduced.is_empty() {
                        return Err(CompactionError::Truncated);
                    }
                    // A recovery, not a backoff retry: reset the ladder.
                    retry_count = 0;
                    dropped_count =
                        dropped_count.saturating_add((current.len() - reduced.len()) as u32);
                    current = std::borrow::Cow::Owned(reduced);
                    continue;
                }
                let trimmed = response.content.trim();
                if !trimmed.is_empty() {
                    // v2 reports the usage of the attempt that succeeded
                    // (`attempt.usage`), not a sum across retries.
                    let usage = (response.usage.measured_context_tokens() > 0)
                        .then(|| response.usage.clone());
                    let own = crate::session::cache_observation::CacheProbeRequest {
                        model_alias: llm
                            .model_alias()
                            .unwrap_or_else(|| llm.model_name())
                            .to_string(),
                        tools_hash: crate::session::cache_observation::fingerprint("[]"),
                        system_prompt_hash: crate::session::cache_observation::fingerprint(""),
                        message_count: probe_message_count,
                        history_hash: probe_history_hash.clone(),
                        input_tokens: usage
                            .as_ref()
                            .map(crate::session::cache_observation::input_total),
                        started_at: probe_started_at,
                        finished_at: chrono::Utc::now().timestamp_millis(),
                    };
                    let cache_probe = Some(crate::session::cache_observation::build_cache_probe(
                        crate::session::cache_observation::CacheProbeSource::Compaction,
                        cache_probe_turn_id,
                        llm.provider_type(),
                        llm.protocol(),
                        usage.as_ref(),
                        &own,
                        cache_probe_baseline,
                        probe_history_prefix_match,
                    ));
                    return Ok(SummaryOutcome {
                        summary: trimmed.to_string(),
                        usage,
                        retry_count,
                        dropped_count,
                        cache_probe,
                    });
                }
                // v2's empty-response recovery excludes a *filtered* empty
                // answer: `APIEmptyResponseError` with `finishReason ===
                // 'filtered'` is constructed non-retryable and fails the
                // round, while an ordinary empty answer drops a message.
                if crate::turn_loop::turn_step::is_content_filtered(finish) {
                    return Err(CompactionError::EmptySummary);
                }
                if attempt >= retry_config.max_attempts || current.len() <= 1 {
                    return Err(CompactionError::EmptySummary);
                }
                // v2 resets the retry count on every recovery path: a dropped
                // message already shrinks the request, so the next attempt is
                // not a backoff retry. The drop must be persisted in
                // `current`, or the next attempt re-sends the same history.
                retry_count = 0;
                let reduced = drop_oldest_and_leading_tool_results(&current);
                if reduced.is_empty() {
                    return Err(CompactionError::EmptySummary);
                }
                dropped_count =
                    dropped_count.saturating_add((current.len() - reduced.len()) as u32);
                current = std::borrow::Cow::Owned(reduced);
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
                let prompt = summarization_prompt(&snip_large_tool_results(&current), instruction);
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
                    observe_context_overflow_for(
                        agent_id,
                        llm.model_name(),
                        estimated_request_tokens,
                        effective_max_tokens,
                    );
                    overflow_shrink_count += 1;
                    if overflow_shrink_count > MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS
                        || attempt >= retry_config.max_attempts
                        || current.len() <= 1
                    {
                        return Err(CompactionError::Provider {
                            error: err,
                            retry_count,
                        });
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
                        return Err(CompactionError::Provider {
                            error: err,
                            retry_count,
                        });
                    }
                    // A shrink is a recovery, not a backoff retry (v2 resets
                    // `retryCount` here).
                    retry_count = 0;
                    dropped_count =
                        dropped_count.saturating_add((current.len() - shrunk.len()) as u32);
                    current = std::borrow::Cow::Owned(shrunk);
                    continue;
                }
                if !llm.is_retryable_error(&err_str)
                    || (!infinite && attempt >= retry_config.max_attempts)
                {
                    return Err(CompactionError::Provider {
                        error: err,
                        retry_count,
                    });
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
                retry_count += 1;
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
    let input_limit = effective_max_tokens - output_reserve;

    let instruction_tokens = instruction.map(estimate_tokens).unwrap_or(0);
    let total: u32 = history
        .iter()
        .map(estimate_message_tokens)
        .fold(instruction_tokens, u32::saturating_add);
    // v2 #4456: gate on the real input limit (window − output reserve), not
    // the safety budget. The budget (~0.74·window) sits below the
    // auto-compaction trigger (min(0.85W, W−50k)), so gating on it dropped
    // the oldest messages of every auto compaction on windows above ~195k
    // and broke the prompt-cache prefix for nothing — the request fit.
    if total.saturating_add(SUMMARY_REQUEST_TOKENS) <= input_limit {
        return None;
    }

    // The safety budget stays the shrink *target* once the gate says the
    // request genuinely overflows (v2 keeps the same split).
    let message_budget = ((input_limit as f64) * OVERFLOW_CONTEXT_SAFETY_RATIO) as u32;
    let message_budget = message_budget.saturating_sub(SUMMARY_REQUEST_TOKENS);
    if message_budget == 0 {
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

/// v2 `dropOldestMessageAndLeadingToolResults` (`compactionUtils.ts`): drop the
/// oldest message, then any tool results that lost their call. A tool result
/// without its call is not a sequence a provider accepts.
fn drop_oldest_and_leading_tool_results(history: &[LLMMessage]) -> Vec<LLMMessage> {
    if history.len() <= 1 {
        return Vec::new();
    }
    let mut slice = history[1..].to_vec();
    while slice.first().is_some_and(|message| message.role == "tool") {
        slice.remove(0);
    }
    slice
}

/// v2 `SNIPPED_TOOL_RESULT_MARKER` (`fullCompaction/compactionUtils.ts`): the
/// prefix of an already-snipped tool result. A result that already carries it
/// is never snipped twice — the second pass would count the marker's own lines
/// as content and shorten the head/tail again.
const SNIPPED_TOOL_RESULT_MARKER: &str = "[snipped tool result — ";

/// v2 `DEFAULT_TOOL_RESULT_SNIP_MIN_BYTES`.
const DEFAULT_TOOL_RESULT_SNIP_MIN_BYTES: usize = 1024;

/// v2 `DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES`.
const DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES: usize = 40;

/// v2 `DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES`.
const DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES: usize = 40;

/// Replace a large tool result with its head and tail before it is summarized
/// (v2 `snipLargeToolResults`, `fullCompaction/compactionUtils.ts:122-173`).
///
/// The summarizer does not need the middle of a 5000-line build log to write a
/// faithful summary, and sending it is how a compaction request overflows the
/// window it was supposed to relieve. Only the first text part is rewritten (v2
/// looks up a single `type: 'text'` part); other roles pass through untouched.
fn snip_large_tool_results(messages: &[LLMMessage]) -> std::borrow::Cow<'_, [LLMMessage]> {
    let mut changed: Option<Vec<LLMMessage>> = None;
    for (index, message) in messages.iter().enumerate() {
        let content = (message.role == "tool")
            .then(|| snip_tool_result_content(&message.content))
            .flatten();
        match content {
            Some(content) => {
                if changed.is_none() {
                    changed = Some(messages[..index].to_vec());
                }
                let mut next = message.clone();
                next.content = content;
                if let Some(changed) = changed.as_mut() {
                    changed.push(next);
                }
            }
            None => {
                if let Some(changed) = changed.as_mut() {
                    changed.push(message.clone());
                }
            }
        }
    }
    match changed {
        Some(changed) => std::borrow::Cow::Owned(changed),
        None => std::borrow::Cow::Borrowed(messages),
    }
}

/// v2 `snipToolResultContent`: `None` when the text is already short enough,
/// already snipped, or has no head/tail gap to cut.
fn snip_tool_result_content(text: &str) -> Option<String> {
    if text.starts_with(SNIPPED_TOOL_RESULT_MARKER) {
        return None;
    }
    // v2 measures `text.length` (UTF-16 code units); counting chars is the
    // closest Rust equivalent and keeps multi-byte content from being cut
    // earlier than the reference implementation would cut it.
    if text.chars().count() < DEFAULT_TOOL_RESULT_SNIP_MIN_BYTES {
        return None;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES + DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES {
        return None;
    }
    let head = lines[..DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES].join("\n");
    let tail = lines[lines.len() - DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES..].join("\n");
    let omitted =
        lines.len() - DEFAULT_TOOL_RESULT_SNIP_HEAD_LINES - DEFAULT_TOOL_RESULT_SNIP_TAIL_LINES;
    Some(format!(
        "{SNIPPED_TOOL_RESULT_MARKER}{omitted} lines omitted for compaction; re-run the tool if the \
full output is needed]\n{head}\n[... {omitted} lines omitted ...]\n{tail}"
    ))
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
    force_compact_messages_with_summary_budgeted(
        messages,
        config,
        llm,
        instruction,
        cancel,
        None,
        "",
        false,
    )
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
    agent_id: &str,
    in_flight: bool,
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
        agent_id,
        in_flight,
        None,
        None,
    )
    .await?
    .0)
}

/// What one compaction did, for the host's `compaction.completed` event (v2
/// `full_compaction.completed`). The engine folds a prefix into an LLM-written
/// summary; the host renders a transcript card from these numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionReport {
    pub summary: String,
    pub compacted_count: u32,
    /// Messages the summarization round dropped before its final request
    /// (v2 `droppedCount`); `0` when nothing had to be dropped.
    pub dropped_count: u32,
    pub tokens_before: u32,
    pub tokens_after: u32,
    /// Usage of the summarization request that produced the summary (v2
    /// `compaction_finished`'s `input_tokens` / `output_tokens` / cache
    /// fields); `None` when the transport reported none.
    pub usage: Option<TokenUsage>,
    /// Backoff retries the summarization round performed (v2 `retry_count`).
    pub retry_count: u32,
    /// Wall-clock time the compaction took, summarizer call included (v2
    /// `duration_ms`).
    pub duration_ms: u64,
    /// The summarizer request's `prompt_cache_probe` (v2
    /// `source: 'compaction'`); `None` when no request ran.
    pub cache_probe: Option<crate::session::cache_observation::CacheProbeObservation>,
}

impl CompactionReport {
    /// The `compaction_finished` telemetry properties v2 reports for a
    /// completed compaction (`fullCompactionService.ts:768-795`). `source` is
    /// `"auto"` or `"manual"`; `turn_index` is `None` for a compaction between
    /// turns (v2 omits `turn_id` there).
    pub fn finished_payload(
        &self,
        source: &str,
        round: u32,
        thinking_effort: &str,
        turn_index: Option<u32>,
    ) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "event": "compaction_finished",
            "source": source,
            "tokens_before": self.tokens_before,
            "tokens_after": self.tokens_after,
            "duration_ms": self.duration_ms,
            "compacted_count": self.compacted_count,
            "retry_count": self.retry_count,
            "round": round,
            "thinking_effort": thinking_effort,
        });
        if let Some(index) = turn_index {
            payload["turn_id"] = serde_json::json!(index);
        }
        if self.dropped_count > 0 {
            payload["dropped_count"] = serde_json::json!(self.dropped_count);
        }
        if let Some(usage) = &self.usage {
            // v2 `usageTelemetry`: `input_tokens` is the *total* input — the
            // uncached remainder plus both cache buckets.
            payload["input_tokens"] = serde_json::json!(
                usage.input_tokens + usage.input_cache_read + usage.input_cache_creation
            );
            payload["output_tokens"] = serde_json::json!(usage.output_tokens);
            payload["input_cache_read"] = serde_json::json!(usage.input_cache_read);
            payload["input_cache_creation"] = serde_json::json!(usage.input_cache_creation);
        }
        payload
    }
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
    agent_id: &str,
    in_flight: bool,
    // v2 compaction probe baseline (the agent's last turn request).
    cache_probe_baseline: Option<&crate::session::cache_observation::CacheProbeRequest>,
    // The turn that triggered compaction (v2 `request.source.turnId`).
    cache_probe_turn_id: Option<&str>,
) -> Result<(Vec<LLMMessage>, CompactionReport), CompactionError> {
    let count = compute_compact_count(messages, config);
    if count == 0 {
        return Ok((
            messages.to_vec(),
            CompactionReport {
                summary: String::new(),
                compacted_count: 0,
                dropped_count: 0,
                tokens_before,
                tokens_after: tokens_before,
                usage: None,
                retry_count: 0,
                duration_ms: 0,
                cache_probe: None,
            },
        ));
    }
    // v2 summarizes the history it replaces. Since the shape below keeps only
    // real user input and the in-flight turn, everything up to the in-flight
    // start has to enter the summarizer request (the budgeted call pre-shrinks
    // it when needed, v2 #3911).
    let omitted_end = if in_flight {
        messages
            .iter()
            .rposition(|message| is_real_user_input(message))
            .map(|index| index + 1)
            .unwrap_or(messages.len())
            .max(count as usize)
    } else {
        messages.len()
    };
    let omitted = &messages[1..omitted_end.min(messages.len())];
    let started = std::time::Instant::now();
    let outcome = summarize_with_llm_budgeted(
        omitted,
        llm,
        instruction,
        cancel,
        config.max_attempts,
        effective_max_tokens,
        agent_id,
        cache_probe_baseline,
        cache_probe_turn_id,
    )
    .await?;
    let duration_ms = started.elapsed().as_millis() as u64;
    let summary = post_process_summary(outcome.summary, todos);
    let compacted = apply_compaction_with_summary(messages, count, summary.clone(), in_flight);
    let tokens_after = estimate_messages_tokens(&compacted);
    Ok((
        compacted,
        CompactionReport {
            summary,
            compacted_count: count,
            dropped_count: outcome.dropped_count,
            tokens_before,
            tokens_after,
            usage: outcome.usage,
            retry_count: outcome.retry_count,
            duration_ms,
            cache_probe: outcome.cache_probe,
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
    agent_id: &str,
) -> Result<(Vec<LLMMessage>, String), CompactionError> {
    let (compacted, summary, _usage) = force_compact_messages_manual_with_summary_report(
        messages,
        config,
        llm,
        instruction,
        cancel,
        effective_max_tokens,
        todos,
        agent_id,
    )
    .await?;
    Ok((compacted, summary))
}

/// [`force_compact_messages_manual_with_summary`] plus the summarizer's usage.
/// Every compaction is a real LLM call; callers that own session accounting
/// need the usage to bill it without re-deriving it from the text.
pub async fn force_compact_messages_manual_with_summary_report(
    messages: &[LLMMessage],
    config: &CompactionConfig,
    llm: &dyn LLM,
    instruction: Option<&str>,
    cancel: Option<&CancellationToken>,
    effective_max_tokens: Option<u32>,
    todos: Option<String>,
    agent_id: &str,
) -> Result<(Vec<LLMMessage>, String, Option<TokenUsage>), CompactionError> {
    let count = compute_compact_count_manual(messages, config);
    if count == 0 {
        return Ok((messages.to_vec(), String::new(), None));
    }
    let omitted = &messages[1..count as usize];
    let outcome = summarize_with_llm_budgeted(
        omitted,
        llm,
        instruction,
        cancel,
        config.max_attempts,
        effective_max_tokens,
        agent_id,
        None,
        None,
    )
    .await?;
    let usage = outcome.usage.clone();
    let summary = post_process_summary(outcome.summary, todos);
    let compacted = apply_compaction_with_summary(messages, count, summary.clone(), false);
    Ok((compacted, summary, usage))
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
        "",
        false,
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
    agent_id: &str,
    in_flight: bool,
    cache_probe_baseline: Option<&crate::session::cache_observation::CacheProbeRequest>,
    cache_probe_turn_id: Option<&str>,
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
            agent_id,
            in_flight,
            cache_probe_baseline,
            cache_probe_turn_id,
        )
        .await?,
    ))
}

/// The synthetic tool result v2 appends for a call still open when the
/// in-flight turn is carried across compaction (`openToolExchange.ts:5-6`).
const INHERITED_IN_FLIGHT_TOOL_OUTPUT: &str = "This tool call was still executing when this conversation snapshot was inherited from the source agent, so its result is not part of this context. The outcome is unknown — do not assume it succeeded or failed, and do not wait for it.";

/// The in-flight turn v2 `inFlightTurnOf` carries verbatim
/// (`fullCompactionService.ts:754-765`): everything after the last real user
/// input, with any trailing unresolved tool exchange closed.
fn in_flight_turn_messages(messages: &[LLMMessage], in_flight: bool) -> Vec<LLMMessage> {
    if !in_flight {
        return Vec::new();
    }
    let Some(last_user) = messages
        .iter()
        .rposition(|message| is_real_user_input(message))
    else {
        return Vec::new();
    };
    close_trailing_open_tool_exchange(&messages[last_user + 1..])
}

/// Close a trailing assistant tool call that has no recorded result
/// (v2 `closeTrailingOpenToolExchange`, `openToolExchange.ts:8-35`).
fn close_trailing_open_tool_exchange(history: &[LLMMessage]) -> Vec<LLMMessage> {
    let mut last_non_tool = history.len() as isize - 1;
    while last_non_tool >= 0 && history[last_non_tool as usize].role == "tool" {
        last_non_tool -= 1;
    }
    if last_non_tool < 0 {
        return history.to_vec();
    }
    let assistant = &history[last_non_tool as usize];
    if assistant.role != "assistant" || assistant.tool_calls.is_empty() {
        return history.to_vec();
    }
    let answered: std::collections::HashSet<&str> = history[last_non_tool as usize + 1..]
        .iter()
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect();
    let open_calls: Vec<_> = assistant
        .tool_calls
        .iter()
        .filter(|call| !answered.contains(call.id.as_str()))
        .collect();
    if open_calls.is_empty() {
        return history.to_vec();
    }
    let mut closed = history.to_vec();
    closed.extend(
        open_calls
            .into_iter()
            .map(|call| LLMMessage::tool_result(call.id.clone(), INHERITED_IN_FLIGHT_TOOL_OUTPUT)),
    );
    closed
}

/// Project the history into v2's compaction shape
/// (`buildContextCompactionShape`, `compactionHandoff.ts:70-136`): the system
/// message (index 0), the real user input kept verbatim from the whole history
/// (head 2k tokens + elision note + tail 20k tokens), the in-flight turn, the
/// prefixed summary and the continuation note. Completed-turn assistant/tool
/// messages are dropped — they are covered by the summary. The older
/// `legacyTail` shape (summary + untouched tail) is not used by v2's engine
/// path either.
pub(crate) fn apply_compaction_with_summary(
    messages: &[LLMMessage],
    count: u32,
    summary: String,
    in_flight: bool,
) -> Vec<LLMMessage> {
    if count == 0 {
        return messages.to_vec();
    }
    let user_in_range: Vec<LLMMessage> = messages[1..]
        .iter()
        .filter(|m| is_real_user_input(m) && !is_compaction_summary_message(m))
        .cloned()
        .collect();
    let kept = select_kept_user_messages(&user_in_range);
    let in_flight_turn = in_flight_turn_messages(messages, in_flight);
    let summary_text = build_compaction_summary_text(&summary);
    let mut compacted = Vec::with_capacity(1 + kept.len() + in_flight_turn.len() + 2);
    compacted.push(messages[0].clone());
    compacted.extend(kept);
    compacted.extend(in_flight_turn);
    compacted.push(LLMMessage {
        role: "user".into(),
        content: summary_text,
        // v2 `createCompactionSummaryMessage`: the origin lets a later
        // compaction and the renderer tell this from typed user input.
        origin: Some(serde_json::json!({ "kind": COMPACTION_SUMMARY_ORIGIN_KIND })),
        ..Default::default()
    });
    compacted.push(compaction_continuation_message());
    compacted
}

/// Decide how many leading messages to compact.
///
/// Returns N where `messages[0..N]` is replaced by a summary placeholder
/// and `messages[N..]` is preserved. The system message (index 0) is never
/// compacted, so N is either 0 (no compaction) or >= 2. Mirrors the auto
/// path of `computeCompactCount` in strategy.ts.
pub fn compute_compact_count(messages: &[LLMMessage], config: &CompactionConfig) -> u32 {
    let count = window::compute_compact_count(messages, config, false);
    // A count of 1 would mean compacting only the system prompt, which is
    // never allowed — treat it as "nothing to compact".
    if count <= 1 { 0 } else { count }
}

/// Shrink `compacted_count` so the compacted prefix fits within the
/// context window. Mirrors `fitCompactCountToWindow` in strategy.ts.
#[cfg(test)]
fn fit_compact_count_to_window(
    messages: &[LLMMessage],
    compacted_count: u32,
    config: &CompactionConfig,
) -> u32 {
    window::fit_compact_count_to_window(messages, compacted_count, config)
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
    window::can_split_after(messages, index)
}

/// Whether the prefix `messages[0..=index]` ends with an unresolved tool
/// exchange — a trailing tool result whose owning assistant issued more
/// calls than the trailing results satisfy.
#[cfg(test)]
fn prefix_ends_with_open_tool_exchange(messages: &[LLMMessage], index: usize) -> bool {
    window::prefix_ends_with_open_tool_exchange(messages, index)
}

#[cfg(test)]
mod tests;
