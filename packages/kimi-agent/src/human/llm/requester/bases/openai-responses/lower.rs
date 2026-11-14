use crate::human::llm::message::{ContentPart, Message, Role};
use crate::human::llm::requester::bases::openai_responses::contract::*;
use crate::human::llm::requester::bases::tool_result_text::convert_tool_result_to_plain_text;

const OMITTED_AUDIO_PLACEHOLDER: &str = "(audio omitted: unsupported audio format)";
const OMITTED_VIDEO_PLACEHOLDER: &str = "(video omitted: not supported by this provider)";

fn content_parts_to_input_items(parts: &[ContentPart]) -> Vec<ResponsesInputContentItem> {
    let mut items = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text, .. } if !text.is_empty() => {
                items.push(ResponsesInputContentItem::InputText { text: text.clone() });
            }
            ContentPart::ImageUrl { image_url, .. } => {
                items.push(ResponsesInputContentItem::InputImage {
                    detail: Some(image_url.detail.clone().unwrap_or_else(|| "auto".to_string())),
                    image_url: image_url.url.clone(),
                });
            }
            ContentPart::AudioUrl { audio_url, .. } => {
                items.push(map_audio_url_to_input_item(&audio_url.url).unwrap_or_else(|| {
                    ResponsesInputContentItem::InputText {
                        text: OMITTED_AUDIO_PLACEHOLDER.to_string(),
                    }
                }));
            }
            ContentPart::VideoUrl { .. } => {
                items.push(ResponsesInputContentItem::InputText {
                    text: OMITTED_VIDEO_PLACEHOLDER.to_string(),
                });
            }
            ContentPart::DocumentUrl { document_url, .. } => {
                items.push(document_url_part_to_input_file(&document_url.url, document_url.name.as_deref()));
            }
            _ => {}
        }
    }
    items
}

fn content_parts_to_output_items(parts: &[ContentPart]) -> Vec<ResponsesInputContentItem> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } if !text.is_empty() => {
                Some(ResponsesInputContentItem::OutputText {
                    text: text.clone(),
                    annotations: Vec::new(),
                })
            }
            _ => None,
        })
        .collect()
}

fn message_content_to_function_output_items(parts: &[ContentPart]) -> Vec<ResponsesInputContentItem> {
    let mut items = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text, .. } if !text.is_empty() => {
                items.push(ResponsesInputContentItem::InputText { text: text.clone() });
            }
            ContentPart::ImageUrl { image_url, .. } => {
                items.push(ResponsesInputContentItem::InputImage {
                    detail: None,
                    image_url: image_url.url.clone(),
                });
            }
            ContentPart::AudioUrl { audio_url, .. } => {
                items.push(map_audio_url_to_input_item(&audio_url.url).unwrap_or_else(|| {
                    ResponsesInputContentItem::InputText {
                        text: OMITTED_AUDIO_PLACEHOLDER.to_string(),
                    }
                }));
            }
            ContentPart::VideoUrl { .. } => {
                items.push(ResponsesInputContentItem::InputText {
                    text: OMITTED_VIDEO_PLACEHOLDER.to_string(),
                });
            }
            ContentPart::DocumentUrl { document_url, .. } => {
                items.push(document_url_part_to_input_file(&document_url.url, document_url.name.as_deref()));
            }
            _ => {}
        }
    }
    items
}

fn document_url_part_to_input_file(url: &str, name: Option<&str>) -> ResponsesInputContentItem {
    if url.starts_with("data:") {
        return ResponsesInputContentItem::InputFile {
            file_data: Some(url.to_string()),
            filename: Some(name.unwrap_or("document.pdf").to_string()),
            file_url: None,
        };
    }
    ResponsesInputContentItem::InputFile {
        file_data: None,
        filename: None,
        file_url: Some(url.to_string()),
    }
}

fn map_audio_url_to_input_item(url: &str) -> Option<ResponsesInputContentItem> {
    if url.starts_with("data:audio/") {
        let mut parts = url.splitn(2, ',');
        let header = parts.next()?;
        let data = parts.next()?;
        let subtype = header.split('/').nth(1)?.split(';').next()?.to_lowercase();
        let extension = match subtype.as_str() {
            "mp3" | "mpeg" => "mp3",
            "wav" => "wav",
            _ => return None,
        };
        return Some(ResponsesInputContentItem::InputFile {
            file_data: Some(data.to_string()),
            filename: Some(format!("inline.{extension}")),
            file_url: None,
        });
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        return Some(ResponsesInputContentItem::InputFile {
            file_data: None,
            filename: None,
            file_url: Some(url.to_string()),
        });
    }
    None
}

const OPENAI_RESPONSES_DEVELOPER_ROLE_MODELS: &[&str] = &[
    "gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano", "gpt-5-codex", "o1", "o1-mini",
    "o1-pro", "o3", "o3-mini", "o3-pro", "o4-mini",
];

fn uses_open_ai_responses_developer_role(model_name: &str) -> bool {
    let normalized = model_name.to_lowercase();
    if OPENAI_RESPONSES_DEVELOPER_ROLE_MODELS.contains(&normalized.as_str()) {
        return true;
    }
    OPENAI_RESPONSES_DEVELOPER_ROLE_MODELS
        .iter()
        .any(|model| normalized.starts_with(&format!("{model}-")))
}

#[derive(Debug, Clone)]
pub struct OpenAIResponsesLowerOptions {
    pub model_name: String,
    pub extract_text: bool,
}

impl Default for OpenAIResponsesLowerOptions {
    fn default() -> Self {
        Self {
            model_name: String::new(),
            extract_text: false,
        }
    }
}

pub type OpenAIResponsesLowerContext = OpenAIResponsesLowerOptions;

pub fn lower_message(
    message: &Message,
    lower: &OpenAIResponsesLowerContext,
) -> Vec<ResponsesInputItem> {
    if let Message::Tool(tool) = message {
        return vec![ResponsesInputItem::FunctionCallOutput {
            call_id: tool.tool_call_id.clone(),
            output: if lower.extract_text {
                ResponsesOutputContent::Text(convert_tool_result_to_plain_text(message))
            } else {
                ResponsesOutputContent::Parts(message_content_to_function_output_items(&tool.content))
            },
        }];
    }
    let mut role = match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::Tool(_) => unreachable!(),
    };
    if role == "system" && uses_open_ai_responses_developer_role(&lower.model_name) {
        role = "developer";
    }
    let content = match message {
        Message::System(message) => &message.content,
        Message::User(message) => &message.content,
        Message::Assistant(message) => &message.content,
        Message::Tool(_) => unreachable!(),
    };
    let mut result = Vec::new();
    let mut pending_parts = Vec::new();
    let mut flush_pending = |result: &mut Vec<ResponsesInputItem>, pending_parts: &mut Vec<ContentPart>| {
        if pending_parts.is_empty() {
            return;
        }
        let content = if role == "assistant" {
            content_parts_to_output_items(pending_parts)
        } else {
            content_parts_to_input_items(pending_parts)
        };
        result.push(ResponsesInputItem::Message {
            role: role.to_string(),
            content,
        });
        pending_parts.clear();
    };
    let mut index = 0usize;
    while index < content.len() {
        let part = &content[index];
        if let ContentPart::Think {
            think,
            encrypted,
            ..
        } = part
        {
            flush_pending(&mut result, &mut pending_parts);
            let encrypted_value = encrypted.clone();
            let mut summaries = vec![ResponsesSummaryText {
                type_: "summary_text".to_string(),
                text: think.clone(),
            }];
            index += 1;
            while index < content.len() {
                let ContentPart::Think {
                    think,
                    encrypted: next_encrypted,
                    ..
                } = &content[index]
                else {
                    break;
                };
                if next_encrypted != &encrypted_value {
                    break;
                }
                summaries.push(ResponsesSummaryText {
                    type_: "summary_text".to_string(),
                    text: think.clone(),
                });
                index += 1;
            }
            result.push(ResponsesInputItem::Reasoning {
                summary: summaries,
                encrypted_content: encrypted_value,
            });
        } else {
            pending_parts.push(part.clone());
            index += 1;
        }
    }
    flush_pending(&mut result, &mut pending_parts);
    if let Message::Assistant(assistant) = message {
        for call in &assistant.tool_calls {
            result.push(ResponsesInputItem::FunctionCall {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone().unwrap_or_else(|| "{}".to_string()),
            });
        }
    }
    result
}
