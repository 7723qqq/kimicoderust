use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::human::llm::message::{ContentPart, Message};
use crate::human::llm::requester::bases::google_genai::contract::{
    GoogleContent, GoogleFileData, GoogleFunctionCall, GoogleFunctionResponse, GoogleInlineData,
    GooglePart,
};
use crate::human::llm::syntax_errors::SyntaxRequestFormatError;

const FUNCTION_RESPONSE_NESTED_MIME_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "application/pdf",
    "text/plain",
];

pub fn is_modern_gemini_model(model_name: &str) -> bool {
    let Some(rest) = model_name.strip_prefix("gemini-") else {
        return false;
    };
    let mut characters = rest.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    match first {
        '3'..='9' => characters.next() == Some('-'),
        '1'..='9' => {
            characters.next().is_some_and(|c| c.is_ascii_digit()) && characters.next() == Some('-')
        }
        _ => false,
    }
}

fn is_nested_function_response_part(part: &GooglePart) -> bool {
    part.inline_data
        .as_ref()
        .map(|inline_data| {
            FUNCTION_RESPONSE_NESTED_MIME_TYPES
                .contains(&inline_data.mime_type.to_lowercase().as_str())
        })
        .unwrap_or(false)
}

fn tool_call_id_to_name(tool_call_id: &str, tool_name_by_id: &HashMap<String, String>) -> String {
    if let Some(name) = tool_name_by_id.get(tool_call_id) {
        return name.clone();
    }
    let entropy_start = tool_call_id.len().checked_sub(9);
    let without_entropy = match entropy_start {
        Some(index)
            if tool_call_id.as_bytes()[index] == b'_'
                && tool_call_id[index + 1..]
                    .chars()
                    .all(|character| {
                        character.is_ascii_hexdigit() && !character.is_ascii_uppercase()
                    }) =>
        {
            &tool_call_id[..index]
        }
        _ => tool_call_id,
    };
    let Some(index) = without_entropy.rfind('_') else {
        return without_entropy.to_string();
    };
    if index == 0 || index + 1 == without_entropy.len() {
        return without_entropy.to_string();
    }
    without_entropy[..index].to_string()
}

fn convert_media_url(url: &str, fallback_mime_type: &str) -> GooglePart {
    if url.starts_with("data:") {
        let Some(comma_index) = url.find(',') else {
            return GooglePart {
                file_data: Some(GoogleFileData {
                    file_uri: url.to_string(),
                    mime_type: fallback_mime_type.to_string(),
                }),
                ..Default::default()
            };
        };
        let meta = &url[..comma_index];
        let data = &url[comma_index + 1..];
        let colon_index = meta.find(':');
        let semi_index = meta.find(';');
        let mime_type = match (colon_index, semi_index) {
            (Some(colon), Some(semi)) => {
                let start = colon + 1;
                if start <= semi {
                    meta[start..semi].to_string()
                } else {
                    String::new()
                }
            }
            _ => fallback_mime_type.to_string(),
        };
        return GooglePart {
            inline_data: Some(GoogleInlineData {
                mime_type,
                data: data.to_string(),
            }),
            ..Default::default()
        };
    }
    let mut mime_type = fallback_mime_type.to_string();
    if let Ok(parsed) = reqwest::Url::parse(url) {
        let pathname = parsed.path().to_lowercase();
        if pathname.ends_with(".png") {
            mime_type = "image/png".to_string();
        } else if pathname.ends_with(".jpg") || pathname.ends_with(".jpeg") {
            mime_type = "image/jpeg".to_string();
        } else if pathname.ends_with(".gif") {
            mime_type = "image/gif".to_string();
        } else if pathname.ends_with(".webp") {
            mime_type = "image/webp".to_string();
        } else if pathname.ends_with(".mp3") || pathname.ends_with(".mpeg") {
            mime_type = "audio/mpeg".to_string();
        } else if pathname.ends_with(".wav") {
            mime_type = "audio/wav".to_string();
        } else if pathname.ends_with(".ogg") {
            mime_type = "audio/ogg".to_string();
        }
    }
    GooglePart {
        file_data: Some(GoogleFileData {
            file_uri: url.to_string(),
            mime_type,
        }),
        ..Default::default()
    }
}

pub fn build_tool_name_by_id(messages: &[Message]) -> HashMap<String, String> {
    let mut tool_name_by_id = HashMap::new();
    for message in messages {
        let Message::Assistant(message) = message else {
            continue;
        };
        for tool_call in &message.tool_calls {
            tool_name_by_id.insert(tool_call.id.clone(), tool_call.name.clone());
        }
    }
    tool_name_by_id
}

/// Provider `rawId` keyed by the derived internal tool-call id, so a tool
/// response can echo the original provider id back out (v2 `toolCallOf(...).rawId`).
pub fn build_tool_raw_id_by_id(messages: &[Message]) -> HashMap<String, String> {
    let mut tool_raw_id_by_id = HashMap::new();
    for message in messages {
        let Message::Assistant(message) = message else {
            continue;
        };
        for tool_call in &message.tool_calls {
            if let Some(raw_id) = tool_call.raw_id.as_deref() {
                tool_raw_id_by_id.insert(tool_call.id.clone(), raw_id.to_string());
            }
        }
    }
    tool_raw_id_by_id
}

pub struct GoogleGenAILowerContext {
    pub tool_name_by_id: HashMap<String, String>,
    pub tool_raw_id_by_id: HashMap<String, String>,
    pub modern_function_responses: bool,
}

pub fn lower_message(
    message: &Message,
    lower: &GoogleGenAILowerContext,
) -> Result<Vec<GoogleContent>, SyntaxRequestFormatError> {
    if let Message::Tool(message) = message {
        let mut text_output = String::new();
        let mut media_parts = Vec::new();
        for part in &message.content {
            match part {
                ContentPart::Text { text, .. } => text_output.push_str(text),
                ContentPart::ImageUrl { image_url, .. } => {
                    media_parts.push(convert_media_url(&image_url.url, "image/jpeg"));
                }
                ContentPart::AudioUrl { audio_url, .. } => {
                    media_parts.push(convert_media_url(&audio_url.url, "audio/mpeg"));
                }
                ContentPart::VideoUrl { video_url, .. } => {
                    media_parts.push(convert_media_url(&video_url.url, "video/mp4"));
                }
                ContentPart::DocumentUrl { document_url, .. } => {
                    media_parts.push(convert_media_url(
                        &document_url.url,
                        &document_url.mime_type,
                    ));
                }
                ContentPart::Think { .. } => {}
            }
        }
        let (nested_parts, outer_media) = if lower.modern_function_responses {
            let mut nested = Vec::new();
            let mut outer = Vec::new();
            for part in media_parts {
                if is_nested_function_response_part(&part) {
                    nested.push(part);
                } else {
                    outer.push(part);
                }
            }
            (nested, outer)
        } else {
            (Vec::new(), media_parts)
        };
        let response_parts = (!nested_parts.is_empty()).then_some(nested_parts);
        let mut parts = vec![GooglePart {
            function_response: Some(GoogleFunctionResponse {
                id: lower
                    .tool_raw_id_by_id
                    .get(&message.tool_call_id)
                    .cloned(),
                name: tool_call_id_to_name(&message.tool_call_id, &lower.tool_name_by_id),
                response: HashMap::from_iter([("output".to_string(), text_output)]),
                parts: response_parts,
            }),
            ..Default::default()
        }];
        parts.extend(outer_media);
        return Ok(vec![GoogleContent {
            role: "user".to_string(),
            parts,
        }]);
    }
    if let Message::System(message) = message {
        let text = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![GoogleContent {
            role: "user".to_string(),
            parts: vec![GooglePart {
                text: Some(format!("<system>{text}</system>")),
                ..Default::default()
            }],
        }]);
    }
    let (role, content) = match message {
        Message::User(message) => ("user", &message.content),
        Message::Assistant(message) => ("model", &message.content),
        _ => unreachable!(),
    };
    let mut parts = Vec::new();
    for part in content {
        match part {
            ContentPart::Text { text, meta } => {
                let thought_signature = if role == "model" {
                    meta.as_ref()
                        .and_then(|meta| meta.get("thought_signature_b64"))
                        .and_then(Value::as_str)
                        .filter(|signature| !signature.is_empty())
                        .map(str::to_string)
                } else {
                    None
                };
                parts.push(GooglePart {
                    text: Some(text.clone()),
                    thought_signature,
                    ..Default::default()
                })
            }
            ContentPart::Think {
                think, encrypted, ..
            } => parts.push(GooglePart {
                text: Some(think.clone()),
                thought: Some(true),
                thought_signature: encrypted.as_ref().filter(|value| !value.is_empty()).cloned(),
                ..Default::default()
            }),
            ContentPart::ImageUrl { image_url, .. } => {
                parts.push(convert_media_url(&image_url.url, "image/jpeg"));
            }
            ContentPart::AudioUrl { audio_url, .. } => {
                parts.push(convert_media_url(&audio_url.url, "audio/mpeg"));
            }
            ContentPart::VideoUrl { video_url, .. } => {
                parts.push(convert_media_url(&video_url.url, "video/mp4"));
            }
            ContentPart::DocumentUrl { document_url, .. } => {
                parts.push(convert_media_url(&document_url.url, &document_url.mime_type));
            }
        }
    }
    if let Message::Assistant(message) = message {
        for tool_call in &message.tool_calls {
            let args = match tool_call.arguments.as_ref().filter(|arguments| !arguments.is_empty()) {
                None => Map::new(),
                Some(arguments) => {
                    let parsed = serde_json::from_str::<Value>(arguments).map_err(|_| {
                        SyntaxRequestFormatError::new("Tool call arguments must be valid JSON.")
                    })?;
                    match parsed {
                        Value::Object(args) => args,
                        _ => {
                            return Err(SyntaxRequestFormatError::new(
                                "Tool call arguments must be a JSON object.",
                            ));
                        }
                    }
                }
            };
            let thought_signature = tool_call
                .extras
                .as_ref()
                .and_then(|extras| extras.get("thought_signature_b64"))
                .and_then(Value::as_str)
                .map(str::to_string);
            parts.push(GooglePart {
                function_call: Some(GoogleFunctionCall {
                    id: tool_call.raw_id.clone(),
                    name: tool_call.name.clone(),
                    args,
                }),
                thought_signature,
                ..Default::default()
            });
        }
    }
    Ok(vec![GoogleContent {
        role: role.to_string(),
        parts,
    }])
}
