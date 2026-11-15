use kimi_core::types::{
    AudioUrl, ContentPart, DocumentUrl, ImageUrl, LLMMessage, MediaKind, ToolCall, VideoUrl,
};

use crate::rpc::types::ContentBlock;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    Text(String),
    Think(String),

    ToolCall {
        id: String,
        index: Option<usize>,
        arguments: String,
    },
}

impl StreamDelta {
    pub fn to_part(self) -> serde_json::Value {
        match self {
            StreamDelta::Text(text) => serde_json::json!({ "type": "text", "text": text }),
            StreamDelta::Think(think) => serde_json::json!({ "type": "think", "think": think }),
            StreamDelta::ToolCall {
                id,
                index,
                arguments,
            } => serde_json::json!({
                "type": "tool_call",
                "id": id,
                "index": index,
                "arguments": arguments,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WireMessage {
    pub role: String,

    pub content: String,

    pub blocks: Vec<ContentBlock>,

    pub tool_calls: Vec<ToolCall>,

    pub tool_call_id: Option<String>,
    pub tools: Vec<kimi_core::types::ToolDescription>,
}

impl WireMessage {
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: content.to_string(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
        }
    }

    pub fn with_blocks(role: &str, blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: role.to_string(),
            content: String::new(),
            blocks,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(content: &str, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
            blocks: Vec::new(),
            tool_calls,
            tool_call_id: None,
            tools: Vec::new(),
        }
    }

    pub fn tool_result(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.to_string(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.to_string()),
            tools: Vec::new(),
        }
    }
}

fn inline_image(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (metadata, data) = rest.split_once(";base64,")?;
    Some((metadata.to_string(), data.to_string()))
}

pub fn content_part_to_provider_block(part: &ContentPart) -> ContentBlock {
    match part {
        ContentPart::Text { text, meta } => ContentBlock::Text {
            text: text.clone(),
            meta: meta.clone(),
        },
        ContentPart::Think {
            think,
            encrypted,
            details_index,
            hidden,
            reasoning_key,
        } => ContentBlock::Think {
            think: think.clone(),
            encrypted: encrypted.clone(),
            details_index: *details_index,
            hidden: *hidden,
            reasoning_key: reasoning_key.clone(),
        },
        ContentPart::ImageUrl { image_url } => {
            if image_url.id.is_none()
                && image_url.dimensions.is_none()
                && image_url.detail.is_none()
                && let Some((media_type, data)) = inline_image(&image_url.url)
            {
                return ContentBlock::Image {
                    media_type,
                    data,
                    name: image_url.name.clone(),
                };
            }
            if image_url.id.is_none()
                && image_url.name.is_none()
                && image_url.dimensions.is_none()
                && image_url.detail.is_none()
                && let Some(file_id) = crate::llm::media_resolver::parse_daemon_file_url(&image_url.url)
            {
                return ContentBlock::MediaRef {
                    file_id: file_id.to_string(),
                    kind: MediaKind::Image,
                };
            }
            ContentBlock::ImageUrl {
                url: image_url.url.clone(),
                id: image_url.id.clone(),
                name: image_url.name.clone(),
                dimensions: image_url.dimensions.clone(),
                detail: image_url.detail.clone(),
            }
        }
        ContentPart::AudioUrl { audio_url } => {
            if audio_url.id.is_none()
                && let Some(file_id) = crate::llm::media_resolver::parse_daemon_file_url(&audio_url.url)
            {
                return ContentBlock::MediaRef {
                    file_id: file_id.to_string(),
                    kind: MediaKind::Audio,
                };
            }
            ContentBlock::AudioUrl {
                url: audio_url.url.clone(),
                id: audio_url.id.clone(),
                name: None,
            }
        }
        ContentPart::VideoUrl { video_url } => {
            if video_url.id.is_none()
                && video_url.name.is_none()
                && let Some(file_id) = crate::llm::media_resolver::parse_daemon_file_url(&video_url.url)
            {
                return ContentBlock::MediaRef {
                    file_id: file_id.to_string(),
                    kind: MediaKind::Video,
                };
            }
            ContentBlock::VideoUrl {
                url: video_url.url.clone(),
                id: video_url.id.clone(),
                name: video_url.name.clone(),
            }
        }
        ContentPart::DocumentUrl { document_url } => ContentBlock::DocumentUrl {
            url: document_url.url.clone(),
            mime_type: document_url.mime_type.clone(),
            id: document_url.id.clone(),
            name: document_url.name.clone(),
        },
    }
}

pub fn provider_block_to_content_part(block: &ContentBlock) -> ContentPart {
    match block {
        ContentBlock::Text { text, meta } => ContentPart::Text {
            text: text.clone(),
            meta: meta.clone(),
        },
        ContentBlock::Think {
            think,
            encrypted,
            details_index,
            hidden,
            reasoning_key,
        } => ContentPart::Think {
            think: think.clone(),
            encrypted: encrypted.clone(),
            details_index: *details_index,
            hidden: *hidden,
            reasoning_key: reasoning_key.clone(),
        },
        ContentBlock::Image {
            media_type,
            data,
            name,
        } => ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:{media_type};base64,{data}"),
                id: None,
                name: name.clone(),
                dimensions: None,
                detail: None,
            },
        },
        ContentBlock::MediaRef { file_id, kind } => {
            let url = format!("{}{file_id}", crate::llm::media_resolver::KIMI_FILE_SCHEME);
            match kind {
                MediaKind::Image => ContentPart::ImageUrl {
                    image_url: ImageUrl {
                        url,
                        id: None,
                        name: None,
                        dimensions: None,
                        detail: None,
                    },
                },
                MediaKind::Audio => ContentPart::AudioUrl {
                    audio_url: AudioUrl { url, id: None },
                },
                MediaKind::Video => ContentPart::VideoUrl {
                    video_url: VideoUrl {
                        url,
                        id: None,
                        name: None,
                    },
                },
            }
        }
        ContentBlock::ImageUrl {
            url,
            id,
            name,
            dimensions,
            detail,
        } => ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: url.clone(),
                id: id.clone(),
                name: name.clone(),
                dimensions: dimensions.clone(),
                detail: detail.clone(),
            },
        },
        ContentBlock::AudioUrl { url, id, .. } => ContentPart::AudioUrl {
            audio_url: AudioUrl {
                url: url.clone(),
                id: id.clone(),
            },
        },
        ContentBlock::VideoUrl { url, id, name } => ContentPart::VideoUrl {
            video_url: VideoUrl {
                url: url.clone(),
                id: id.clone(),
                name: name.clone(),
            },
        },
        ContentBlock::DocumentUrl {
            url,
            mime_type,
            id,
            name,
        } => ContentPart::DocumentUrl {
            document_url: DocumentUrl {
                url: url.clone(),
                mime_type: mime_type.clone(),
                id: id.clone(),
                name: name.clone(),
            },
        },
    }
}

pub fn tool_delivery_to_wire(
    delivery: &kimi_core::types::ToolDelivery,
) -> crate::rpc::types::ToolDelivery {
    crate::rpc::types::ToolDelivery {
        blocks: delivery
            .blocks
            .iter()
            .map(content_part_to_provider_block)
            .collect(),
        origin: delivery.origin.clone(),
    }
}

pub fn tool_delivery_from_wire(
    delivery: &crate::rpc::types::ToolDelivery,
) -> kimi_core::types::ToolDelivery {
    kimi_core::types::ToolDelivery {
        blocks: delivery
            .blocks
            .iter()
            .map(provider_block_to_content_part)
            .collect(),
        origin: delivery.origin.clone(),
    }
}

pub fn to_wire(messages: &[LLMMessage]) -> Vec<WireMessage> {
    messages
        .iter()
        .map(|m| WireMessage {
            role: m.role.as_str().to_string(),
            content: m.content.clone(),
            blocks: m.blocks.iter().map(content_part_to_provider_block).collect(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
            tools: m.tools.clone(),
        })
        .collect()
}

pub fn is_vacuous_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text, .. } => text.trim().is_empty(),
        ContentPart::Think {
            think,
            encrypted,
            hidden,
            ..
        } => {
            think.trim().is_empty()
                && encrypted.as_deref().is_none_or(str::is_empty)
                && *hidden != Some(true)
        }
        _ => false,
    }
}

pub fn project_vacuous_messages(messages: &[LLMMessage]) -> Vec<LLMMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        let mut projected = message.clone();
        if !message.blocks.is_empty() {
            projected.blocks = message
                .blocks
                .iter()
                .filter(|part| !is_vacuous_part(part))
                .cloned()
                .collect();
        }
        let keep = message.role == "system"
            || message.role == "tool"
            || !message.tool_calls.is_empty()
            || !projected.blocks.is_empty()
            || !projected.content.trim().is_empty();
        if keep {
            out.push(projected);
        }
    }
    out
}

pub const TOOL_INTERRUPTED_TEXT: &str = "Tool result is not available in the current context. Do not assume the tool completed successfully.";

pub fn project_strict(messages: &[LLMMessage]) -> Vec<LLMMessage> {
    let cleaned: Vec<LLMMessage> = messages.iter().filter_map(clean_message).collect();
    let reordered = attach_tool_results(cleaned);
    let deduped = dedupe_tool_calls(reordered);
    let merged = merge_consecutive_assistants(deduped);
    let merged = merge_consecutive_user_messages(merged);
    drop_leading_non_user(merged)
}

pub fn project_messages(messages: &[LLMMessage]) -> Vec<LLMMessage> {
    let cleaned: Vec<LLMMessage> = messages.iter().filter_map(clean_message).collect();
    let attached = attach_tool_results(cleaned);
    merge_consecutive_user_messages(attached)
}

fn clean_message(message: &LLMMessage) -> Option<LLMMessage> {
    let mut projected = message.clone();
    if !message.blocks.is_empty() {
        projected.blocks = message
            .blocks
            .iter()
            .filter(|part| !is_vacuous_part(part))
            .cloned()
            .collect();
    }
    let keep = message.role == "system"
        || message.role == "tool"
        || !message.tool_calls.is_empty()
        || !projected.blocks.is_empty()
        || !projected.content.trim().is_empty();
    keep.then_some(projected)
}

fn interrupted_tool_result(tool_call_id: &str) -> LLMMessage {
    LLMMessage {
        role: kimi_core::types::Role::Tool,
        content: TOOL_INTERRUPTED_TEXT.into(),
        tool_call_id: Some(tool_call_id.to_string()),
        ..Default::default()
    }
}

fn attach_tool_results(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let mut out = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            out.push(message.clone());
            let end = messages[index + 1..]
                .iter()
                .position(|candidate| candidate.role == "user")
                .map(|offset| index + 1 + offset)
                .unwrap_or(messages.len());
            let mut consumed = std::collections::HashSet::new();
            for call in &message.tool_calls {
                let found = (index + 1..end).find(|position| {
                    !consumed.contains(position)
                        && messages[*position].role == "tool"
                        && messages[*position].tool_call_id.as_deref() == Some(call.id.as_str())
                });
                if let Some(position) = found {
                    consumed.insert(position);
                    out.push(messages[position].clone());
                } else {
                    out.push(interrupted_tool_result(&call.id));
                }
            }
            index += 1;
            continue;
        }
        if message.role != "tool" {
            out.push(message.clone());
        }
        index += 1;
    }
    out
}

fn dedupe_tool_calls(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let mut seen_calls = std::collections::HashSet::new();
    let mut result_indexes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut out: Vec<LLMMessage> = Vec::with_capacity(messages.len());
    for mut message in messages {
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            let kept: Vec<kimi_core::types::ToolCall> = message
                .tool_calls
                .iter()
                .filter(|call| seen_calls.insert(call.id.clone()))
                .cloned()
                .collect();
            if kept.len() == message.tool_calls.len() {
                out.push(message);
            } else if !kept.is_empty() || !message.content.trim().is_empty() {
                message.tool_calls = kept;
                out.push(message);
            }
            continue;
        }
        if message.role == "tool"
            && let Some(id) = message.tool_call_id.clone()
        {
            if let Some(previous) = result_indexes.get(&id).copied() {
                if out[previous].content == TOOL_INTERRUPTED_TEXT
                    && message.content != TOOL_INTERRUPTED_TEXT
                {
                    out[previous] = message;
                }
                continue;
            }
            result_indexes.insert(id, out.len());
        }
        out.push(message);
    }
    out
}

fn merge_consecutive_assistants(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let mut out: Vec<LLMMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(previous) = out.last_mut()
            && previous.role == "assistant"
            && message.role == "assistant"
        {
            previous.content.push_str(&message.content);
            previous.blocks.extend(message.blocks);
            previous.tool_calls.extend(message.tool_calls);
            continue;
        }
        out.push(message);
    }
    out
}

fn can_merge_user(message: &LLMMessage) -> bool {
    if crate::injection::is_system_reminder(&message.content) {
        return false;
    }
    match &message.origin {
        None => true,
        Some(origin) => origin.get("kind").and_then(serde_json::Value::as_str) == Some("user"),
    }
}

fn merge_consecutive_user_messages(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let mut out: Vec<LLMMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(previous) = out.last_mut()
            && previous.role == "user"
            && message.role == "user"
            && can_merge_user(previous)
            && can_merge_user(&message)
        {
            if !previous.content.is_empty() && !message.content.is_empty() {
                previous.content.push_str("\n\n");
            }
            previous.content.push_str(&message.content);
            previous.blocks.extend(message.blocks);
            continue;
        }
        out.push(message);
    }
    out
}

fn drop_leading_non_user(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let prefix = messages
        .iter()
        .take_while(|message| message.role == "system")
        .count();
    let rest = &messages[prefix..];
    let start = rest
        .iter()
        .position(|message| message.role == "user")
        .unwrap_or(rest.len());
    let mut out = messages[..prefix].to_vec();
    out.extend_from_slice(&rest[start..]);
    out
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_wire_preserves_tool_structure() {
        let messages = vec![
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "hi".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::Assistant,
                content: String::new(),
                blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: Some(json!({ "path": "a" }).to_string()),
                    extras: None,
                    raw_id: None,
                    _stream_index: None,
                }],
                tool_call_id: None,
                tools: Vec::new(),
                prompt_id: None,
                origin: None,
            },
            LLMMessage {
                role: kimi_core::types::Role::Tool,
                content: "body".into(),
                blocks: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: Some("c1".into()),
                tools: Vec::new(),
                prompt_id: None,
                origin: None,
            },
        ];

        let wire = to_wire(&messages);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[0].tool_calls.is_empty());
        assert_eq!(wire[1].tool_calls.len(), 1);
        assert_eq!(wire[1].tool_calls[0].id, "c1");
        assert_eq!(wire[2].tool_call_id.as_deref(), Some("c1"));
    }
}
