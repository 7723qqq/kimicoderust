use std::collections::{HashMap, HashSet};

use kimi_core::primitives::errors::domains::prompt::REQUEST_INVALID;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value};

use crate::agent::context_memory::tool_result_render::{
    RenderableToolOutput, RenderableToolResult, render_tool_result_for_model,
};
use crate::agent::context_memory::types::{ContextMessage, PromptOrigin};
use crate::agent::context_memory::vacuous_content::is_vacuous_content_part;
use crate::human::llm::message::{ContentPart, Role};
use crate::llm_adapter::contract::message::Message;
use crate::wire::record::WireAsyncError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionAnomaly {
    ToolResultReordered {
        tool_call_id: String,
    },
    ToolResultSynthesized {
        tool_call_id: String,
        trailing: bool,
    },
    OrphanToolResultDropped {
        tool_call_id: String,
    },
    DuplicateToolCallDropped {
        tool_call_id: String,
    },
    DuplicateToolResultDropped {
        tool_call_id: String,
    },
    LeadingNonUserDropped {
        role: String,
    },
    ConsecutiveAssistantsMerged,
    WhitespaceTextDropped {
        role: String,
    },
    VacuousMessageDropped {
        role: String,
    },
}

impl ProjectionAnomaly {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ToolResultReordered { .. } => "tool_result_reordered",
            Self::ToolResultSynthesized { .. } => "tool_result_synthesized",
            Self::OrphanToolResultDropped { .. } => "orphan_tool_result_dropped",
            Self::DuplicateToolCallDropped { .. } => "duplicate_tool_call_dropped",
            Self::DuplicateToolResultDropped { .. } => "duplicate_tool_result_dropped",
            Self::LeadingNonUserDropped { .. } => "leading_non_user_dropped",
            Self::ConsecutiveAssistantsMerged => "consecutive_assistants_merged",
            Self::WhitespaceTextDropped { .. } => "whitespace_text_dropped",
            Self::VacuousMessageDropped { .. } => "vacuous_message_dropped",
        }
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        match self {
            Self::ToolResultReordered { tool_call_id }
            | Self::ToolResultSynthesized { tool_call_id, .. }
            | Self::OrphanToolResultDropped { tool_call_id }
            | Self::DuplicateToolCallDropped { tool_call_id }
            | Self::DuplicateToolResultDropped { tool_call_id } => Some(tool_call_id),
            _ => None,
        }
    }

    pub fn trailing(&self) -> bool {
        matches!(self, Self::ToolResultSynthesized { trailing: true, .. })
    }
}

pub type OnAnomaly<'a> = &'a mut dyn FnMut(ProjectionAnomaly);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionRepairSummary {
    pub reordered: usize,
    pub synthesized: usize,
    pub dropped_orphan: usize,
    pub duplicate_calls_dropped: usize,
    pub duplicate_results_dropped: usize,
    pub leading_dropped: usize,
    pub assistants_merged: usize,
    pub whitespace_dropped: usize,
    pub vacuous_dropped: usize,
}

pub fn summarize_projection_repairs(
    anomalies: &[ProjectionAnomaly],
) -> ProjectionRepairSummary {
    let mut summary = ProjectionRepairSummary::default();
    for anomaly in anomalies {
        match anomaly {
            ProjectionAnomaly::ToolResultReordered { .. } => summary.reordered += 1,
            ProjectionAnomaly::ToolResultSynthesized { .. } => summary.synthesized += 1,
            ProjectionAnomaly::OrphanToolResultDropped { .. } => summary.dropped_orphan += 1,
            ProjectionAnomaly::DuplicateToolCallDropped { .. } => {
                summary.duplicate_calls_dropped += 1
            }
            ProjectionAnomaly::DuplicateToolResultDropped { .. } => {
                summary.duplicate_results_dropped += 1
            }
            ProjectionAnomaly::LeadingNonUserDropped { .. } => summary.leading_dropped += 1,
            ProjectionAnomaly::ConsecutiveAssistantsMerged => summary.assistants_merged += 1,
            ProjectionAnomaly::WhitespaceTextDropped { .. } => summary.whitespace_dropped += 1,
            ProjectionAnomaly::VacuousMessageDropped { .. } => summary.vacuous_dropped += 1,
        }
    }
    summary
}

pub fn project(
    history: &[ContextMessage],
    on_anomaly: Option<OnAnomaly<'_>>,
) -> Result<Vec<Message>, WireAsyncError> {
    let layout = slice_layout(history);
    let mut on_anomaly = on_anomaly;
    let blocks = pair_blocks(history, &layout, &mut on_anomaly)?;
    Ok(flatten_blocks(blocks, &layout, &mut on_anomaly))
}

pub fn project_strict(
    history: &[ContextMessage],
    on_anomaly: Option<OnAnomaly<'_>>,
) -> Result<Vec<Message>, WireAsyncError> {
    let mut on_anomaly = on_anomaly;
    let projected = project(history, on_anomaly.as_deref_mut())?;
    let deduped = dedupe_duplicate_tool_calls(projected, &mut on_anomaly);
    let merged = merge_consecutive_assistant_messages(deduped, &mut on_anomaly);
    Ok(drop_leading_non_user_messages(merged, &mut on_anomaly))
}

#[derive(Debug, Clone, Copy)]
struct SliceLayout {
    sizing: bool,
    last_non_tool_index: Option<usize>,
}

impl SliceLayout {
    fn is_trailing(&self, owner_index: usize) -> bool {
        self.last_non_tool_index
            .is_none_or(|last| owner_index >= last)
    }
}

fn slice_layout(history: &[ContextMessage]) -> SliceLayout {
    let mut sizing = true;
    let mut last_non_tool_index = None;
    for (index, message) in history.iter().enumerate() {
        if message.partial == Some(true) || message.role == Role::Tool {
            continue;
        }
        last_non_tool_index = Some(index);
        if message.role == Role::Assistant {
            sizing = false;
        }
    }
    SliceLayout {
        sizing,
        last_non_tool_index,
    }
}

#[derive(Debug, Clone)]
struct AttachedResult {
    source: ContextMessage,
    content: Vec<ContentPart>,
}

#[derive(Debug, Clone)]
enum PendingResult {
    Pending,
    Interrupted,
    Attached(AttachedResult),
}

#[derive(Debug, Clone)]
struct PendingCall {
    call_id: String,
    result: PendingResult,
    foreign_between: bool,
}

#[derive(Debug, Clone)]
struct Exchange {
    source: ContextMessage,
    content: Vec<ContentPart>,
    owner_index: usize,
    pending: Vec<PendingCall>,
}

#[derive(Debug, Clone)]
enum Block {
    Message {
        source: ContextMessage,
        content: Vec<ContentPart>,
    },
    Exchange(Exchange),
}

#[derive(Debug, Clone, Copy)]
struct OpenCall {
    exchange_index: usize,
    pending_index: usize,
}

fn exchange_mut(blocks: &mut [Block], index: usize) -> &mut Exchange {
    match &mut blocks[index] {
        Block::Exchange(exchange) => exchange,
        Block::Message { .. } => unreachable!(),
    }
}

fn pending_mut<'a>(blocks: &'a mut [Block], open: OpenCall) -> &'a mut PendingCall {
    &mut exchange_mut(blocks, open.exchange_index).pending[open.pending_index]
}

fn emit(on_anomaly: &mut Option<OnAnomaly<'_>>, anomaly: ProjectionAnomaly) {
    if let Some(callback) = on_anomaly.as_deref_mut() {
        callback(anomaly);
    }
}

fn mark_foreign_between(
    blocks: &mut [Block],
    open_calls: &HashMap<String, OpenCall>,
) {
    for open in open_calls.values().copied().collect::<Vec<_>>() {
        pending_mut(blocks, open).foreign_between = true;
    }
}

fn pair_blocks(
    history: &[ContextMessage],
    layout: &SliceLayout,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Result<Vec<Block>, WireAsyncError> {
    let mut blocks = Vec::new();
    let mut open_calls = HashMap::<String, OpenCall>::new();

    for (index, message) in history.iter().enumerate() {
        if message.partial == Some(true) {
            continue;
        }
        if message.role == Role::Tool && !layout.sizing {
            let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                continue;
            };
            let Some(open) = open_calls.get(tool_call_id).copied() else {
                mark_foreign_between(&mut blocks, &open_calls);
                emit(
                    on_anomaly,
                    ProjectionAnomaly::OrphanToolResultDropped {
                        tool_call_id: tool_call_id.to_string(),
                    },
                );
                continue;
            };
            open_calls.remove(tool_call_id);
            let content = projected_content(message, on_anomaly)?;
            let pending = pending_mut(&mut blocks, open);
            pending.result = PendingResult::Attached(AttachedResult {
                source: message.clone(),
                content,
            });
            if pending.foreign_between {
                emit(
                    on_anomaly,
                    ProjectionAnomaly::ToolResultReordered {
                        tool_call_id: tool_call_id.to_string(),
                    },
                );
            }
            continue;
        }

        let content = projected_content(message, on_anomaly)?;
        if message.tool_calls.is_empty() && !has_declared_tools(message) {
            if content.is_empty() {
                continue;
            }
            if content.iter().all(is_vacuous_content_part) {
                emit(
                    on_anomaly,
                    ProjectionAnomaly::VacuousMessageDropped {
                        role: role_name(message.role).to_string(),
                    },
                );
                continue;
            }
        }
        mark_foreign_between(&mut blocks, &open_calls);
        if message.tool_calls.is_empty() {
            blocks.push(Block::Message {
                source: message.clone(),
                content,
            });
            continue;
        }

        let exchange_index = blocks.len();
        blocks.push(Block::Exchange(Exchange {
            source: message.clone(),
            content,
            owner_index: index,
            pending: Vec::new(),
        }));
        for call in &message.tool_calls {
            if let Some(superseded) = open_calls.get(&call.id).copied() {
                let owner_index = exchange_mut(&mut blocks, superseded.exchange_index).owner_index;
                pending_mut(&mut blocks, superseded).result = PendingResult::Interrupted;
                emit(
                    on_anomaly,
                    ProjectionAnomaly::ToolResultSynthesized {
                        tool_call_id: call.id.clone(),
                        trailing: layout.is_trailing(owner_index),
                    },
                );
            }
            let pending_index = {
                let exchange = exchange_mut(&mut blocks, exchange_index);
                let pending_index = exchange.pending.len();
                exchange.pending.push(PendingCall {
                    call_id: call.id.clone(),
                    result: PendingResult::Pending,
                    foreign_between: false,
                });
                pending_index
            };
            open_calls.insert(
                call.id.clone(),
                OpenCall {
                    exchange_index,
                    pending_index,
                },
            );
        }
    }
    Ok(blocks)
}

#[derive(Default)]
struct MergeState {
    single: Option<AttachedResult>,
    texts: Vec<String>,
    parts: Vec<ContentPart>,
}

fn append_merge_content(group: &mut MergeState, content: Vec<ContentPart>) {
    let mut text = String::new();
    for part in content {
        match part {
            ContentPart::Text { text: part_text, .. } => text.push_str(&part_text),
            part => group.parts.push(part),
        }
    }
    if !text.is_empty() {
        group.texts.push(text);
    }
}

fn flush_merge(out: &mut Vec<Message>, merge: &mut Option<MergeState>) {
    let Some(merge) = merge.take() else {
        return;
    };
    if let Some(single) = merge.single {
        out.push(to_wire_message(&single.source, single.content));
        return;
    }
    let text = merge.texts.join("\n\n");
    let mut content = if text.is_empty() {
        Vec::new()
    } else {
        vec![ContentPart::Text {
            text,
            meta: None,
        }]
    };
    content.extend(merge.parts);
    out.push(Message {
        role: Role::User,
        name: None,
        content,
        tool_calls: Vec::new(),
        tool_call_id: None,
        partial: None,
        tools: None,
    });
}

fn flatten_blocks(
    blocks: Vec<Block>,
    layout: &SliceLayout,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Vec<Message> {
    let mut out = Vec::new();
    let mut merge = None::<MergeState>;

    for block in blocks {
        match block {
            Block::Message { source, content } => {
                if can_merge_user_message(&source) {
                    if merge.is_none() {
                        merge = Some(MergeState {
                            single: Some(AttachedResult { source, content }),
                            texts: Vec::new(),
                            parts: Vec::new(),
                        });
                    } else {
                        let group = merge.as_mut().unwrap();
                        if let Some(single) = group.single.take() {
                            append_merge_content(group, single.content);
                        }
                        append_merge_content(group, content);
                    }
                    continue;
                }
                flush_merge(&mut out, &mut merge);
                out.push(to_wire_message(&source, content));
            }
            Block::Exchange(exchange) => {
                flush_merge(&mut out, &mut merge);
                let Exchange {
                    source,
                    content,
                    owner_index,
                    pending,
                } = exchange;
                out.push(to_wire_message(&source, content));
                for pending in pending {
                    match pending.result {
                        PendingResult::Pending => {
                            out.push(create_interrupted_tool_result(&pending.call_id));
                            emit(
                                on_anomaly,
                                ProjectionAnomaly::ToolResultSynthesized {
                                    tool_call_id: pending.call_id,
                                    trailing: layout.is_trailing(owner_index),
                                },
                            );
                        }
                        PendingResult::Interrupted => {
                            out.push(create_interrupted_tool_result(&pending.call_id));
                        }
                        PendingResult::Attached(result) => {
                            out.push(to_wire_message(&result.source, result.content));
                        }
                    }
                }
            }
        }
    }
    flush_merge(&mut out, &mut merge);
    out
}

fn dedupe_duplicate_tool_calls(
    messages: Vec<Message>,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Vec<Message> {
    let mut seen_tool_call_ids = HashSet::new();
    let mut kept_tool_result_indexes = HashMap::<String, usize>::new();
    let mut out = Vec::<Message>::new();
    for mut message in messages {
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            let original_count = message.tool_calls.len();
            let mut kept = Vec::with_capacity(original_count);
            for tool_call in message.tool_calls.drain(..) {
                if seen_tool_call_ids.contains(&tool_call.id) {
                    emit(
                        on_anomaly,
                        ProjectionAnomaly::DuplicateToolCallDropped {
                            tool_call_id: tool_call.id,
                        },
                    );
                } else {
                    seen_tool_call_ids.insert(tool_call.id.clone());
                    kept.push(tool_call);
                }
            }
            let kept_count = kept.len();
            message.tool_calls = kept;
            if kept_count == original_count
                || kept_count > 0
                || !message.content.iter().all(is_vacuous_content_part)
            {
                out.push(message);
            } else if !message.content.is_empty() {
                emit(
                    on_anomaly,
                    ProjectionAnomaly::VacuousMessageDropped {
                        role: role_name(message.role).to_string(),
                    },
                );
            }
            continue;
        }
        if message.role == Role::Tool {
            if let Some(tool_call_id) = message.tool_call_id.clone() {
                if let Some(previous_index) = kept_tool_result_indexes.get(&tool_call_id).copied() {
                    if is_interrupted_tool_result(out.get(previous_index))
                        && !is_interrupted_tool_result(Some(&message))
                    {
                        out[previous_index] = message;
                    } else {
                        emit(
                            on_anomaly,
                            ProjectionAnomaly::DuplicateToolResultDropped { tool_call_id },
                        );
                    }
                    continue;
                }
                kept_tool_result_indexes.insert(tool_call_id, out.len());
            }
        }
        out.push(message);
    }
    out
}

fn merge_consecutive_assistant_messages(
    messages: Vec<Message>,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Vec<Message> {
    let mut out = Vec::<Message>::new();
    for message in messages {
        if out.last().is_some_and(|previous| {
            previous.role == Role::Assistant && message.role == Role::Assistant
        }) {
            let previous = out.last_mut().unwrap();
            previous.content.extend(message.content);
            previous.tool_calls.extend(message.tool_calls);
            emit(on_anomaly, ProjectionAnomaly::ConsecutiveAssistantsMerged);
            continue;
        }
        out.push(message);
    }
    out
}

fn drop_leading_non_user_messages(
    messages: Vec<Message>,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Vec<Message> {
    let mut start = 0;
    while start < messages.len() && messages[start].role != Role::User {
        emit(
            on_anomaly,
            ProjectionAnomaly::LeadingNonUserDropped {
                role: role_name(messages[start].role).to_string(),
            },
        );
        start += 1;
    }
    if start == 0 {
        messages
    } else {
        messages[start..].to_vec()
    }
}

fn projected_content(
    source: &ContextMessage,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Result<Vec<ContentPart>, WireAsyncError> {
    let content = if source.role == Role::Tool {
        render_tool_result_for_model(&RenderableToolResult {
            output: output_from_tool_content(&source.content),
            is_error: source.is_error,
            note: source.note.clone(),
            duration_ms: source.duration_ms,
        })
    } else {
        source.content.clone()
    };
    clean_content(source, content, on_anomaly)
}

fn clean_content(
    source: &ContextMessage,
    raw_content: Vec<ContentPart>,
    on_anomaly: &mut Option<OnAnomaly<'_>>,
) -> Result<Vec<ContentPart>, WireAsyncError> {
    let has_blank = raw_content.iter().any(is_blank_text);
    let mut content = raw_content;
    if has_blank {
        let mut filtered = Vec::new();
        for part in content {
            if is_blank_text(&part) {
                if matches!(&part, ContentPart::Text { text, .. } if !text.is_empty()) {
                    emit(
                        on_anomaly,
                        ProjectionAnomaly::WhitespaceTextDropped {
                            role: role_name(source.role).to_string(),
                        },
                    );
                }
            } else {
                filtered.push(part);
            }
        }
        content = filtered;
    }
    if source.role == Role::Tool && content.is_empty() {
        let mut details = Map::new();
        if let Some(tool_call_id) = &source.tool_call_id {
            details.insert(
                "toolCallId".to_string(),
                Value::String(tool_call_id.clone()),
            );
        }
        return Err(Box::new(Error2::with_options(
            REQUEST_INVALID,
            "Tool result message content cannot be empty after removing empty text blocks.",
            Error2Options {
                details: Some(details),
                ..Default::default()
            },
        )));
    }
    Ok(content)
}

fn output_from_tool_content(content: &[ContentPart]) -> RenderableToolOutput {
    if content.len() == 1 {
        if let ContentPart::Text { text, .. } = &content[0] {
            return RenderableToolOutput::Text(text.clone());
        }
    }
    RenderableToolOutput::Parts(content.to_vec())
}

const TOOL_INTERRUPTED_TEXT: &str = "Tool result is not available in the current context. Do not assume the tool completed successfully.";

fn create_interrupted_tool_result(tool_call_id: &str) -> Message {
    Message {
        role: Role::Tool,
        name: None,
        content: vec![ContentPart::Text {
            text: TOOL_INTERRUPTED_TEXT.to_string(),
            meta: None,
        }],
        tool_calls: Vec::new(),
        tool_call_id: Some(tool_call_id.to_string()),
        partial: None,
        tools: None,
    }
}

fn is_interrupted_tool_result(message: Option<&Message>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if message.role != Role::Tool {
        return false;
    }
    matches!(
        message.content.first(),
        Some(ContentPart::Text { text, .. }) if text == TOOL_INTERRUPTED_TEXT
    )
}

fn is_blank_text(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text, meta } => {
            let has_thought_signature = meta
                .as_ref()
                .and_then(|meta| meta.get("thought_signature_b64"))
                .is_some();
            !has_thought_signature && text.trim().is_empty()
        }
        _ => false,
    }
}

fn can_merge_user_message(message: &ContextMessage) -> bool {
    if message.role != Role::User {
        return false;
    }
    matches!(
        &message.origin,
        Some(PromptOrigin::User(origin))
            if origin.kind == "user" && origin.in_turn != Some(true)
    )
}

fn has_declared_tools(message: &ContextMessage) -> bool {
    message.tools.as_ref().is_some_and(|tools| !tools.is_empty())
}

fn to_wire_message(message: &ContextMessage, content: Vec<ContentPart>) -> Message {
    Message {
        role: message.role,
        name: message.name.clone(),
        content,
        tool_calls: message.tool_calls.clone(),
        tool_call_id: message.tool_call_id.clone(),
        partial: message.partial,
        tools: message.tools.clone(),
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}
