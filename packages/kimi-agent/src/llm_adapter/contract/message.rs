use serde::{Deserialize, Serialize};

use crate::human::llm::message::{
    AssistantMessage, IntoToolContent, Message as LlmMessage, SystemMessage, ToolMessage,
    UserMessage,
};

pub use crate::human::llm::message::{
    AudioUrl, ContentPart, ImageUrl, Role, StreamedMessagePart, ToolCall, ToolCallPart, VideoUrl,
    extract_text, get_text_content, is_content_part, is_tool_call, is_tool_call_part,
    merge_in_place,
};
use crate::human::llm::message::ToolDescription;

pub type Tool = ToolDescription;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub content: Vec<ContentPart>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self {
            role,
            name: None,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: None,
            tools: None,
        }
    }
}

pub fn is_tool_declaration_only_message(message: &Message) -> bool {
    message.tools.as_ref().is_some_and(|tools| !tools.is_empty())
        && message.content.is_empty()
        && message.tool_calls.is_empty()
}

pub fn create_user_message(content: &str) -> Message {
    Message::new(
        Role::User,
        vec![ContentPart::Text {
            text: content.to_string(),
            meta: None,
        }],
    )
}

pub fn create_assistant_message(
    content: Vec<ContentPart>,
    tool_calls: Option<Vec<ToolCall>>,
) -> Message {
    Message {
        role: Role::Assistant,
        content,
        tool_calls: tool_calls.unwrap_or_default(),
        ..Message::new(Role::Assistant, Vec::new())
    }
}

pub fn create_tool_message(tool_call_id: &str, output: impl IntoToolContent) -> Message {
    Message {
        role: Role::Tool,
        content: output.into_tool_content(),
        tool_call_id: Some(tool_call_id.to_string()),
        ..Message::new(Role::Tool, Vec::new())
    }
}

pub fn to_llm_message(message: Message) -> LlmMessage {
    match message.role {
        Role::System => LlmMessage::System(SystemMessage {
            role: Role::System,
            content: message.content,
            tools: message.tools,
        }),
        Role::User => LlmMessage::User(UserMessage {
            role: Role::User,
            content: message.content,
        }),
        Role::Assistant => LlmMessage::Assistant(AssistantMessage {
            role: Role::Assistant,
            content: message.content,
            tool_calls: message.tool_calls,
        }),
        Role::Tool => LlmMessage::Tool(ToolMessage {
            role: Role::Tool,
            content: message.content,
            tool_call_id: message.tool_call_id.unwrap_or_default(),
        }),
    }
}

pub fn from_llm_message(message: LlmMessage) -> Message {
    match message {
        LlmMessage::System(message) => Message {
            role: Role::System,
            content: message.content,
            tools: message.tools,
            ..Message::new(Role::System, Vec::new())
        },
        LlmMessage::User(message) => Message::new(Role::User, message.content),
        LlmMessage::Assistant(message) => Message {
            role: Role::Assistant,
            content: message.content,
            tool_calls: message.tool_calls,
            ..Message::new(Role::Assistant, Vec::new())
        },
        LlmMessage::Tool(message) => Message {
            role: Role::Tool,
            content: message.content,
            tool_call_id: Some(message.tool_call_id),
            ..Message::new(Role::Tool, Vec::new())
        },
    }
}

pub fn from_llm_assistant_message(message: AssistantMessage) -> Message {
    Message {
        role: Role::Assistant,
        content: message.content,
        tool_calls: message.tool_calls,
        ..Message::new(Role::Assistant, Vec::new())
    }
}
