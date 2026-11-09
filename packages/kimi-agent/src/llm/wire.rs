//! Provider-agnostic wire message used to project a turn's history into a
//! specific provider's request format.

use kimi_core::types::{ContentBlock, LLMMessage, ToolCall};

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

/// v2 `TOOL_INTERRUPTED_TEXT` (`contextProjector/projection.ts:394-395`): the
/// synthetic tool result for a call whose result never arrived.
pub const TOOL_INTERRUPTED_TEXT: &str = "Tool result is not available in the current context. Do not assume the tool completed successfully.";

/// v2 `projectStrict` (`contextProjector/projection.ts:70-79`), applied where
/// the provider request is assembled.
///
/// The stored history keeps its anomalies; the request must not. This performs
/// the full repair pipeline in v2's order:
///
/// 1. clean vacuous content parts (`cleanContent`);
/// 2. attach every tool result to its call in declaration order, synthesizing
///    an interrupted result for a call with none and dropping orphan results
///    (`project`);
/// 3. dedupe duplicate tool-call ids and result ids
///    (`dedupeDuplicateToolCalls`);
/// 4. merge consecutive assistants, content and tool calls concatenated
///    (`mergeConsecutiveAssistantMessages`);
/// 5. merge consecutive real-user messages (`canMergeUserMessage`);
/// 6. drop everything before the first user message, keeping the leading
///    system prompt this engine stores at index 0.
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

/// v2 `cleanContent`: drop vacuous content parts, and drop a whole
/// non-tool/non-system message that held nothing else.
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
        role: "tool".into(),
        content: TOOL_INTERRUPTED_TEXT.into(),
        tool_call_id: Some(tool_call_id.to_string()),
        ..Default::default()
    }
}

/// v2 `project`'s exchange repair: results follow their call in declaration
/// order; a call with no result gets an interrupted one; results whose id no
/// call declared are orphans and disappear.
fn attach_tool_results(messages: Vec<LLMMessage>) -> Vec<LLMMessage> {
    let mut out = Vec::with_capacity(messages.len());
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            out.push(message.clone());
            // An exchange ends at the next user message; a result outside its
            // own exchange is an orphan.
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
        // A tool message reaching this arm was not consumed by any call in its
        // exchange, so it has no declaration to answer.
        if message.role != "tool" {
            out.push(message.clone());
        }
        index += 1;
    }
    out
}

/// v2 `dedupeDuplicateToolCalls`: the first declaration of an id wins; later
/// ones are dropped, and a result whose id duplicates an earlier result is
/// dropped unless the earlier one is the synthetic interrupted placeholder.
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

/// v2 `mergeConsecutiveAssistantMessages`: unconditional merge — a provider
/// that requires alternating roles rejects two assistants in a row.
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

/// v2 `canMergeUserMessage`: only real user input merges. Injected reminders
/// are `user`-role but must stay distinct, or their text fuses into the user's.
fn can_merge_user(message: &LLMMessage) -> bool {
    if crate::injection::is_system_reminder(&message.content) {
        return false;
    }
    match &message.origin {
        None => true,
        Some(origin) => origin.get("kind").and_then(serde_json::Value::as_str) == Some("user"),
    }
}

/// v2's user merge: texts join with a blank line, non-text parts accumulate.
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

/// v2 `dropLeadingNonUserMessages`, keeping the leading system prompt this
/// engine stores at index 0 (v2 passes it outside the history list).
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

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "Read".into(),
            arguments: json!({}),
            extras: None,
            args_parse_error: None,
        }
    }

    #[test]
    fn the_default_projection_closes_an_unanswered_tool_call() {
        let messages = vec![
            LLMMessage {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                tool_calls: vec![call("c1")],
                ..Default::default()
            },
        ];

        let projected = project_messages(&messages);

        assert_eq!(projected.len(), 3, "{projected:?}");
        assert_eq!(projected[2].role, "tool");
        assert_eq!(projected[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(projected[2].content, TOOL_INTERRUPTED_TEXT);
    }

    #[test]
    fn the_default_projection_drops_an_orphan_tool_result() {
        let messages = vec![
            LLMMessage {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "tool".into(),
                content: "stray".into(),
                tool_call_id: Some("c_missing".into()),
                ..Default::default()
            },
        ];

        let projected = project_messages(&messages);

        assert_eq!(projected.len(), 1, "{projected:?}");
        assert_eq!(projected[0].role, "user");
    }

    #[test]
    fn the_default_projection_merges_real_user_messages_only() {
        let messages = vec![
            LLMMessage {
                role: "user".into(),
                content: "first".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "second".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "reminder".into(),
                origin: Some(json!({ "kind": "injection" })),
                ..Default::default()
            },
        ];

        let projected = project_messages(&messages);

        assert_eq!(projected.len(), 2, "{projected:?}");
        assert_eq!(projected[0].content, "first\n\nsecond");
        assert_eq!(projected[1].content, "reminder");
    }
}
