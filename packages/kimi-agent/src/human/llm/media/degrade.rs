use std::collections::HashSet;
use std::sync::Arc;

use crate::human::llm::errors::LlmErrorKind;
use crate::human::llm::message::{
    AssistantMessage, ContentPart, Message, SystemMessage, ToolMessage, UserMessage,
};
use crate::human::llm::requester::recovery::{
    LlmRecovery, LlmRecoveryContext, LlmRecoveryProposalRecord,
};

pub const MEDIA_DEGRADE_KEEP_RECENT: usize = 2;
const MEDIA_RECOVERY_ID: &str = "media-degrade";

fn degraded_placeholder(part: &ContentPart) -> Option<&'static str> {
    match part {
        ContentPart::ImageUrl { .. } => Some("[image omitted: dropped to fit the provider request size limit; re-read the file to view it]"),
        ContentPart::AudioUrl { .. } => Some("[audio omitted: dropped to fit the provider request size limit; re-read the file to hear it]"),
        ContentPart::VideoUrl { .. } => Some("[video omitted: dropped to fit the provider request size limit; re-read the file to view it]"),
        _ => None,
    }
}

fn stripped_placeholder(part: &ContentPart) -> Option<&'static str> {
    match part {
        ContentPart::ImageUrl { .. } => Some("[image omitted for provider compatibility; re-read the file to view it or get conversion guidance]"),
        ContentPart::AudioUrl { .. } => Some("[audio omitted for provider compatibility; re-read the file to hear it]"),
        ContentPart::VideoUrl { .. } => Some("[video omitted for provider compatibility; re-read the file to view it]"),
        _ => None,
    }
}

fn with_content(message: &Message, content: Vec<ContentPart>) -> Message {
    match message {
        Message::System(message) => Message::System(SystemMessage {
            content,
            ..message.clone()
        }),
        Message::User(message) => Message::User(UserMessage {
            content,
            ..message.clone()
        }),
        Message::Assistant(message) => Message::Assistant(AssistantMessage {
            content,
            ..message.clone()
        }),
        Message::Tool(message) => Message::Tool(ToolMessage {
            content,
            ..message.clone()
        }),
    }
}

fn replace_media_parts(
    messages: &[Message],
    placeholder: fn(&ContentPart) -> Option<&'static str>,
    mut should_replace: impl FnMut(&ContentPart) -> bool,
) -> Vec<Message> {
    messages
        .iter()
        .map(|message| {
            let original = match message {
                Message::System(message) => &message.content,
                Message::User(message) => &message.content,
                Message::Assistant(message) => &message.content,
                Message::Tool(message) => &message.content,
            };
            let mut message_changed = false;
            let content = original
                .iter()
                .map(|part| {
                    let Some(text) = placeholder(part) else {
                        return part.clone();
                    };
                    if !should_replace(part) {
                        return part.clone();
                    }
                    message_changed = true;
                    ContentPart::Text {
                        text: text.to_string(),
                        meta: None,
                    }
                })
                .collect();
            if message_changed {
                with_content(message, content)
            } else {
                message.clone()
            }
        })
        .collect()
}

pub fn degrade_older_media_parts(messages: &[Message], keep_recent: usize) -> Vec<Message> {
    let media_count = messages
        .iter()
        .map(|message| {
            let content = match message {
                Message::System(message) => &message.content,
                Message::User(message) => &message.content,
                Message::Assistant(message) => &message.content,
                Message::Tool(message) => &message.content,
            };
            content
                .iter()
                .filter(|part| degraded_placeholder(part).is_some())
                .count()
        })
        .sum::<usize>();
    let mut to_degrade = media_count.saturating_sub(keep_recent);
    if to_degrade == 0 {
        return messages.to_vec();
    }
    replace_media_parts(messages, degraded_placeholder, |_| {
        if to_degrade == 0 {
            return false;
        }
        to_degrade -= 1;
        true
    })
}

pub fn strip_media_parts(messages: &[Message]) -> Vec<Message> {
    replace_media_parts(messages, stripped_placeholder, |_| true)
}

pub struct MediaDegradeRecovery;

impl LlmRecovery for MediaDegradeRecovery {
    fn propose(&self, ctx: LlmRecoveryContext) -> Option<LlmRecoveryProposalRecord> {
        let done = ctx
            .applied_recoveries
            .iter()
            .filter(|recovery| recovery.strategy == MEDIA_RECOVERY_ID)
            .map(|recovery| recovery.action.clone())
            .collect::<HashSet<_>>();
        let proposal = |action: &str, messages: Vec<Message>| LlmRecoveryProposalRecord {
            strategy: MEDIA_RECOVERY_ID.to_string(),
            action: action.to_string(),
            attempt_message_override: Some(messages),
            before_next_attempt: None,
        };
        if ctx.error.kind == LlmErrorKind::ImageFormat {
            if !done.contains("stripped") {
                let stripped = strip_media_parts(&ctx.messages);
                if stripped != ctx.messages {
                    return Some(proposal("stripped", stripped));
                }
            }
            return None;
        }
        if ctx.error.kind != LlmErrorKind::RequestTooLarge {
            return None;
        }
        if !done.contains("degraded") {
            let degraded = degrade_older_media_parts(&ctx.messages, MEDIA_DEGRADE_KEEP_RECENT);
            if degraded != ctx.messages {
                return Some(proposal("degraded", degraded));
            }
        }
        if !done.contains("stripped") {
            let stripped = strip_media_parts(&ctx.messages);
            if stripped != ctx.messages {
                return Some(proposal("stripped", stripped));
            }
        }
        None
    }
}

pub fn create_media_degrade_recovery() -> Arc<dyn LlmRecovery> {
    Arc::new(MediaDegradeRecovery)
}
