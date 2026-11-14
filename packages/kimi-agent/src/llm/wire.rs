use kimi_core::types::{ContentBlock, LLMMessage, ToolCall};

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

pub fn to_wire(messages: &[LLMMessage]) -> Vec<WireMessage> {
    messages
        .iter()
        .map(|m| WireMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            blocks: m.blocks.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
            tools: m.tools.clone(),
        })
        .collect()
}

#[cfg(test)]
mod wire_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_wire_preserves_tool_structure() {
        let messages = vec![
            LLMMessage {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                blocks: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    arguments: json!({ "path": "a" }),
                    extras: None,
                    args_parse_error: None,
                }],
                tool_call_id: None,
                tools: Vec::new(),
                prompt_id: None,
                origin: None,
            },
            LLMMessage {
                role: "tool".into(),
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
