use serde_json::{Map, Value, json};

use crate::human::llm::message::{ContentPart, Message, extract_text};
use crate::human::llm::requester::bases::openai::contract::*;
use crate::human::llm::requester::bases::openai::patterns::TOOL_RESULT_MEDIA_PLACEHOLDER;
use crate::human::llm::requester::bases::openai::reasoning_key::{
    DEFAULT_REASONING_KEY, REASONING_DETAILS_KEY,
};
use crate::human::llm::requester::requester::ToolMessageConversion;

const OMITTED_AUDIO_PLACEHOLDER: &str = "(audio omitted: not supported by this provider)";
const OMITTED_VIDEO_PLACEHOLDER: &str = "(video omitted: not supported by this provider)";

fn convert_content_part(part: &ContentPart) -> Option<OpenAIContentPart> {
    match part {
        ContentPart::Text { text, .. } => Some(OpenAIContentPart {
            type_: "text".to_string(),
            text: Some(text.clone()),
            image_url: None,
            audio_url: None,
            video_url: None,
            file: None,
        }),
        ContentPart::Think { .. } => None,
        ContentPart::ImageUrl { image_url, .. } => Some(OpenAIContentPart {
            type_: "image_url".to_string(),
            text: None,
            image_url: Some(OpenAIImageUrl {
                url: image_url.url.clone(),
                id: image_url.id.clone(),
                detail: image_url.detail.clone(),
            }),
            audio_url: None,
            video_url: None,
            file: None,
        }),
        ContentPart::AudioUrl { audio_url, .. } => Some(OpenAIContentPart {
            type_: "audio_url".to_string(),
            text: None,
            image_url: None,
            audio_url: Some(OpenAIAudioUrl {
                url: audio_url.url.clone(),
                id: audio_url.id.clone(),
            }),
            video_url: None,
            file: None,
        }),
        ContentPart::VideoUrl { video_url, .. } => Some(OpenAIContentPart {
            type_: "video_url".to_string(),
            text: None,
            image_url: None,
            audio_url: None,
            video_url: Some(OpenAIVideoUrl {
                url: video_url.url.clone(),
                id: video_url.id.clone(),
            }),
            file: None,
        }),
        ContentPart::DocumentUrl { document_url, .. } => Some(OpenAIContentPart {
            type_: "file".to_string(),
            text: None,
            image_url: None,
            audio_url: None,
            video_url: None,
            file: Some(OpenAIFile {
                file_data: document_url.url.clone(),
                filename: document_url.name.clone(),
            }),
        }),
    }
}

fn convert_tool_message_media_text(content: &[ContentPart]) -> String {
    let text = extract_text(content, "");
    let mut lines = if text.is_empty() { Vec::new() } else { vec![text] };
    if content.iter().any(|part| matches!(part, ContentPart::AudioUrl { .. })) {
        lines.push(OMITTED_AUDIO_PLACEHOLDER.to_string());
    }
    if content.iter().any(|part| {
        matches!(part, ContentPart::VideoUrl { video_url, .. } if video_url.url.starts_with("data:"))
    }) {
        lines.push(OMITTED_VIDEO_PLACEHOLDER.to_string());
    }
    if lines.is_empty() && content.iter().any(|part| matches!(part, ContentPart::ImageUrl { .. })) {
        return TOOL_RESULT_MEDIA_PLACEHOLDER.to_string();
    }
    lines.join("\n")
}

#[derive(Debug, Clone)]
pub struct OpenAILowerOptions {
    pub reasoning_key: String,
    pub preserve_thinking: bool,
    pub echo_full_reasoning: bool,
    pub tool_message_conversion: Option<ToolMessageConversion>,
}

pub type OpenAILowerContext = OpenAILowerOptions;

pub fn lower_message(message: &Message, lower: &OpenAILowerContext) -> Vec<OpenAIWireMessage> {
    let include_hidden = lower.echo_full_reasoning;
    let mut reasoning_content = String::new();
    let mut has_reasoning_part = false;
    let mut non_think_parts = Vec::new();
    let content_parts = match message {
        Message::System(message) => &message.content,
        Message::User(message) => &message.content,
        Message::Assistant(message) => &message.content,
        Message::Tool(message) => &message.content,
    };
    for part in content_parts {
        if let ContentPart::Think {
            think,
            hidden,
            ..
        } = part
        {
            has_reasoning_part = true;
            if include_hidden || hidden != &Some(true) {
                reasoning_content.push_str(think);
            }
        } else {
            non_think_parts.push(part);
        }
    }
    let mut content = None;
    if matches!(message, Message::Tool(_))
        && lower.tool_message_conversion != Some(ToolMessageConversion::KeepParts)
    {
        let converted = if content_parts.iter().any(|part| {
            !matches!(part, ContentPart::Text { .. } | ContentPart::Think { .. })
        }) {
            convert_tool_message_media_text(content_parts)
        } else {
            extract_text(content_parts, "")
        };
        content = Some(OpenAIContent::Text(converted));
    } else if non_think_parts.len() == 1
        && matches!(non_think_parts[0], ContentPart::Text { .. })
    {
        if let ContentPart::Text { text, .. } = non_think_parts[0] {
            content = Some(OpenAIContent::Text(text.clone()));
        }
    } else if !non_think_parts.is_empty() {
        content = Some(OpenAIContent::Parts(
            non_think_parts
                .iter()
                .filter_map(convert_content_part)
                .collect(),
        ));
    }
    let mut converted = match message {
        Message::Assistant(assistant) => OpenAIWireMessage::Assistant {
            content: match content {
                Some(content) => Some(content),
                None if has_reasoning_part && assistant.tool_calls.is_empty() => {
                    Some(OpenAIContent::Text(String::new()))
                }
                None => None,
            },
            tool_calls: (!assistant.tool_calls.is_empty()).then(|| {
                assistant
                    .tool_calls
                    .iter()
                    .map(|call| OpenAIWireToolCall {
                        id: call.id.clone(),
                        type_: "function".to_string(),
                        function: OpenAIWireFunction {
                            name: call.name.clone(),
                            arguments: call.arguments.clone().unwrap_or_default(),
                        },
                    })
                    .collect()
            }),
            extra: Map::new(),
        },
        Message::Tool(tool) => OpenAIWireMessage::Tool {
            tool_call_id: tool.tool_call_id.clone(),
            content: content.unwrap_or_else(|| OpenAIContent::Text(String::new())),
            extra: Map::new(),
        },
        Message::System(_) => OpenAIWireMessage::System {
            content: content.unwrap_or_else(|| OpenAIContent::Text(String::new())),
            extra: Map::new(),
        },
        Message::User(_) => OpenAIWireMessage::User {
            content: content.unwrap_or_else(|| OpenAIContent::Text(String::new())),
            extra: Map::new(),
        },
    };
    let mut reasoning_details = Vec::new();
    let mut string_fields = Map::<String, String>::new();
    let mut unstamped = String::new();
    let mut has_unstamped = false;
    for part in content_parts {
        let ContentPart::Think {
            think,
            encrypted,
            details_index,
            hidden,
            reasoning_key: part_reasoning_key,
        } = part
        else {
            continue;
        };
        if details_index.is_some() {
            if !think.is_empty() {
                reasoning_details.push(json!({ "type": "summary", "summary": think }));
            }
            if let Some(encrypted) = encrypted {
                reasoning_details.push(json!({ "type": "encrypted", "encrypted": encrypted }));
            }
            continue;
        }
        if let Some(key) = part_reasoning_key
            && key != REASONING_DETAILS_KEY
        {
            let current = string_fields.get(key).cloned().unwrap_or_default();
            let keep = include_hidden || hidden != &Some(true);
            string_fields.insert(
                key.clone(),
                if keep { format!("{current}{think}") } else { current },
            );
            continue;
        }
        has_unstamped = true;
        if include_hidden || hidden != &Some(true) {
            unstamped.push_str(think);
        }
    }
    if has_unstamped {
        let current = string_fields
            .get(&lower.reasoning_key)
            .cloned()
            .unwrap_or_default();
        string_fields.insert(lower.reasoning_key.clone(), format!("{current}{unstamped}"));
    }
    let string_fields_empty = string_fields.is_empty() && reasoning_details.is_empty();
    if !reasoning_details.is_empty() {
        converted
            .extra_mut()
            .insert(REASONING_DETAILS_KEY.to_string(), Value::Array(reasoning_details));
        converted.extra_mut().insert(
            DEFAULT_REASONING_KEY.to_string(),
            Value::String(
                string_fields
                    .get(DEFAULT_REASONING_KEY)
                    .cloned()
                    .unwrap_or(reasoning_content.clone()),
            ),
        );
    }
    for (key, value) in string_fields {
        converted.extra_mut().insert(key, Value::String(value));
    }
    if string_fields_empty
        && (has_reasoning_part || (lower.preserve_thinking && matches!(message, Message::Assistant(_))))
    {
        converted.extra_mut().insert(
            lower.reasoning_key.clone(),
            Value::String(reasoning_content),
        );
    }
    vec![converted]
}
