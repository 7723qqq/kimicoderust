use crate::human::llm::message::ContentPart;
use crate::llm_adapter::contract::message::Message;

fn byte_length(value: Option<&str>) -> u64 {
    value.map_or(0, |value| value.len() as u64)
}

pub fn estimate_llm_request_bytes(messages: &[Message]) -> u64 {
    let mut total = 0;
    for message in messages {
        for part in &message.content {
            match part {
                ContentPart::Text { text, .. } => total += byte_length(Some(text)),
                ContentPart::Think {
                    think, encrypted, ..
                } => {
                    total += byte_length(Some(think));
                    total += byte_length(encrypted.as_deref());
                }
                ContentPart::ImageUrl { image_url } => {
                    total += byte_length(Some(&image_url.url));
                }
                ContentPart::AudioUrl { audio_url } => {
                    total += byte_length(Some(&audio_url.url));
                }
                ContentPart::VideoUrl { video_url } => {
                    total += byte_length(Some(&video_url.url));
                }
                ContentPart::DocumentUrl { .. } => {}
            }
        }
        for call in &message.tool_calls {
            total += byte_length(call.arguments.as_deref());
        }
    }
    total
}
