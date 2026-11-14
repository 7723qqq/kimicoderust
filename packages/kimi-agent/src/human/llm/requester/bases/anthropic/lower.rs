use std::collections::HashSet;

use serde_json::{Map, Value};

use crate::human::llm::message::{ContentPart, Message, Role};
use crate::human::llm::requester::bases::anthropic::contract::{
    AnthropicSource, AnthropicWireContentBlock, AnthropicWireMessage,
};
use crate::human::llm::syntax_errors::SyntaxRequestFormatError;

const SUPPORTED_B64_VIDEO_TYPES: [&str; 8] = [
    "video/mp4",
    "video/mpeg",
    "video/quicktime",
    "video/webm",
    "video/x-matroska",
    "video/x-msvideo",
    "video/x-flv",
    "video/3gpp",
];

fn invalid_data_url(kind: &str, url: &str) -> SyntaxRequestFormatError {
    SyntaxRequestFormatError::new(format!("Invalid data URL for {kind}: {url}"))
}

fn image_url_part_to_anthropic(
    url: &str,
    accepted_mimes: &HashSet<String>,
) -> Result<AnthropicWireContentBlock, SyntaxRequestFormatError> {
    if let Some(without_scheme) = url.strip_prefix("data:") {
        let mut parts = without_scheme.split(";base64,");
        let media_type = parts.next().ok_or_else(|| invalid_data_url("image", url))?;
        let data = parts.next().ok_or_else(|| invalid_data_url("image", url))?;
        if !accepted_mimes.contains(media_type) {
            return Err(SyntaxRequestFormatError::new(format!(
                "Unsupported media type for base64 image: {media_type}, url: {url}"
            )));
        }
        return Ok(AnthropicWireContentBlock::Image {
            source: AnthropicSource::Base64 {
                data: data.to_string(),
                media_type: media_type.to_string(),
            },
            cache_control: None,
        });
    }
    Ok(AnthropicWireContentBlock::Image {
        source: AnthropicSource::Url {
            url: url.to_string(),
        },
        cache_control: None,
    })
}

fn video_url_part_to_anthropic(
    url: &str,
) -> Result<AnthropicWireContentBlock, SyntaxRequestFormatError> {
    if let Some(without_scheme) = url.strip_prefix("data:") {
        let mut parts = without_scheme.split(";base64,");
        let media_type = parts.next().ok_or_else(|| invalid_data_url("video", url))?;
        let data = parts.next().ok_or_else(|| invalid_data_url("video", url))?;
        if !SUPPORTED_B64_VIDEO_TYPES.contains(&media_type) {
            return Err(SyntaxRequestFormatError::new(format!(
                "Unsupported media type for base64 video: {media_type}, url: {url}"
            )));
        }
        return Ok(AnthropicWireContentBlock::Video {
            source: AnthropicSource::Base64 {
                media_type: media_type.to_string(),
                data: data.to_string(),
            },
            cache_control: None,
        });
    }
    Ok(AnthropicWireContentBlock::Video {
        source: AnthropicSource::Url {
            url: url.to_string(),
        },
        cache_control: None,
    })
}

fn document_url_part_to_anthropic(
    url: &str,
    mime_type: &str,
) -> Result<AnthropicWireContentBlock, SyntaxRequestFormatError> {
    if let Some(without_scheme) = url.strip_prefix("data:") {
        let mut parts = without_scheme.split(";base64,");
        let declared = parts
            .next()
            .ok_or_else(|| invalid_data_url("document", url))?
            .trim();
        let data = parts
            .next()
            .ok_or_else(|| invalid_data_url("document", url))?;
        return Ok(AnthropicWireContentBlock::Document {
            source: AnthropicSource::Base64 {
                media_type: if declared.is_empty() {
                    mime_type.to_string()
                } else {
                    declared.to_string()
                },
                data: data.to_string(),
            },
            cache_control: None,
        });
    }
    Ok(AnthropicWireContentBlock::Document {
        source: AnthropicSource::Url {
            url: url.to_string(),
        },
        cache_control: None,
    })
}

fn parse_tool_arguments(args: Option<&String>) -> Value {
    let Some(args) = args else {
        return Value::Object(Map::new());
    };
    if args.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(args).unwrap_or_else(|_| Value::Object(Map::new()))
}

pub fn message_content(message: &AnthropicWireMessage) -> Vec<AnthropicWireContentBlock> {
    message.content.clone()
}

pub fn is_anthropic_wire_message_empty(message: &AnthropicWireMessage) -> bool {
    message.content.is_empty()
}

pub fn lower_message(
    message: &Message,
    accepted_mimes: &HashSet<String>,
) -> Vec<AnthropicWireMessage> {
    let (role, content_parts) = match message {
        Message::System(message) => (Role::System, &message.content),
        Message::User(message) => (Role::User, &message.content),
        Message::Assistant(message) => (Role::Assistant, &message.content),
        Message::Tool(message) => (Role::Tool, &message.content),
    };
    let mut content = Vec::new();
    if role == Role::System {
        let text = content_parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        content.push(AnthropicWireContentBlock::Text {
            text: format!("<system>{text}</system>"),
            cache_control: None,
        });
    } else if role == Role::Tool {
        let mut blocks = Vec::new();
        for part in content_parts {
            let converted = match part {
                ContentPart::Text { text, .. } if !text.is_empty() => {
                    Some(Ok(AnthropicWireContentBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    }))
                }
                ContentPart::ImageUrl { image_url, .. } => {
                    Some(image_url_part_to_anthropic(&image_url.url, accepted_mimes))
                }
                ContentPart::VideoUrl { video_url, .. } => {
                    Some(video_url_part_to_anthropic(&video_url.url))
                }
                ContentPart::DocumentUrl { document_url, .. } => Some(
                    document_url_part_to_anthropic(&document_url.url, &document_url.mime_type),
                ),
                _ => None,
            };
            if let Some(converted) = converted {
                blocks.push(converted.unwrap_or_else(|error| std::panic::panic_any(error)));
            }
        }
        let tool_call_id = match message {
            Message::Tool(message) => message.tool_call_id.clone(),
            _ => unreachable!(),
        };
        content.push(AnthropicWireContentBlock::ToolResult {
            tool_use_id: tool_call_id,
            content: blocks,
            cache_control: None,
        });
    } else {
        for part in content_parts {
            let converted = match part {
                ContentPart::Think {
                    think, encrypted, ..
                } => Some(Ok(AnthropicWireContentBlock::Thinking {
                    thinking: think.clone(),
                    signature: encrypted.clone(),
                    cache_control: None,
                })),
                ContentPart::Text { text, .. } if !text.trim().is_empty() => {
                    Some(Ok(AnthropicWireContentBlock::Text {
                        text: text.clone(),
                        cache_control: None,
                    }))
                }
                ContentPart::ImageUrl { image_url, .. } => {
                    Some(image_url_part_to_anthropic(&image_url.url, accepted_mimes))
                }
                ContentPart::VideoUrl { video_url, .. } => {
                    Some(video_url_part_to_anthropic(&video_url.url))
                }
                ContentPart::DocumentUrl { document_url, .. } => Some(
                    document_url_part_to_anthropic(&document_url.url, &document_url.mime_type),
                ),
                ContentPart::AudioUrl { .. } => None,
                _ => None,
            };
            if let Some(converted) = converted {
                content.push(converted.unwrap_or_else(|error| std::panic::panic_any(error)));
            }
        }
        if let Message::Assistant(assistant) = message {
            for tool_call in &assistant.tool_calls {
                content.push(AnthropicWireContentBlock::ToolUse {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    input: parse_tool_arguments(tool_call.arguments.as_ref()),
                    cache_control: None,
                });
            }
        }
    }
    vec![AnthropicWireMessage {
        role: if role == Role::Assistant {
            "assistant".to_string()
        } else {
            "user".to_string()
        },
        content,
    }]
}
