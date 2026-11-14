use crate::human::agent::turn::{AssistantMeta, HistoryEntry, HistoryMessage};
use crate::human::llm::message::{Message, Role, UserMessage};
use crate::human::llm::usage::empty_usage;
use crate::llm_adapter::contract::message::{Message as ContextMessage, to_llm_message};

pub const EMPTY_MACHINE_PROMPT: UserMessage = UserMessage {
    role: Role::User,
    content: Vec::new(),
};

pub fn history_entry_from_context(message: ContextMessage) -> HistoryMessage {
    match to_llm_message(message) {
        Message::System(message) => HistoryMessage::System(HistoryEntry {
            message,
            meta: Some(Default::default()),
        }),
        Message::User(message) => HistoryMessage::User(HistoryEntry {
            message,
            meta: Some(Default::default()),
        }),
        Message::Assistant(message) => HistoryMessage::Assistant(HistoryEntry {
            message,
            meta: Some(AssistantMeta {
                usage: empty_usage(),
                ..Default::default()
            }),
        }),
        Message::Tool(message) => HistoryMessage::Tool(HistoryEntry {
            message,
            meta: Some(Default::default()),
        }),
    }
}

pub fn history_from_context(messages: &[ContextMessage]) -> Vec<HistoryMessage> {
    messages
        .iter()
        .cloned()
        .map(history_entry_from_context)
        .collect()
}
