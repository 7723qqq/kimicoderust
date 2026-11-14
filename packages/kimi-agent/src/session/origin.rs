use std::sync::OnceLock;

use kimi_core::types::{ContentBlock, LLMMessage, MediaKind};
use serde_json::{Map, Value, json};

const SKILL_ACTIVATION_PART_SOURCE: &str = "skill activation";

fn skill_activation_part(text: String, activation_id: &str) -> ContentBlock {
    ContentBlock::Text {
        text,
        meta: Some(Map::from_iter([
            (
                "source".into(),
                Value::String(SKILL_ACTIVATION_PART_SOURCE.into()),
            ),
            ("activationId".into(), Value::String(activation_id.into())),
        ])),
    }
}

fn is_skill_activation_part(part: &ContentBlock) -> bool {
    matches!(
        part,
        ContentBlock::Text {
            meta: Some(meta),
            ..
        } if meta.get("source").and_then(Value::as_str) == Some(SKILL_ACTIVATION_PART_SOURCE)
    )
}

fn user_origin_of(message: &LLMMessage) -> Option<&Value> {
    message
        .origin
        .as_ref()
        .filter(|origin| origin.get("kind").and_then(Value::as_str) == Some("user"))
}

fn bundled_skill_activations_of(message: &LLMMessage) -> &[Value] {
    user_origin_of(message)
        .and_then(|origin| origin.get("skillActivations"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn content_parts_of(message: &LLMMessage) -> Vec<ContentBlock> {
    if message.blocks.is_empty() {
        return vec![ContentBlock::Text {
            text: message.content.clone(),
            meta: None,
        }];
    }
    message.blocks.clone()
}

fn annotate_bundled_skill_parts(
    content: &[ContentBlock],
    bundled_activations: &[Value],
) -> Vec<ContentBlock> {
    if bundled_activations.is_empty() || content.iter().any(is_skill_activation_part) {
        return content.to_vec();
    }
    let mut index = 0usize;
    content
        .iter()
        .map(|part| {
            let ContentBlock::Text { text, meta } = part else {
                return part.clone();
            };
            let has_source = meta
                .as_ref()
                .is_some_and(|meta| meta.contains_key("source"));
            if has_source {
                return part.clone();
            }
            let Some(activation_id) = bundled_activations
                .get(index)
                .and_then(|activation| activation.get("activationId"))
                .and_then(Value::as_str)
            else {
                return part.clone();
            };
            index += 1;
            skill_activation_part(text.clone(), activation_id)
        })
        .collect()
}

fn strip_bundled_skill_blocks(message: &LLMMessage) -> Vec<ContentBlock> {
    annotate_bundled_skill_parts(
        &content_parts_of(message),
        bundled_skill_activations_of(message),
    )
    .into_iter()
    .filter(|part| !is_skill_activation_part(part))
    .collect()
}

fn single_media_path_tag(text: &str) -> bool {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(
            r#"(?s)^<(image|video|audio|document|file)\b[^>]*?\bpath="([^"]*)"[^>]*>(.*)$"#,
        )
        .unwrap_or_else(|_| unreachable!())
    });
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Some(captures) = pattern.captures(trimmed) else {
        return false;
    };
    let kind = captures.get(1).map(|value| value.as_str()).unwrap_or("");
    let remainder = captures.get(3).map(|value| value.as_str()).unwrap_or("");
    remainder.is_empty() || remainder == format!("</{kind}>")
}

fn without_image_compression_captions(text: &str) -> String {
    const CAPTION_OPENING: &str = "<system>Image compressed to fit model limits:";
    if !text.contains(CAPTION_OPENING) {
        return text.to_string();
    }
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r#"(?s)<system>(Image compressed to fit model limits:.*?)</system>"#)
            .unwrap_or_else(|_| unreachable!())
    });
    pattern.replace_all(text, "").into_owned()
}

fn prompt_part_text(part: &ContentBlock) -> Option<String> {
    match part {
        ContentBlock::Text { text, .. } => {
            if single_media_path_tag(text) {
                return None;
            }
            let text = without_image_compression_captions(text);
            (!text.trim().is_empty()).then_some(text)
        }
        ContentBlock::Think { .. } => None,
        ContentBlock::Image { .. } | ContentBlock::ImageUrl { .. } => Some("[image]".into()),
        ContentBlock::AudioUrl { .. } => Some("[audio]".into()),
        ContentBlock::VideoUrl { .. } => Some("[video]".into()),
        ContentBlock::MediaRef { kind, .. } => match kind {
            MediaKind::Image => Some("[image]".into()),
            MediaKind::Audio => Some("[audio]".into()),
            MediaKind::Video => Some("[video]".into()),
        },
    }
}

fn prompt_display_text_from_content_parts(parts: &[ContentBlock]) -> String {
    parts
        .iter()
        .filter_map(prompt_part_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn non_empty_array<'a>(origin: Option<&'a Value>, key: &str) -> Option<&'a [Value]> {
    origin
        .and_then(|origin| origin.get(key))
        .and_then(Value::as_array)
        .filter(|values| !values.is_empty())
        .map(Vec::as_slice)
}

pub(crate) fn merge_steer_messages(messages: &[LLMMessage]) -> LLMMessage {
    let has_client_metadata = messages
        .iter()
        .any(|message| non_empty_array(user_origin_of(message), "clientMetadata").is_some());
    let mut client_metadata = Vec::new();
    if has_client_metadata {
        for message in messages {
            if let Some(metadata) = non_empty_array(user_origin_of(message), "clientMetadata") {
                client_metadata.extend_from_slice(metadata);
            } else {
                client_metadata.push(json!({
                    "display_text": prompt_display_text_from_content_parts(
                        &strip_bundled_skill_blocks(message),
                    ),
                }));
            }
        }
    }
    let skill_activations: Vec<Value> = messages
        .iter()
        .flat_map(|message| {
            non_empty_array(user_origin_of(message), "skillActivations").unwrap_or(&[])
        })
        .cloned()
        .collect();
    let attachments: Vec<Value> = messages
        .iter()
        .flat_map(|message| non_empty_array(user_origin_of(message), "attachments").unwrap_or(&[]))
        .cloned()
        .collect();
    let mut content = Vec::new();
    for message in messages {
        content.extend(
            annotate_bundled_skill_parts(
                &content_parts_of(message),
                bundled_skill_activations_of(message),
            )
            .into_iter()
            .filter(is_skill_activation_part),
        );
    }
    for message in messages {
        content.extend(strip_bundled_skill_blocks(message));
    }
    let mut origin = Map::from_iter([("kind".into(), Value::String("user".into()))]);
    if !client_metadata.is_empty() {
        origin.insert("clientMetadata".into(), Value::Array(client_metadata));
    }
    if !skill_activations.is_empty() {
        origin.insert("skillActivations".into(), Value::Array(skill_activations));
    }
    if !attachments.is_empty() {
        origin.insert("attachments".into(), Value::Array(attachments));
    }
    let legacy_content = content
        .iter()
        .filter_map(|part| match part {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    LLMMessage {
        role: "user".into(),
        content: legacy_content,
        blocks: content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        prompt_id: None,
        origin: Some(Value::Object(origin)),
        tools: Vec::new(),
    }
}
