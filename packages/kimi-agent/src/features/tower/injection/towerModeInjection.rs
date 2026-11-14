use crate::turn_loop::types::LLMMessage;

pub const TOWER_MODE_DEDUP_MIN_TURNS: usize = 2;
pub const TOWER_MODE_FULL_REFRESH_TURNS: usize = 5;
pub const TOWER_MODE_INJECTION_VARIANT: &str = "tower_mode";
pub const TOWER_MODE_EXIT_DISCLOSURE: &str = "exit";
pub const TOWER_MODE_EXIT_REMINDER: &str = include_str!("tower-mode-exit-reminder.md");
pub const TOWER_MODE_FULL_REMINDER: &str = include_str!("tower-mode-full-reminder.md");
pub const TOWER_MODE_SPARSE_REMINDER: &str = include_str!("tower-mode-sparse-reminder.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TowerModeReminderVariant {
    Full,
    Sparse,
}

pub fn tower_mode_reminder_variant(
    injected_at: Option<usize>,
    history: &[LLMMessage],
) -> Option<TowerModeReminderVariant> {
    let Some(injected_at) = injected_at else {
        return Some(TowerModeReminderVariant::Full);
    };
    let mut assistant_turns_since = 0usize;
    for message in history.iter().skip(injected_at + 1) {
        if message.role == "assistant" {
            assistant_turns_since += 1;
            continue;
        }
        if message.role == "user"
            && message
                .origin
                .as_ref()
                .and_then(|origin| origin.get("kind"))
                .and_then(serde_json::Value::as_str)
                != Some("injection")
            && assistant_turns_since >= 1
        {
            return Some(TowerModeReminderVariant::Full);
        }
    }
    if assistant_turns_since >= TOWER_MODE_FULL_REFRESH_TURNS {
        return Some(TowerModeReminderVariant::Full);
    }
    if assistant_turns_since >= TOWER_MODE_DEDUP_MIN_TURNS {
        return Some(TowerModeReminderVariant::Sparse);
    }
    None
}

pub fn tower_mode_reminder(
    active: bool,
    flag_enabled: bool,
    injected_positions: usize,
    last_disclosure: Option<&str>,
    injected_at: Option<usize>,
    history: &[LLMMessage],
) -> Option<(&'static str, Option<&'static str>)> {
    if !active {
        if injected_positions == 0 || last_disclosure == Some(TOWER_MODE_EXIT_DISCLOSURE) {
            return None;
        }
        return Some((TOWER_MODE_EXIT_REMINDER, Some(TOWER_MODE_EXIT_DISCLOSURE)));
    }
    if !flag_enabled {
        return None;
    }
    if injected_positions == 0 || last_disclosure == Some(TOWER_MODE_EXIT_DISCLOSURE) {
        return Some((TOWER_MODE_FULL_REMINDER, None));
    }
    match tower_mode_reminder_variant(injected_at, history) {
        Some(TowerModeReminderVariant::Full) => Some((TOWER_MODE_FULL_REMINDER, None)),
        Some(TowerModeReminderVariant::Sparse) => Some((TOWER_MODE_SPARSE_REMINDER, None)),
        None => None,
    }
}
