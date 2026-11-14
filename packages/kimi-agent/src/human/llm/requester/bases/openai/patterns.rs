use crate::human::llm::message::{ContentPart, Message, ToolMessage, UserMessage};
use crate::human::llm::protocol::rewrite::{Pattern, Rewrite};

pub const TOOL_RESULT_MEDIA_PROMPT: &str = "Attached media from tool result:";
pub const TOOL_RESULT_MEDIA_PLACEHOLDER: &str = "(see attached media)";

fn is_extractable_media(part: &ContentPart) -> bool {
    match part {
        ContentPart::ImageUrl { .. } => true,
        ContentPart::VideoUrl { video_url, .. } => !video_url.url.starts_with("data:"),
        _ => false,
    }
}

pub struct ExtractToolMedia;

impl Pattern<Message> for ExtractToolMedia {
    fn name(&self) -> &str {
        "extractToolMedia"
    }

    fn rewrite(&self, items: &[Message], index: usize) -> Option<Rewrite<Message>> {
        if !matches!(items.get(index), Some(Message::Tool(_))) {
            return None;
        }
        let mut end = index;
        while end < items.len() && matches!(items[end], Message::Tool(_)) {
            end += 1;
        }
        let run = &items[index..end];
        let media = run
            .iter()
            .flat_map(|message| match message {
                Message::Tool(message) => message
                    .content
                    .iter()
                    .filter(|part| is_extractable_media(part))
                    .cloned()
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect::<Vec<_>>();
        if media.is_empty() {
            return None;
        }
        let stripped = run
            .iter()
            .map(|message| {
                let Message::Tool(message) = message else {
                    unreachable!()
                };
                let content = message
                    .content
                    .iter()
                    .filter(|part| !is_extractable_media(part))
                    .cloned()
                    .collect::<Vec<_>>();
                let had_image = message
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentPart::ImageUrl { .. }));
                let has_text = content.iter().any(|part| {
                    matches!(part, ContentPart::Text { text, .. } if !text.is_empty())
                });
                let has_audio = content
                    .iter()
                    .any(|part| matches!(part, ContentPart::AudioUrl { .. }));
                let has_data_video = content
                    .iter()
                    .any(|part| matches!(part, ContentPart::VideoUrl { .. }));
                if !has_text && !has_audio && !has_data_video && had_image {
                    return Message::Tool(ToolMessage {
                        role: message.role,
                        tool_call_id: message.tool_call_id.clone(),
                        content: std::iter::once(ContentPart::Text {
                            text: TOOL_RESULT_MEDIA_PLACEHOLDER.to_string(),
                            meta: None,
                        })
                        .chain(
                            message
                                .content
                                .iter()
                                .filter(|part| matches!(part, ContentPart::Think { .. }))
                                .cloned(),
                        )
                        .collect(),
                    });
                }
                if content.len() == message.content.len() {
                    return Message::Tool(message.clone());
                }
                Message::Tool(ToolMessage {
                    content,
                    ..message.clone()
                })
            })
            .collect::<Vec<_>>();
        let mut replacement = stripped;
        replacement.push(Message::User(UserMessage {
            role: crate::human::llm::message::Role::User,
            content: std::iter::once(ContentPart::Text {
                text: TOOL_RESULT_MEDIA_PROMPT.to_string(),
                meta: None,
            })
            .chain(media)
            .collect(),
        }));
        Some(Rewrite {
            consumed: run.len(),
            replacement,
        })
    }
}

pub static EXTRACT_TOOL_MEDIA: ExtractToolMedia = ExtractToolMedia;
