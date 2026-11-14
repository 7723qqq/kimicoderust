use crate::agent::permission_policy::types::PermissionMode;
use crate::injection::InjectionContext;
use crate::turn_loop::types::LLMMessage;

pub const PERMISSION_MODE_REMINDER_ENV: &str = "KIMI_CODE_PERMISSION_MODE_REMINDER";

pub const AUTO_ENTER_REMINDER: &str = r#"Auto permission mode is active. Tool approvals will be handled automatically while this mode remains enabled.
  - Continue normally without pausing for approval prompts.
  - Do NOT call AskUserQuestion while auto mode is active; decide and continue.
  - Plan mode is the exception: ExitPlanMode still waits for the user to review the plan. Settle open questions yourself and list them under Assumptions in the plan."#;

pub const AUTO_EXIT_REMINDER: &str = r#"Auto permission mode is no longer active. Tool approvals and permission checks are back to the current mode.
  - Continue normally, but expect approval prompts or denials when a tool requires them."#;

const AUTO_ENTER_MARKER: &str = "Auto permission mode is active.";
const AUTO_EXIT_MARKER: &str = "Auto permission mode is no longer active.";

pub fn permission_mode_reminder_enabled() -> bool {
    crate::env::env_switch_default_on(PERMISSION_MODE_REMINDER_ENV)
}

pub struct PermissionModeTracker {
    last_mode: Option<PermissionMode>,
    injected: bool,
}

impl PermissionModeTracker {
    pub fn with_last_mode(last_mode: Option<PermissionMode>) -> Self {
        Self {
            last_mode,
            injected: last_mode.is_some(),
        }
    }

    pub fn step(&mut self, current: PermissionMode) -> Option<&'static str> {
        let previous = self.last_mode;
        if previous == Some(current) {
            if self.injected || current != PermissionMode::Auto {
                return None;
            }
            self.injected = true;
            return Some(AUTO_ENTER_REMINDER);
        }
        self.last_mode = Some(current);
        let reminder = if current == PermissionMode::Auto {
            Some(AUTO_ENTER_REMINDER)
        } else if previous == Some(PermissionMode::Auto) {
            Some(AUTO_EXIT_REMINDER)
        } else {
            None
        };
        self.injected |= reminder.is_some();
        reminder
    }
}

fn permission_mode_provider(
    mode: PermissionMode,
    baseline: Option<PermissionMode>,
) -> impl FnMut(&InjectionContext) -> Option<String> {
    let mut tracker = PermissionModeTracker::with_last_mode(baseline);
    move |ctx: &InjectionContext| {
        if ctx.rearmed {
            tracker =
                PermissionModeTracker::with_last_mode(scan_permission_mode_baseline(ctx.history));
        }
        tracker.step(mode).map(str::to_string)
    }
}

pub fn scan_permission_mode_baseline(messages: &[LLMMessage]) -> Option<PermissionMode> {
    for message in messages.iter().rev() {
        let content = message.content.as_str();
        if !crate::injection::is_system_reminder(content) {
            continue;
        }
        if content.contains(AUTO_ENTER_MARKER) {
            return Some(PermissionMode::Auto);
        }
        if content.contains(AUTO_EXIT_MARKER) {
            return Some(PermissionMode::Manual);
        }
    }
    None
}

pub fn register_permission_mode_injection(
    registry: &mut crate::injection::InjectionRegistry,
    mode: Option<PermissionMode>,
    baseline: Option<PermissionMode>,
) {
    if !permission_mode_reminder_enabled() {
        return;
    }
    let Some(mode) = mode else {
        return;
    };
    registry.register(
        "permission_mode",
        Box::new(permission_mode_provider(mode, baseline)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injection::{InjectionRegistry, is_system_reminder};

    fn reminder(content: &str) -> LLMMessage {
        crate::injection::injection_message(crate::injection::wrap_system_reminder(content))
    }

    fn message(content: &str) -> LLMMessage {
        LLMMessage {
            role: "user".into(),
            content: content.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn test_reminder_texts_carry_their_load_bearing_lines() {
        assert!(AUTO_ENTER_REMINDER.starts_with("Auto permission mode is active."));
        assert!(AUTO_ENTER_REMINDER.contains("Do NOT call AskUserQuestion while auto mode is active"));
        assert!(AUTO_ENTER_REMINDER.contains("Plan mode is the exception: ExitPlanMode still waits"));
        assert!(AUTO_ENTER_REMINDER.contains("list them under Assumptions in the plan"));
        assert!(AUTO_EXIT_REMINDER.starts_with("Auto permission mode is no longer active."));
        assert!(AUTO_EXIT_REMINDER.contains("expect approval prompts or denials"));
    }

    #[test]
    fn test_tracker_enters_stays_and_exits_auto() {
        let mut tracker = PermissionModeTracker::with_last_mode(None);
        assert_eq!(tracker.step(PermissionMode::Manual), None);
        assert_eq!(tracker.step(PermissionMode::Auto), Some(AUTO_ENTER_REMINDER));
        assert_eq!(tracker.step(PermissionMode::Auto), None);
        assert_eq!(tracker.step(PermissionMode::Manual), Some(AUTO_EXIT_REMINDER));
        assert_eq!(tracker.step(PermissionMode::Manual), None);
        assert_eq!(tracker.step(PermissionMode::Yolo), None);
        assert_eq!(tracker.step(PermissionMode::Auto), Some(AUTO_ENTER_REMINDER));
    }

    #[test]
    fn test_tracker_reannounces_auto_when_the_reminder_is_gone() {
        let mut tracker = PermissionModeTracker::with_last_mode(None);
        assert_eq!(tracker.step(PermissionMode::Auto), Some(AUTO_ENTER_REMINDER));
        assert_eq!(tracker.step(PermissionMode::Auto), None);
        let mut spliced = PermissionModeTracker::with_last_mode(None);
        assert_eq!(spliced.step(PermissionMode::Auto), Some(AUTO_ENTER_REMINDER));
    }

    #[test]
    fn test_tracker_baseline_suppresses_the_turn_head_repeat() {
        let mut tracker = PermissionModeTracker::with_last_mode(Some(PermissionMode::Auto));
        assert_eq!(tracker.step(PermissionMode::Auto), None);
        assert_eq!(tracker.step(PermissionMode::Manual), Some(AUTO_EXIT_REMINDER));
    }

    #[test]
    fn test_scan_permission_mode_baseline() {
        assert_eq!(scan_permission_mode_baseline(&[]), None);
        assert_eq!(scan_permission_mode_baseline(&[message("unrelated")]), None);
        assert_eq!(
            scan_permission_mode_baseline(&[reminder(AUTO_ENTER_REMINDER)]),
            Some(PermissionMode::Auto)
        );
        assert_eq!(
            scan_permission_mode_baseline(&[reminder(AUTO_EXIT_REMINDER)]),
            Some(PermissionMode::Manual)
        );
        assert_eq!(
            scan_permission_mode_baseline(&[
                reminder(AUTO_ENTER_REMINDER),
                message("unrelated"),
                reminder(AUTO_EXIT_REMINDER),
            ]),
            Some(PermissionMode::Manual)
        );
        assert_eq!(
            scan_permission_mode_baseline(&[
                reminder(AUTO_EXIT_REMINDER),
                reminder(AUTO_ENTER_REMINDER),
            ]),
            Some(PermissionMode::Auto)
        );
    }

    #[test]
    fn test_scan_ignores_user_text_quoting_the_reminder() {
        assert_eq!(scan_permission_mode_baseline(&[message(AUTO_ENTER_REMINDER)]), None);
        assert_eq!(
            scan_permission_mode_baseline(&[
                reminder(AUTO_ENTER_REMINDER),
                message(&format!("the docs say \"{AUTO_ENTER_MARKER}\" — what does that mean?")),
            ]),
            Some(PermissionMode::Auto)
        );
        assert_eq!(
            scan_permission_mode_baseline(&[message(&format!(
                "{AUTO_EXIT_MARKER} (quoting docs)"
            ))]),
            None
        );
    }

    #[test]
    fn test_registered_provider_injects_through_the_registry() {
        assert!(permission_mode_reminder_enabled());
        let mut registry = InjectionRegistry::new();
        register_permission_mode_injection(&mut registry, Some(PermissionMode::Auto), None);
        assert_eq!(registry.names(), vec!["permission_mode"]);
        let texts = registry.build_injections(true);
        assert_eq!(texts.len(), 1);
        assert!(is_system_reminder(&texts[0]));
        assert_eq!(texts[0], crate::injection::wrap_system_reminder(AUTO_ENTER_REMINDER));
        assert!(registry.build_injections(false).is_empty());
    }

    #[test]
    fn test_registration_skips_unknown_mode_and_non_auto() {
        assert!(permission_mode_reminder_enabled());
        let mut registry = InjectionRegistry::new();
        register_permission_mode_injection(&mut registry, None, None);
        assert!(registry.names().is_empty());
        let mut manual = InjectionRegistry::new();
        register_permission_mode_injection(&mut manual, Some(PermissionMode::Manual), None);
        assert!(manual.build_injections(true).is_empty());
        let mut exiting = InjectionRegistry::new();
        register_permission_mode_injection(
            &mut exiting,
            Some(PermissionMode::Manual),
            Some(PermissionMode::Auto),
        );
        assert_eq!(
            exiting.build_injections(true),
            vec![crate::injection::wrap_system_reminder(AUTO_EXIT_REMINDER)]
        );
    }
}
