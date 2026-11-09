//! The interruption reminder, ported from v2's
//! `interruptionReminder/interruptionReminderService.ts`: after a turn the
//! user cancelled, the next turn's head carries a one-shot system reminder
//! that any partial output above is incomplete.
//!
//! v2 keys the reminder off `TurnEnded { reason: 'cancelled',
//! interruptReason: 'user_cancelled' }` and dedupes by inspecting whether the
//! last comparable message is already this variant. The engine's stop reason
//! alone cannot tell a user cancellation from a teardown or a host-side abort
//! (both flip the same cancel flag), so each host records whether *the user*
//! asked for the stop and arms the reminder through [`arms_reminder`]. The
//! caller reports the aborted outcome through
//! [`RunTurnInput::previous_turn_aborted`], and the history-scan baseline keeps
//! a resumed session from re-announcing a reminder already in the transcript.

use crate::injection::InjectionRegistry;
use crate::rpc::types::ContentBlock;
use crate::turn_loop::types::LLMMessage;

/// The injection variant v2 registers (`interruptionReminderOps.ts:7`).
pub const INTERRUPTION_REMINDER_VARIANT: &str = "interruption";

/// v2 `TurnEnded` filter (`interruptionReminderService.ts:34-36`): only an
/// abort the *user* asked for arms the reminder. A session teardown or a host
/// abort flips the same cancel flag, and telling the model "the user
/// interrupted" after one of those is false.
pub fn arms_reminder(turn_aborted: bool, user_cancelled: bool) -> bool {
    turn_aborted && user_cancelled
}

/// The reminder injected after a user-cancelled turn (v2
/// `interruptionReminderService.ts:15-19`, joined verbatim).
pub const INTERRUPTION_REMINDER: &str = "The previous turn was interrupted by the user before completion; any partial output shown above is incomplete. The user's next message continues the conversation.";

/// The reminder's opening line, used to recognize the variant in history.
const REMINDER_MARKER: &str = "The previous turn was interrupted by the user";

/// v2 `lastComparableMessage` (`interruptionReminderService.ts:52-66`):
/// invisible partial assistant messages are skipped; every other message is the
/// one the dedupe compares. The Rust loop has no `partial` flag — an assistant
/// message with no tool calls and only vacuous content is the same shape.
fn is_comparable(message: &LLMMessage) -> bool {
    if message.role != "assistant" || !message.tool_calls.is_empty() {
        return true;
    }
    if !message.content.trim().is_empty() {
        return true;
    }
    !message.blocks.iter().all(is_vacuous_block)
}

/// v2 `isVacuousContentPart` (`vacuousContent.ts:3-19`): empty text, and think
/// parts that carry neither text, an encrypted payload, nor a
/// `detailsSummary`. Media is never vacuous.
///
/// v2 keeps a details-derived summary in `think` behind `hidden: true`, so
/// `isVacuousContentPart` sees non-empty text and never calls it vacuous. Rust
/// parks the same payload in `details_summary` and leaves `think` empty, so the
/// summary must be counted here or an interrupted, replay-only reasoning block
/// is mistaken for an invisible partial message.
fn is_vacuous_block(block: &ContentBlock) -> bool {
    match block {
        ContentBlock::Text { text, .. } => text.trim().is_empty(),
        ContentBlock::Think {
            think,
            encrypted,
            details_summary,
            ..
        } => {
            encrypted.is_none()
                && think.trim().is_empty()
                && details_summary
                    .as_deref()
                    .is_none_or(|summary| summary.trim().is_empty())
        }
        _ => false,
    }
}

/// Whether a message is the interruption reminder (v2's `origin.kind ===
/// 'injection' && origin.variant === INTERRUPTION_REMINDER_VARIANT`).
///
/// Messages written before injections carried an origin are recognized by the
/// wrapped marker instead. The marker is trusted only inside a real injection:
/// a user quoting the line must not make a genuine post-abort reminder look
/// already-sent.
fn is_interruption_reminder(message: &LLMMessage) -> bool {
    if let Some(origin) = &message.origin {
        return origin.get("kind").and_then(serde_json::Value::as_str) == Some("injection")
            && origin.get("variant").and_then(serde_json::Value::as_str)
                == Some(INTERRUPTION_REMINDER_VARIANT);
    }
    let content = message.content.as_str();
    crate::injection::is_system_reminder(content) && content.contains(REMINDER_MARKER)
}

/// Whether the *last comparable* message already is the interruption reminder
/// (v2 `interruptionReminderService.ts:37-47`): the reminder is one-shot per
/// abort, so it re-arms only once the model has produced something after it.
/// Scanning the whole history instead would suppress every later interruption
/// for the rest of the session.
pub fn scan_interruption_baseline(messages: &[LLMMessage]) -> bool {
    messages
        .iter()
        .rev()
        .find(|message| is_comparable(message))
        .is_some_and(is_interruption_reminder)
}

/// Register the one-shot provider when the previous turn aborted and the
/// history does not already carry the reminder. The provider fires on the
/// turn's first injection pass only.
pub fn register_interruption_reminder(
    registry: &mut InjectionRegistry,
    previous_turn_aborted: bool,
    messages: &[LLMMessage],
) {
    if !previous_turn_aborted || scan_interruption_baseline(messages) {
        return;
    }
    // Per turn, not per step: the reminder heads the turn, so the providers
    // below actually see it. v2 appends it once through `notify()` rather than
    // registering a reminder variant, which is also why a mid-turn compaction
    // rearm (v2 `compactionRearmPending`) must not bring it back — the abort
    // notice describes the turn's first request, and the model has already
    // read it. `emitted` is that one-shot: `is_new_turn` is true again on a
    // rearm, so the flag, not the gate, is what makes it per-turn.
    let mut emitted = false;
    registry.register(
        INTERRUPTION_REMINDER_VARIANT,
        Box::new(move |ctx| {
            if !ctx.is_new_turn || emitted {
                return None;
            }
            emitted = true;
            Some(INTERRUPTION_REMINDER.to_string())
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::{is_system_reminder, wrap_system_reminder};

    fn message(content: &str) -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_reminder_text_carries_the_load_bearing_lines() {
        assert!(INTERRUPTION_REMINDER.starts_with("The previous turn was interrupted by the user"));
        assert!(INTERRUPTION_REMINDER.contains("any partial output shown above is incomplete"));
        assert!(
            INTERRUPTION_REMINDER.contains("The user's next message continues the conversation.")
        );
    }

    #[test]
    fn test_reminder_arms_only_for_a_user_cancel() {
        // v2 `TurnEnded` filter: a cancelled turn that nobody asked for (a
        // teardown, a host abort) must not tell the model the user interrupted.
        assert!(arms_reminder(true, true));
        assert!(!arms_reminder(true, false));
        assert!(!arms_reminder(false, true));
        assert!(!arms_reminder(false, false));
    }

    #[test]
    fn test_baseline_detects_a_prior_reminder() {
        assert!(!scan_interruption_baseline(&[]));
        assert!(!scan_interruption_baseline(&[message("unrelated")]));
        assert!(scan_interruption_baseline(&[message(
            &wrap_system_reminder(INTERRUPTION_REMINDER)
        )]));
    }

    #[test]
    fn test_baseline_only_compares_the_last_message() {
        // v2 checks `lastComparableMessage`, not the whole history: a reminder
        // followed by a newer turn must not suppress the next interruption.
        let history = [
            message(&wrap_system_reminder(INTERRUPTION_REMINDER)),
            message("a later user message"),
        ];
        assert!(!scan_interruption_baseline(&history));

        let with_assistant = [
            message(&wrap_system_reminder(INTERRUPTION_REMINDER)),
            LLMMessage::assistant("the model answered"),
        ];
        assert!(!scan_interruption_baseline(&with_assistant));
    }

    #[test]
    fn test_baseline_skips_an_empty_partial_assistant() {
        // v2 `lastComparableMessage` skips an invisible partial assistant
        // message, so a reminder followed only by that still counts as last.
        let history = [
            message(&wrap_system_reminder(INTERRUPTION_REMINDER)),
            LLMMessage {
                role: "assistant".into(),
                content: "   ".into(),
                ..Default::default()
            },
        ];
        assert!(scan_interruption_baseline(&history));
    }

    #[test]
    fn test_baseline_counts_a_replay_only_details_summary() {
        // v2 `isVacuousContentPart`: a details-derived think part keeps its
        // summary in `think` behind `hidden: true`, so it is not vacuous. Rust
        // parks it in `details_summary`, which must likewise keep the message
        // comparable; otherwise an interrupted, replay-only message is skipped
        // and the reminder baseline looks at the wrong message.
        let history = [
            message(&wrap_system_reminder(INTERRUPTION_REMINDER)),
            LLMMessage {
                role: "assistant".into(),
                blocks: vec![ContentBlock::Think {
                    think: String::new(),
                    encrypted: None,
                    details_index: Some(0),
                    reasoning_key: None,
                    details_summary: Some("the reasoning summary".into()),
                }],
                ..Default::default()
            },
        ];
        assert!(!scan_interruption_baseline(&history));
    }

    #[test]
    fn test_baseline_matches_the_origin_variant() {
        let history = [LLMMessage {
            role: "user".into(),
            content: wrap_system_reminder(INTERRUPTION_REMINDER),
            origin: Some(serde_json::json!({
                "kind": "injection",
                "variant": INTERRUPTION_REMINDER_VARIANT,
            })),
            ..Default::default()
        }];
        assert!(scan_interruption_baseline(&history));
    }

    #[test]
    fn test_baseline_ignores_user_text_quoting_the_reminder() {
        // A user reporting the reminder verbatim must not make a genuine
        // post-abort reminder look already-sent, which would drop the one
        // injection that tells the model its earlier output is incomplete.
        assert!(
            !scan_interruption_baseline(&[message(INTERRUPTION_REMINDER)]),
            "an unwrapped user message is not an injection"
        );
        assert!(
            scan_interruption_baseline(&[message(&wrap_system_reminder(INTERRUPTION_REMINDER))]),
            "a real injection still counts"
        );
    }

    #[test]
    fn test_reminder_is_per_turn_not_per_step() {
        let mut registry = InjectionRegistry::new();
        register_interruption_reminder(&mut registry, true, &[]);

        assert_eq!(
            registry.build_injections(true).len(),
            1,
            "the aborted-turn reminder heads the turn"
        );
        assert!(
            registry.build_injections(false).is_empty(),
            "a later step of the same turn must not inject it again"
        );
        assert!(
            registry.build_injections(true).is_empty(),
            "a mid-turn compaction rearm must not inject it again"
        );
    }

    #[test]
    fn test_registration_gates_on_abort_and_baseline() {
        let mut registry = InjectionRegistry::new();
        register_interruption_reminder(&mut registry, false, &[]);
        assert!(registry.names().is_empty(), "no abort: no reminder");

        let history = [message(&wrap_system_reminder(INTERRUPTION_REMINDER))];
        let mut registry = InjectionRegistry::new();
        register_interruption_reminder(&mut registry, true, &history);
        assert!(
            registry.names().is_empty(),
            "already reminded: not re-announced"
        );

        let mut registry = InjectionRegistry::new();
        register_interruption_reminder(&mut registry, true, &[]);
        assert_eq!(registry.names(), vec![INTERRUPTION_REMINDER_VARIANT]);
        let texts = registry.build_injections(true);
        assert_eq!(texts.len(), 1);
        assert!(is_system_reminder(&texts[0]));
        assert_eq!(texts[0], wrap_system_reminder(INTERRUPTION_REMINDER));
    }
}
