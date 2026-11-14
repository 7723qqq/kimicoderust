use crate::human::llm::message::{ContentPart, Message, ToolMessage, extract_text};

const OMITTED_IMAGE_PLACEHOLDER: &str = "(image omitted: tool result converted to plain text)";
const OMITTED_AUDIO_PLACEHOLDER: &str = "(audio omitted: tool result converted to plain text)";
const OMITTED_VIDEO_PLACEHOLDER: &str = "(video omitted: tool result converted to plain text)";

pub fn convert_tool_result_to_plain_text(message: &Message) -> String {
    let content = match message {
        Message::System(message) => &message.content,
        Message::User(message) => &message.content,
        Message::Assistant(message) => &message.content,
        Message::Tool(message) => &message.content,
    };
    convert_content_to_plain_text(content)
}

pub fn convert_tool_message_to_plain_text(message: &ToolMessage) -> String {
    convert_content_to_plain_text(&message.content)
}

fn convert_content_to_plain_text(content: &[ContentPart]) -> String {
    let mut lines = Vec::new();
    let text = extract_text(content, "");
    if !text.is_empty() {
        lines.push(text);
    }
    if content.iter().any(|part| matches!(part, ContentPart::ImageUrl { .. })) {
        lines.push(OMITTED_IMAGE_PLACEHOLDER.to_string());
    }
    if content.iter().any(|part| matches!(part, ContentPart::AudioUrl { .. })) {
        lines.push(OMITTED_AUDIO_PLACEHOLDER.to_string());
    }
    if content.iter().any(|part| matches!(part, ContentPart::VideoUrl { .. })) {
        lines.push(OMITTED_VIDEO_PLACEHOLDER.to_string());
    }
    lines.join("\n")
}
