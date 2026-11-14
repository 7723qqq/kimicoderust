use crate::agent::context_memory::types::{
    ContextMessage, PromptOrigin, SkillActivationTrigger,
};

use super::tools::notifyUser::notifyUser::NOTIFY_USER_TOOL_NAME;

pub const NOTIFY_USER_NUDGE_VARIANT: &str = "notify_user_nudge";
pub const NOTIFY_USER_NUDGE_THRESHOLD: u64 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifyStreak {
    pub rounds: u64,
    pub nudges: u64,
}

pub fn notify_streak(history: &[ContextMessage]) -> NotifyStreak {
    let mut rounds = 0;
    let mut nudges = 0;
    for message in history.iter().rev() {
        if starts_new_turn(message) {
            break;
        }
        if is_nudge(message) {
            nudges += 1;
        }
        if !is_tool_call_round(message) {
            continue;
        }
        if message
            .tool_calls
            .iter()
            .any(|call| call.name == NOTIFY_USER_TOOL_NAME)
        {
            break;
        }
        rounds += 1;
    }
    NotifyStreak { rounds, nudges }
}

pub fn notify_streak_before(history: &[ContextMessage], tool_call_id: &str) -> NotifyStreak {
    let index = history.iter().rposition(|message| {
        message.role == "assistant"
            && message
                .tool_calls
                .iter()
                .any(|call| call.id == tool_call_id)
    });
    notify_streak(match index {
        Some(index) => &history[..index],
        None => history,
    })
}

pub fn tool_call_rounds_since_position(history: &[ContextMessage], position: usize) -> u64 {
    history
        .get(position + 1..)
        .unwrap_or(&[])
        .iter()
        .filter(|message| is_tool_call_round(message))
        .count() as u64
}

pub fn should_nudge_notify_user(streak: u64, rounds_since_last_nudge: Option<u64>) -> bool {
    if streak < NOTIFY_USER_NUDGE_THRESHOLD {
        return false;
    }
    rounds_since_last_nudge.is_none_or(|rounds| rounds >= NOTIFY_USER_NUDGE_THRESHOLD)
}

pub fn render_notify_user_nudge(count: u64) -> String {
    format!(
        "You have gone through {count} rounds of tool calls without a NotifyUser update — the user has seen no progress from you since. Send one now: a one-line title with what you have concluded so far, and a structured, chat-style message (a well structured paragraph with a few bullet points, under ~1000 characters) with the details and what you will do next, batched with your next tool calls."
    )
}

fn starts_new_turn(message: &ContextMessage) -> bool {
    let Some(origin) = message.origin.as_ref() else {
        return false;
    };
    match origin {
        PromptOrigin::User(_)
        | PromptOrigin::CronJob(_)
        | PromptOrigin::CronMissed(_)
        | PromptOrigin::Task(_)
        | PromptOrigin::Retry(_)
        | PromptOrigin::PluginCommand(_) => true,
        PromptOrigin::SystemTrigger(origin) => origin.name != "stop_hook",
        PromptOrigin::SkillActivation(origin) => {
            origin.trigger == SkillActivationTrigger::UserSlash
        }
        _ => false,
    }
}

fn is_tool_call_round(message: &ContextMessage) -> bool {
    message.role == "assistant" && !message.tool_calls.is_empty()
}

fn is_nudge(message: &ContextMessage) -> bool {
    matches!(
        message.origin.as_ref(),
        Some(PromptOrigin::Injection(origin)) if origin.variant == NOTIFY_USER_NUDGE_VARIANT
    )
}
