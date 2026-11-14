use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescription {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Default for Role {
    fn default() -> Self {
        Self::Assistant
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Map<String, Value>>,
    },
    #[serde(rename = "think", rename_all = "camelCase")]
    Think {
        think: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details_index: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_key: Option<String>,
    },
    #[serde(rename = "image_url")]
    ImageUrl {
        #[serde(rename = "imageUrl")]
        image_url: ImageUrl,
    },
    #[serde(rename = "audio_url")]
    AudioUrl {
        #[serde(rename = "audioUrl")]
        audio_url: AudioUrl,
    },
    #[serde(rename = "video_url")]
    VideoUrl {
        #[serde(rename = "videoUrl")]
        video_url: VideoUrl,
    },
    #[serde(rename = "document_url")]
    DocumentUrl {
        #[serde(rename = "documentUrl")]
        document_url: DocumentUrl,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<ImageDimensions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoUrl {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentUrl {
    pub url: String,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamIndex {
    Number(u64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "function", rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extras: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub _stream_index: Option<StreamIndex>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename = "tool_call_part", rename_all = "camelCase")]
pub struct ToolCallPart {
    pub arguments_part: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<StreamIndex>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamedMessagePart {
    Content(ContentPart),
    ToolCall(ToolCall),
    ToolCallPart(ToolCallPart),
}

impl From<ContentPart> for StreamedMessagePart {
    fn from(value: ContentPart) -> Self {
        Self::Content(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMessage {
    #[serde(deserialize_with = "deserialize_system_role")]
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDescription>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    #[serde(deserialize_with = "deserialize_user_role")]
    pub role: Role,
    pub content: Vec<ContentPart>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    #[serde(deserialize_with = "deserialize_assistant_role")]
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolMessage {
    #[serde(deserialize_with = "deserialize_tool_role")]
    pub role: Role,
    pub content: Vec<ContentPart>,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    System(SystemMessage),
    User(UserMessage),
    Assistant(AssistantMessage),
    Tool(ToolMessage),
}

fn deserialize_expected_role<'de, D>(deserializer: D, expected: Role) -> Result<Role, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let role = Role::deserialize(deserializer)?;
    if role == expected {
        Ok(role)
    } else {
        Err(serde::de::Error::custom(format!(
            "expected role '{expected:?}', got '{role:?}'"
        )))
    }
}

fn deserialize_system_role<'de, D>(deserializer: D) -> Result<Role, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_expected_role(deserializer, Role::System)
}

fn deserialize_user_role<'de, D>(deserializer: D) -> Result<Role, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_expected_role(deserializer, Role::User)
}

fn deserialize_assistant_role<'de, D>(deserializer: D) -> Result<Role, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_expected_role(deserializer, Role::Assistant)
}

fn deserialize_tool_role<'de, D>(deserializer: D) -> Result<Role, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_expected_role(deserializer, Role::Tool)
}

pub fn is_content_part(part: &StreamedMessagePart) -> bool {
    matches!(
        part,
        StreamedMessagePart::Content(
            ContentPart::Text { .. }
                | ContentPart::Think { .. }
                | ContentPart::ImageUrl { .. }
                | ContentPart::AudioUrl { .. }
                | ContentPart::VideoUrl { .. }
        )
    )
}

pub fn is_tool_call(part: &StreamedMessagePart) -> bool {
    matches!(part, StreamedMessagePart::ToolCall(_))
}

pub fn is_tool_call_part(part: &StreamedMessagePart) -> bool {
    matches!(part, StreamedMessagePart::ToolCallPart(_))
}

pub fn merge_in_place(target: &mut StreamedMessagePart, source: &StreamedMessagePart) -> bool {
    match (target, source) {
        (
            StreamedMessagePart::Content(ContentPart::Text { text: target, meta: target_meta }),
            StreamedMessagePart::Content(ContentPart::Text { text: source, meta: source_meta }),
        ) => {
            let target_has_signature = target_meta
                .as_ref()
                .and_then(|meta| meta.get("thought_signature_b64"))
                .is_some();
            let source_has_signature = source_meta
                .as_ref()
                .and_then(|meta| meta.get("thought_signature_b64"))
                .is_some();
            if target_has_signature || source_has_signature {
                return false;
            }
            target.push_str(source);
            true
        }
        (
            StreamedMessagePart::Content(ContentPart::Think {
                think: target_think,
                encrypted: target_encrypted,
                details_index: target_details_index,
                hidden: target_hidden,
                reasoning_key: target_reasoning_key,
            }),
            StreamedMessagePart::Content(ContentPart::Think {
                think: source_think,
                encrypted: source_encrypted,
                details_index: source_details_index,
                hidden: source_hidden,
                reasoning_key: source_reasoning_key,
            }),
        ) => {
            if target_encrypted.is_some()
                || target_details_index != source_details_index
                || target_hidden != source_hidden
                || target_reasoning_key != source_reasoning_key
            {
                return false;
            }
            target_think.push_str(source_think);
            if source_encrypted.is_some() {
                *target_encrypted = source_encrypted.clone();
            }
            true
        }
        (
            StreamedMessagePart::ToolCall(ToolCall { arguments, .. }),
            StreamedMessagePart::ToolCallPart(ToolCallPart { arguments_part, .. }),
        ) => {
            if let Some(arguments_part) = arguments_part {
                match arguments {
                    Some(arguments) => arguments.push_str(arguments_part),
                    None => *arguments = Some(arguments_part.clone()),
                }
            }
            true
        }
        _ => false,
    }
}

pub fn extract_text(content: &[ContentPart], separator: &str) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

pub fn get_text_content(content: &[ContentPart]) -> String {
    extract_text(content, "")
}

pub fn create_user_message(content: &str) -> UserMessage {
    UserMessage {
        role: Role::User,
        content: vec![ContentPart::Text {
            text: content.to_string(),
            meta: None,
        }],
    }
}

pub fn create_assistant_message(
    content: Vec<ContentPart>,
    tool_calls: Option<Vec<ToolCall>>,
) -> AssistantMessage {
    AssistantMessage {
        role: Role::Assistant,
        content,
        tool_calls: tool_calls.unwrap_or_default(),
    }
}

pub trait IntoToolContent {
    fn into_tool_content(self) -> Vec<ContentPart>;
}

impl IntoToolContent for &str {
    fn into_tool_content(self) -> Vec<ContentPart> {
        vec![ContentPart::Text {
            text: self.to_string(),
            meta: None,
        }]
    }
}

impl IntoToolContent for String {
    fn into_tool_content(self) -> Vec<ContentPart> {
        self.as_str().into_tool_content()
    }
}

impl IntoToolContent for Vec<ContentPart> {
    fn into_tool_content(self) -> Vec<ContentPart> {
        self
    }
}

pub fn create_tool_message(
    tool_call_id: &str,
    output: impl IntoToolContent,
) -> ToolMessage {
    ToolMessage {
        role: Role::Tool,
        content: output.into_tool_content(),
        tool_call_id: tool_call_id.to_string(),
    }
}

pub fn is_vacuous_content_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text, meta } => {
            let has_thought_signature = meta
                .as_ref()
                .and_then(|meta| meta.get("thought_signature_b64"))
                .is_some();
            !has_thought_signature && text.trim().is_empty()
        }
        ContentPart::Think {
            think,
            encrypted,
            hidden,
            ..
        } => encrypted.is_none() && hidden != &Some(true) && think.trim().is_empty(),
        _ => false,
    }
}

pub fn salvage_interrupted_message(message: &AssistantMessage) -> Option<AssistantMessage> {
    let content = message
        .content
        .iter()
        .filter(|part| !is_vacuous_content_part(part))
        .cloned()
        .collect::<Vec<_>>();
    if content.is_empty() {
        return None;
    }
    Some(AssistantMessage {
        role: Role::Assistant,
        content,
        tool_calls: Vec::new(),
    })
}

#[derive(Default)]
pub struct MessageAccumulator {
    message: AssistantMessage,
    tool_call_index_map: HashMap<StreamIndex, usize>,
    pending: Option<StreamedMessagePart>,
    deferred_think: Option<ContentPart>,
}

impl MessageAccumulator {
    pub fn new() -> Self {
        Self {
            message: AssistantMessage {
                role: Role::Assistant,
                content: Vec::new(),
                tool_calls: Vec::new(),
            },
            ..Default::default()
        }
    }

    fn flush(&mut self) {
        if let Some(pending) = self.pending.take() {
            match pending {
                StreamedMessagePart::Content(content) if is_content_part(&StreamedMessagePart::Content(content.clone())) => {
                    self.message.content.push(content);
                }
                StreamedMessagePart::ToolCall(call) => {
                    let ordinal = self.message.tool_calls.len();
                    let stream_index = call._stream_index.clone();
                    self.message.tool_calls.push(ToolCall {
                        _stream_index: None,
                        ..call
                    });
                    if let Some(index) = stream_index {
                        self.tool_call_index_map.insert(index, ordinal);
                    }
                }
                _ => {}
            }
        }
        if let Some(deferred) = self.deferred_think.take() {
            self.message.content.push(deferred);
        }
    }

    pub fn push(&mut self, part: StreamedMessagePart) {
        if let StreamedMessagePart::ToolCallPart(ToolCallPart {
            index: Some(index),
            arguments_part,
        }) = &part
        {
            let pending_has_same_index = matches!(
                &self.pending,
                Some(StreamedMessagePart::ToolCall(call))
                    if call._stream_index.as_ref() == Some(index)
            );
            if !pending_has_same_index
                && let Some(array_index) = self.tool_call_index_map.get(index).copied()
                && let Some(target) = self.message.tool_calls.get_mut(array_index)
            {
                if let Some(arguments_part) = arguments_part {
                    match &mut target.arguments {
                        Some(arguments) => arguments.push_str(arguments_part),
                        None => target.arguments = Some(arguments_part.clone()),
                    }
                }
                return;
            }
        }
        if matches!(part, StreamedMessagePart::Content(ContentPart::Text { .. })) {
            self.deferred_think = None;
        }
        if self.pending.is_none() {
            self.pending = Some(part);
            return;
        }
        let defer_empty_think = matches!(
            (&self.pending, &part),
            (
                Some(StreamedMessagePart::Content(ContentPart::Text { .. })),
                StreamedMessagePart::Content(content @ ContentPart::Think { .. })
            ) if is_vacuous_content_part(content)
        );
        if defer_empty_think {
            self.deferred_think = Some(part);
            return;
        }
        let pending = self.pending.as_mut().unwrap();
        if !merge_in_place(pending, &part) {
            self.flush();
            self.pending = Some(part);
        }
    }

    pub fn has_tool_call(&self) -> bool {
        !self.message.tool_calls.is_empty()
            || self.pending.as_ref().is_some_and(is_tool_call)
    }

    pub fn finish(&mut self) -> AssistantMessage {
        self.flush();
        self.message.clone()
    }
}

pub fn create_message_accumulator() -> MessageAccumulator {
    MessageAccumulator::new()
}
