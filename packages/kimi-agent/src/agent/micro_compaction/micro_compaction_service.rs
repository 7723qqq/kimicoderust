pub use super::micro_compaction::{
    DEFAULT_TRUNCATED_MARKER, MicroCompactionConfig, MicroCompactionState,
};
use crate::turn_loop::types::{ContentPart, LLMMessage};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DetectOutcome {
    NotIdleLongEnough,

    Triggered,

    ContextTooEmpty,
}

impl DetectOutcome {
    pub fn triggered(self) -> bool {
        matches!(self, Self::Triggered)
    }
}

pub fn detect(
    last_assistant_at_ms: Option<i64>,
    now_ms: i64,
    context_tokens: usize,
    max_context_tokens: Option<usize>,
    config: &MicroCompactionConfig,
) -> DetectOutcome {
    let Some(last) = last_assistant_at_ms else {
        return DetectOutcome::NotIdleLongEnough;
    };
    let idle_ms = now_ms - last;
    if idle_ms < 0 || (idle_ms as u64) < config.cache_missed_threshold_ms {
        return DetectOutcome::NotIdleLongEnough;
    }

    let ratio = match max_context_tokens {
        Some(max) if max > 0 => context_tokens as f64 / max as f64,
        _ => 1.0,
    };
    if ratio < config.min_context_usage_ratio {
        return DetectOutcome::ContextTooEmpty;
    }
    DetectOutcome::Triggered
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplacedToolResult {
    pub index: usize,

    pub tool_call_id: String,

    pub tokens_before: usize,

    pub tokens_after: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MicroCompactionOutcome {
    pub cutoff: usize,

    pub messages: Vec<LLMMessage>,

    pub changed: bool,

    pub replaced: Vec<ReplacedToolResult>,

    pub marker: String,
}

pub fn compact(messages: &[LLMMessage], config: &MicroCompactionConfig) -> MicroCompactionOutcome {
    apply_micro_compaction_at(
        messages,
        messages.len().saturating_sub(config.keep_recent_messages),
        config,
    )
}

pub fn apply_micro_compaction_at(
    messages: &[LLMMessage],
    cutoff: usize,
    config: &MicroCompactionConfig,
) -> MicroCompactionOutcome {
    let cutoff = cutoff.min(messages.len());
    let marker_tokens = estimate_tokens(&config.truncated_marker);

    let mut out: Vec<LLMMessage> = Vec::with_capacity(messages.len());
    let mut replaced: Vec<ReplacedToolResult> = Vec::new();
    let mut changed = false;

    for (i, msg) in messages.iter().enumerate() {
        let is_old = i < cutoff;
        let is_tool = msg.role == "tool";
        let has_call_id = msg.tool_call_id.is_some();
        let tokens = message_content_tokens(msg);

        if is_old && is_tool && has_call_id && tokens >= config.min_content_tokens {
            changed = true;
            replaced.push(ReplacedToolResult {
                index: i,
                tool_call_id: msg.tool_call_id.clone().unwrap_or_default(),
                tokens_before: tokens,
                tokens_after: marker_tokens,
            });

            out.push(LLMMessage {
                role: msg.role.clone(),
                content: config.truncated_marker.clone(),
                blocks: Vec::new(),
                tool_calls: msg.tool_calls.clone(),
                tool_call_id: msg.tool_call_id.clone(),

                prompt_id: msg.prompt_id.clone(),

                origin: msg.origin.clone(),
                tools: msg.tools.clone(),
            });
        } else {
            out.push(msg.clone());
        }
    }

    MicroCompactionOutcome {
        cutoff,
        messages: out,
        changed,
        replaced,
        marker: config.truncated_marker.clone(),
    }
}

fn message_content_tokens(msg: &LLMMessage) -> usize {
    if !msg.blocks.is_empty() {
        msg.blocks.iter().map(block_tokens).sum()
    } else {
        estimate_tokens(&msg.content)
    }
}

fn block_tokens(block: &ContentPart) -> usize {
    match block {
        ContentPart::Text { text, .. } => estimate_tokens(text),

        ContentPart::Think { think, .. } => estimate_tokens(think),
        _ => crate::human::llm::tokens::MEDIA_TOKEN_ESTIMATE as usize,
    }
}

fn estimate_tokens(text: &str) -> usize {
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for unit in text.encode_utf16() {
        if unit <= 127 {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn_loop::types::LLMMessage;

    fn config() -> MicroCompactionConfig {
        MicroCompactionConfig::default()
    }

    fn config_truncate_all() -> MicroCompactionConfig {
        MicroCompactionConfig {
            keep_recent_messages: 0,
            ..config()
        }
    }

    #[test]
    fn detect_requires_both_idle_time_and_a_full_context() {
        let cfg = config();
        let hour = 60 * 60 * 1000;
        let now = 10 * hour;

        assert_eq!(
            detect(None, now, 1000, Some(1000), &cfg),
            DetectOutcome::NotIdleLongEnough
        );

        assert_eq!(
            detect(Some(now - 1_000), now, 1000, Some(1000), &cfg),
            DetectOutcome::NotIdleLongEnough
        );

        assert_eq!(
            detect(Some(now - hour), now, 100, Some(1000), &cfg),
            DetectOutcome::ContextTooEmpty
        );

        assert_eq!(
            detect(Some(now - hour), now, 500, Some(1000), &cfg),
            DetectOutcome::Triggered
        );

        assert_eq!(
            detect(Some(now - hour + 1), now, 1000, Some(1000), &cfg),
            DetectOutcome::NotIdleLongEnough
        );
    }

    #[test]
    fn an_unknown_window_is_treated_as_full() {
        let cfg = config();
        let hour = 60 * 60 * 1000;
        let now = 10 * hour;
        for window in [None, Some(0)] {
            assert_eq!(
                detect(Some(now - hour), now, 1, window, &cfg),
                DetectOutcome::Triggered,
                "window {window:?} must not suppress the pass"
            );
        }
    }

    #[test]
    fn a_future_last_output_does_not_trigger() {
        let cfg = config();
        let now = 1_000_000;
        assert_eq!(
            detect(Some(now + 5_000), now, 10_000, Some(1000), &cfg),
            DetectOutcome::NotIdleLongEnough
        );
    }

    #[test]
    fn no_oversized_results_returns_unchanged() {
        let msgs = vec![
            LLMMessage::user("hi"),
            LLMMessage::tool_result("call_1", "small result"),
            LLMMessage::assistant("done"),
        ];
        let out = compact(&msgs, &config());
        assert!(!out.changed, "nothing should be replaced");
        assert_eq!(out.replaced.len(), 0);
        assert_eq!(out.messages, msgs, "messages must be returned unchanged");
    }

    #[test]
    fn single_oversized_tool_result_is_replaced_and_meta_preserved() {
        let big = "x".repeat(400);
        let msgs = vec![LLMMessage::tool_result("call_42", big)];
        let out = compact(&msgs, &config_truncate_all());
        assert!(out.changed);
        assert_eq!(out.replaced.len(), 1);
        assert_eq!(out.replaced[0].index, 0);
        assert_eq!(out.replaced[0].tool_call_id, "call_42");
        assert_eq!(out.replaced[0].tokens_before, 100);
        let replaced = &out.messages[0];
        assert_eq!(replaced.content, DEFAULT_TRUNCATED_MARKER);
        assert_eq!(replaced.tool_call_id.as_deref(), Some("call_42"));
        assert!(
            replaced.blocks.is_empty(),
            "blocks must be cleared on replace"
        );

        assert_ne!(replaced.content, "x".repeat(400));
    }

    #[test]
    fn threshold_boundary_at_exactly_min_content_tokens() {
        let cfg = config_truncate_all();

        let at = vec![LLMMessage::tool_result("a", "y".repeat(400))];
        let out_at = compact(&at, &cfg);
        assert!(out_at.changed, "exactly >= min must be replaced");

        let below = vec![LLMMessage::tool_result("b", "y".repeat(396))];
        let out_below = compact(&below, &cfg);
        assert!(!out_below.changed, "just below min must be kept");
    }

    #[test]
    fn cjk_counts_as_full_token() {
        let cfg = config_truncate_all();

        let cjk100 = "中".repeat(100);
        let out = compact(&[LLMMessage::tool_result("c", cjk100)], &cfg);
        assert!(out.changed, "100 CJK chars must reach the 100-token gate");
        assert_eq!(out.replaced[0].tokens_before, 100);

        let cjk99 = "中".repeat(99);
        let out99 = compact(&[LLMMessage::tool_result("d", cjk99)], &cfg);
        assert!(!out99.changed, "99 CJK chars stay below the gate");

        let mixed = format!("{}{}", "z".repeat(396), "中");
        let out_mixed = compact(&[LLMMessage::tool_result("e", mixed)], &cfg);
        assert!(
            out_mixed.changed,
            "396 ascii + 1 cjk = 100 tokens must be replaced"
        );
    }

    #[test]
    fn cjk_weight_also_applies_to_blocks() {
        let mut msg = LLMMessage::tool_result("f", "");
        msg.blocks = vec![ContentPart::Text {
            text: "本".repeat(100),
            meta: None,
        }];
        let out = compact(&[msg], &config_truncate_all());
        assert!(out.changed, "CJK-heavy blocks must be truncated");
    }

    #[test]
    fn multiple_candidates_selected_by_oldest_first_order() {
        let cfg = MicroCompactionConfig {
            keep_recent_messages: 2,
            ..config()
        };
        let big = "x".repeat(400);
        let msgs = vec![
            LLMMessage::user("prompt"),
            LLMMessage::tool_result("old_a", big.clone()),
            LLMMessage::tool_result("old_b", big.clone()),
            LLMMessage::tool_result("old_c", big.clone()),
        ];
        let out = compact(&msgs, &cfg);
        assert_eq!(out.cutoff, 2);

        assert_eq!(out.replaced.len(), 1);
        assert_eq!(out.replaced[0].index, 1);
        assert_eq!(out.replaced[0].tool_call_id, "old_a");

        assert_eq!(out.messages[2].content, big);
        assert_eq!(out.messages[2].tool_call_id.as_deref(), Some("old_b"));
        assert_eq!(out.messages[3].content, big);
        assert_eq!(out.messages[3].tool_call_id.as_deref(), Some("old_c"));
    }

    #[test]
    fn non_tool_messages_never_replaced() {
        let big = "x".repeat(400);
        let msgs = vec![LLMMessage::user(big.clone()), LLMMessage::assistant(big)];
        let out = compact(&msgs, &config());
        assert!(!out.changed, "only tool messages are eligible");
        assert_eq!(out.replaced.len(), 0);
    }

    #[test]
    fn the_cutoff_tracks_len_minus_keep() {
        let mut state = MicroCompactionState::default();
        assert!(state.advance(40, 20), "the first miss sets len - keep");
        assert_eq!(state.cutoff, 20);
        assert!(state.advance(30, 20), "a shorter history rewinds it");
        assert_eq!(state.cutoff, 10);
        assert!(!state.advance(30, 20), "an unchanged cutoff is a no-op");
        assert!(state.advance(45, 20));
        assert_eq!(state.cutoff, 25);
        assert!(state.advance(5, 20), "a history below `keep` floors at 0");
        assert_eq!(state.cutoff, 0);
    }

    #[test]
    fn truncation_clamps_and_clear_resets_the_cutoff() {
        let mut state = MicroCompactionState { cutoff: 12 };
        state.clamp(5);
        assert_eq!(state.cutoff, 5);
        state.clamp(50);
        assert_eq!(state.cutoff, 5, "clamp never grows the index");
        state.reset();
        assert_eq!(state.cutoff, 0);
    }

    #[test]
    fn a_stored_cutoff_truncates_the_same_prefix_as_the_history_grows() {
        let cfg = MicroCompactionConfig {
            keep_recent_messages: 2,
            ..config()
        };
        let big = "x".repeat(800);
        let mut history = vec![
            LLMMessage::user("go"),
            LLMMessage::tool_result("call-1", big.clone()),
            LLMMessage::assistant("ok"),
            LLMMessage::tool_result("call-2", big.clone()),
        ];

        let first = apply_micro_compaction_at(&history, 2, &cfg);
        assert_eq!(first.cutoff, 2);
        assert_eq!(first.replaced.len(), 1);
        assert_eq!(first.replaced[0].tool_call_id, "call-1");

        history.push(LLMMessage::assistant("more"));
        history.push(LLMMessage::tool_result("call-3", big.clone()));

        let warm = apply_micro_compaction_at(&history, 2, &cfg);
        assert_eq!(warm.replaced.len(), 1);
        assert_eq!(warm.replaced[0].tool_call_id, "call-1");
        assert_eq!(warm.messages[3].content, big, "call-2 stays intact");

        let mut state = MicroCompactionState { cutoff: 2 };
        assert!(state.advance(history.len(), 2));
        let next = apply_micro_compaction_at(&history, state.cutoff, &cfg);
        let ids: Vec<&str> = next
            .replaced
            .iter()
            .map(|replaced| replaced.tool_call_id.as_str())
            .collect();
        assert_eq!(ids, vec!["call-1", "call-2"]);
    }
}
