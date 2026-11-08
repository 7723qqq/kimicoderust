//! Provider-agnostic wire message used to project a turn's history into a
//! specific provider's request format.

use crate::turn_loop::types::{ContentBlock, LLMMessage, ToolCall};

/// One piece of a streamed response, as the transport forwards it to the host.
///
/// The two accumulators (OpenAI Chat Completions, Anthropic Messages) report
/// their deltas through one channel so the caller never has to know which
/// grammar produced them — and so reasoning keeps its own identity instead of
/// being appended to the answer text.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    Text(String),
    Think(String),
    /// One tool-call argument fragment, forwarded while the call is still
    /// streaming. `index` is the provider's slot for the call, so successive
    /// fragments of one call can be assigned a single id; the id is what the
    /// turn's ledger normalizes before the fragment leaves the engine.
    ToolCall {
        id: String,
        index: Option<usize>,
        arguments: String,
    },
}

impl StreamDelta {
    /// The transcript part this delta becomes on the host boundary.
    ///
    /// Consumes the delta so its text moves into the value; the streaming
    /// path is per-token, so cloning the chunk here doubles the text
    /// allocations for the whole response.
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

/// A single chat message to be projected into a provider wire format.
///
/// Content is text-first, with optional multimodal `blocks` (text/image)
/// that take precedence when non-empty. Tool calls (assistant) and the
/// tool-result linkage (`tool_call_id`)
/// are carried structurally so multi-step tool turns round-trip faithfully
/// across providers with different tool-call encodings (OpenAI stringified
/// `function.arguments` vs. Anthropic `tool_use.input` objects).
#[derive(Debug, Clone)]
pub struct WireMessage {
    /// One of "system" | "user" | "assistant" | "tool".
    pub role: String,
    /// Text content. May be empty for an assistant turn that only calls tools.
    pub content: String,
    /// Multimodal content blocks. When non-empty, provider projections use
    /// these instead of the plain `content` text.
    pub blocks: Vec<ContentBlock>,
    /// Tool calls requested by an assistant turn (empty otherwise).
    pub tool_calls: Vec<ToolCall>,
    /// For `role == "tool"`: the id of the tool call this message answers.
    pub tool_call_id: Option<String>,
}

impl WireMessage {
    /// A plain text message with no tool calls or tool-result linkage.
    pub fn text(role: &str, content: &str) -> Self {
        Self {
            role: role.to_string(),
            content: content.to_string(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// A message carrying multimodal content blocks.
    pub fn with_blocks(role: &str, blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: role.to_string(),
            content: String::new(),
            blocks,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// An assistant message that issued `tool_calls` (optionally with text).
    pub fn assistant_tool_calls(content: &str, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.to_string(),
            blocks: Vec::new(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// A tool-result message answering the call `tool_call_id`.
    pub fn tool_result(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.to_string(),
            blocks: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }
}

/// Project the loop's `LLMMessage` history into provider-agnostic wire
/// messages. The two types are structurally identical today; this converter is
/// the single seam where the loop message model and the provider projection
/// input can diverge later (e.g. media content parts).
pub fn to_wire(messages: &[LLMMessage]) -> Vec<WireMessage> {
    messages
        .iter()
        .map(|m| WireMessage {
            role: m.role.clone(),
            content: m.content.clone(),
            blocks: m.blocks.clone(),
            tool_calls: m.tool_calls.clone(),
            tool_call_id: m.tool_call_id.clone(),
        })
        .collect()
}

/// v2 `isVacuousContentPart` (`agent/contextMemory/vacuousContent.ts`): a
/// text part that trims to nothing, or a think part with no display text and
/// no provider attestation.
///
/// This engine's think blocks carry two replay-only fields v2's part does not
/// have — `details_summary` (a `reasoning_details` array entry) and
/// `details_index` — and a block that exists only to rebuild that array is
/// content even when its display text is empty. Dropping it would silently
/// reshape the provider's reasoning replay.
pub fn is_vacuous_part(part: &ContentBlock) -> bool {
    match part {
        ContentBlock::Text { text, .. } => text.trim().is_empty(),
        ContentBlock::Think {
            think,
            encrypted,
            details_index,
            details_summary,
            ..
        } => {
            think.trim().is_empty()
                && encrypted.as_deref().is_none_or(str::is_empty)
                && details_summary.as_deref().is_none_or(str::is_empty)
                && details_index.is_none()
        }
        _ => false,
    }
}

/// v2 `contextProjector`'s vacuous-content rule, applied where a provider
/// request is assembled: vacuous parts are dropped from `blocks`, and a
/// message with no tool calls whose remaining content is entirely vacuous is
/// dropped whole. The stored history keeps the originals — v2's test asserts
/// both sides of that split.
///
/// Tool results and system messages are never dropped: a tool result answers a
/// call in the preceding assistant message, and removing it would leave the
/// call unanswered at the provider. v2 reaches the same outcome by handling
/// tool-result messages in a separate branch before the vacuous test.
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

#[cfg(test)]
mod projection_tests {
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
                prompt_id: None,
                origin: None,
            },
            LLMMessage {
                role: "tool".into(),
                content: "body".into(),
                blocks: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: Some("c1".into()),
                prompt_id: None,
                origin: None,
            },
        ];

        let wire = to_wire(&messages);
        assert_eq!(wire.len(), 3);
        assert!(wire[0].tool_calls.is_empty());
        assert_eq!(wire[1].tool_calls.len(), 1);
        assert_eq!(wire[1].tool_calls[0].id, "c1");
        assert_eq!(wire[2].tool_call_id.as_deref(), Some("c1"));
    }

    /// v2 `context.test.ts`: empty text parts are dropped only in the LLM
    /// projection, a whitespace-only message disappears whole, an assistant
    /// carrying tool calls keeps an empty content list, and an attested think
    /// block survives with no display text.
    #[test]
    fn the_projection_drops_vacuous_content_without_touching_history() {
        use crate::rpc::types::ContentBlock;
        let messages = vec![
            LLMMessage {
                role: "user".into(),
                content: String::new(),
                blocks: vec![
                    ContentBlock::Text {
                        text: String::new(),
                        meta: None,
                    },
                    ContentBlock::Text {
                        text: "Run the tool".into(),
                        meta: None,
                    },
                ],
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                blocks: vec![ContentBlock::Text {
                    text: String::new(),
                    meta: None,
                }],
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                blocks: vec![ContentBlock::Text {
                    text: String::new(),
                    meta: None,
                }],
                tool_calls: vec![ToolCall {
                    id: "call_empty".into(),
                    name: "empty".into(),
                    arguments: json!({}),
                    extras: None,
                    args_parse_error: None,
                }],
                ..Default::default()
            },
            LLMMessage {
                role: "tool".into(),
                content: "done".into(),
                tool_call_id: Some("call_empty".into()),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: String::new(),
                blocks: vec![ContentBlock::Think {
                    think: String::new(),
                    encrypted: Some("enc_empty_thinking".into()),
                    details_index: None,
                    reasoning_key: None,
                    details_summary: None,
                }],
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "   ".into(),
                ..Default::default()
            },
        ];

        let projected = project_vacuous_messages(&messages);
        assert_eq!(projected.len(), 4, "{projected:?}");
        // The first user message keeps its one real text part.
        assert_eq!(projected[0].blocks.len(), 1);
        // The assistant carrying the call keeps an empty content list.
        assert!(projected[1].blocks.is_empty());
        assert_eq!(projected[1].tool_calls.len(), 1);
        // The tool result survives, so the call stays answered.
        assert_eq!(projected[2].tool_call_id.as_deref(), Some("call_empty"));
        // The attested think block is content even with no display text.
        assert_eq!(projected[3].blocks.len(), 1);
        // Storage is untouched: the same messages still carry every part.
        assert_eq!(messages[0].blocks.len(), 2);
        assert_eq!(messages[1].blocks.len(), 1);
    }

    /// A think block that exists only to rebuild the provider's
    /// `reasoning_details` array is content even with empty display text; a
    /// whitespace-only text part is not.
    #[test]
    fn a_details_only_think_block_is_not_vacuous() {
        use crate::rpc::types::ContentBlock;
        let details_only = ContentBlock::Think {
            think: String::new(),
            encrypted: None,
            details_index: Some(0),
            reasoning_key: None,
            details_summary: Some("earlier reasoning".into()),
        };
        assert!(!is_vacuous_part(&details_only));
        assert!(is_vacuous_part(&ContentBlock::Text {
            text: " \n".into(),
            meta: None,
        }));
    }
}
