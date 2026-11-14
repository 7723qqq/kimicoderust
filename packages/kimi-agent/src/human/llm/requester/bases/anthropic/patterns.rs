use crate::human::llm::message::{
    AssistantMessage, ContentPart, Message, SystemMessage, ToolMessage, UserMessage,
};
use crate::human::llm::protocol::rewrite::{Pattern, Rewrite};

const OMITTED_AUDIO_PLACEHOLDER: &str = "(audio omitted: not supported by this provider)";

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

#[derive(Debug, Clone, Copy)]
pub struct StripUnsignedThinking {
    pub preserve: bool,
}

impl Pattern<Message> for StripUnsignedThinking {
    fn name(&self) -> &str {
        "stripUnsignedThinking"
    }

    fn rewrite(&self, items: &[Message], index: usize) -> Option<Rewrite<Message>> {
        let message = items.get(index)?;
        let original = match message {
            Message::System(message) => &message.content,
            Message::User(message) => &message.content,
            Message::Assistant(message) => &message.content,
            Message::Tool(message) => &message.content,
        };
        let content = original
            .iter()
            .filter(|part| match part {
                ContentPart::Think { encrypted, .. } => {
                    encrypted.is_some() || self.preserve
                }
                _ => true,
            })
            .cloned()
            .collect::<Vec<_>>();
        if content.len() == original.len() {
            return None;
        }
        Some(Rewrite {
            consumed: 1,
            replacement: vec![with_content(message, content)],
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioToPlaceholder;

pub const AUDIO_TO_PLACEHOLDER: AudioToPlaceholder = AudioToPlaceholder;

impl Pattern<Message> for AudioToPlaceholder {
    fn name(&self) -> &str {
        "audioToPlaceholder"
    }

    fn rewrite(&self, items: &[Message], index: usize) -> Option<Rewrite<Message>> {
        let message = items.get(index)?;
        if matches!(message, Message::System(_)) {
            return None;
        }
        let original = match message {
            Message::System(message) => &message.content,
            Message::User(message) => &message.content,
            Message::Assistant(message) => &message.content,
            Message::Tool(message) => &message.content,
        };
        let is_tool = matches!(message, Message::Tool(_));
        let mut changed = false;
        let mut content = Vec::<ContentPart>::new();
        for part in original {
            match part {
                ContentPart::AudioUrl { .. } => {
                    let already_present = matches!(
                        content.last(),
                        Some(ContentPart::Text { text, .. }) if text == OMITTED_AUDIO_PLACEHOLDER
                    );
                    if !already_present {
                        content.push(ContentPart::Text {
                            text: OMITTED_AUDIO_PLACEHOLDER.to_string(),
                            meta: None,
                        });
                    }
                    changed = true;
                }
                ContentPart::Text { text, .. } if is_tool && text.is_empty() => {
                    changed = true;
                }
                _ => content.push(part.clone()),
            }
        }
        if !changed {
            return None;
        }
        Some(Rewrite {
            consumed: 1,
            replacement: vec![with_content(message, content)],
        })
    }
}
