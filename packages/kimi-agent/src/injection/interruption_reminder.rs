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
use crate::turn_loop::types::LLMMessage;

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

/// Whether the conversation already carries an interruption reminder (v2's
/// dedupe against the last comparable message's injection variant): the
/// reminder is one-shot per abort, and a resumed session must not re-announce
/// one the model was already shown.
///
/// The marker is trusted only inside a real injection. Injections carry role
/// `user` (v2 appends them that way), so role cannot separate them, and a user
/// quoting the line — asking what it means, or reporting a bug that shows it —
/// would otherwise make a genuine post-abort reminder look already-sent.
pub fn scan_interruption_baseline(messages: &[LLMMessage]) -> bool {
    messages.iter().rev().any(|message| {
        let content = message.content.as_str();
        crate::injection::is_system_reminder(content) && content.contains(REMINDER_MARKER)
    })
}

/// Register the one-shot provider when the previous turn aborted and the
/// history does not already carry the reminder.
pub fn register_interruption_reminder(
    registry: &mut InjectionRegistry,
    previous_turn_aborted: bool,
    messages: &[LLMMessage],
) {
    if !previous_turn_aborted || scan_interruption_baseline(messages) {
        return;
    }
    registry.register(
        "interruption_reminder",
        Box::new(|_ctx| Some(INTERRUPTION_REMINDER.to_string())),
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
        assert_eq!(registry.names(), vec!["interruption_reminder"]);
        let texts = registry.build_injections(true);
        assert_eq!(texts.len(), 1);
        assert!(is_system_reminder(&texts[0]));
        assert_eq!(texts[0], wrap_system_reminder(INTERRUPTION_REMINDER));
    }
}
