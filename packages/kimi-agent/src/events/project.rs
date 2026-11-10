use std::collections::{BTreeMap, HashMap};

use kimi_ui_core::{
    AppendDelta, CoalescingPolicy, EntryContent, EntryId, EntryIdAllocator, EntryKind, EntryOp,
    SeqSpan, StreamKind, delta_part_text,
};

use super::types::EngineEvent;
use serde_json::Value;

#[derive(Debug, Default)]
struct Pending {
    text: String,
    first: u64,
    count: u64,
}

impl Pending {
    fn push(&mut self, text: &str, ordinal: u64) {
        if self.count == 0 {
            self.first = ordinal;
        }
        self.text.push_str(text);
        self.count += 1;
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn take(&mut self, is_final: bool) -> AppendDelta {
        let delta = AppendDelta {
            text: std::mem::take(&mut self.text),
            span: SeqSpan::new(self.first, self.first + self.count - 1),
            is_final,
        };
        self.count = 0;
        delta
    }
}

#[derive(Debug)]
struct TurnState {
    turn_id: String,
    alloc: EntryIdAllocator,
    streams: BTreeMap<(String, EntryKind), EntryId>,
    tools: HashMap<String, (EntryId, String)>,
    pendings: BTreeMap<EntryId, Pending>,
    ordinals: BTreeMap<EntryId, u64>,
    subagents: HashMap<String, (EntryId, String)>,
}

impl TurnState {
    fn new(turn_id: String) -> Self {
        Self {
            alloc: EntryIdAllocator::engine(turn_id.clone()),
            turn_id,
            streams: BTreeMap::new(),
            tools: HashMap::new(),
            pendings: BTreeMap::new(),
            ordinals: BTreeMap::new(),
            subagents: HashMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct Projector {
    policy: CoalescingPolicy,
    turn: Option<TurnState>,
}

impl Projector {
    pub fn new(policy: CoalescingPolicy) -> Self {
        Self { policy, turn: None }
    }

    pub fn project(&mut self, event: EngineEvent) -> Vec<EntryOp> {
        match event {
            EngineEvent::TurnStarted {
                turn_id, prompt, ..
            } => {
                self.turn = Some(TurnState::new(turn_id));
                let mut ops = Vec::new();
                if let Some(prompt) = prompt {
                    let (id, created) = self.entry_or_create(
                        EntryKind::UserMsg,
                        crate::callbacks::MAIN_AGENT_ID,
                        None,
                    );
                    debug_assert!(created);
                    ops.push(EntryOp::Upsert {
                        id: id.clone(),
                        content: EntryContent::new(EntryKind::UserMsg, prompt),
                    });
                    ops.push(EntryOp::Seal { id });
                }
                ops
            }
            EngineEvent::AssistantDelta {
                agent_id, delta, ..
            } => self.text_delta(EntryKind::AssistantMsg, &agent_id, &delta),
            EngineEvent::ThinkingDelta {
                agent_id, delta, ..
            } => self.text_delta(EntryKind::Thinking, &agent_id, &delta),
            EngineEvent::LlmDelta { agent_id, part, .. } => {
                if crate::events::is_hidden_delta_part(&part) {
                    return Vec::new();
                }
                match delta_part_text(&part) {
                    Some(("assistant", text)) => {
                        self.text_delta(EntryKind::AssistantMsg, &agent_id, text)
                    }
                    Some(("thinking", think)) => {
                        self.text_delta(EntryKind::Thinking, &agent_id, think)
                    }
                    _ => Vec::new(),
                }
            }
            EngineEvent::ToolCallStarted {
                tool_call_id, name, ..
            } => {
                let mut ops = self.flush_all(false);
                let (id, _) = self.entry_or_create(
                    EntryKind::ToolCall,
                    crate::callbacks::MAIN_AGENT_ID,
                    Some(&tool_call_id),
                );
                if let Some(turn) = self.turn.as_mut() {
                    turn.tools.insert(tool_call_id, (id.clone(), name.clone()));
                }
                ops.push(EntryOp::Upsert {
                    id,
                    content: EntryContent::new(EntryKind::ToolCall, name),
                });
                ops
            }
            EngineEvent::ToolCallDelta {
                tool_call_id,
                arguments_part,
                ..
            } => {
                let Some(part) = arguments_part else {
                    return Vec::new();
                };
                let (id, created) = self.entry_or_create(
                    EntryKind::ToolCall,
                    crate::callbacks::MAIN_AGENT_ID,
                    Some(&tool_call_id),
                );
                self.buffer(id, &part, created)
            }
            EngineEvent::ToolNative {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                let Some(id) = self.tool_entry(&tool_call_id) else {
                    return Vec::new();
                };
                let mut ops = self.flush(&id, true);
                let label = self.tool_label(&tool_call_id);
                let body = if is_error {
                    format!("{label}\nerror: {content}")
                } else {
                    format!("{label}\n{content}")
                };
                ops.push(EntryOp::Upsert {
                    id,
                    content: EntryContent::new(EntryKind::ToolCall, body),
                });
                ops
            }
            EngineEvent::ToolCallCompleted {
                tool_call_id,
                result,
                ..
            } => {
                let Some(id) = self.tool_entry(&tool_call_id) else {
                    return Vec::new();
                };
                let mut ops = self.flush(&id, true);
                if let Some(text) = tool_result_text(&result) {
                    let label = self.tool_label(&tool_call_id);
                    ops.push(EntryOp::Upsert {
                        id: id.clone(),
                        content: EntryContent::new(
                            EntryKind::ToolCall,
                            format!(
                                "{label}
{text}"
                            ),
                        ),
                    });
                }
                ops.push(EntryOp::Seal { id });
                ops
            }
            EngineEvent::ToolCallFailed {
                tool_call_id,
                error,
                ..
            } => {
                let Some(id) = self.tool_entry(&tool_call_id) else {
                    return Vec::new();
                };
                let mut ops = self.flush(&id, true);
                let label = self.tool_label(&tool_call_id);
                ops.push(EntryOp::Upsert {
                    id: id.clone(),
                    content: EntryContent::new(EntryKind::ToolCall, format!("{label}\n{error}")),
                });
                ops.push(EntryOp::Seal { id });
                ops
            }
            EngineEvent::StreamSegment {
                agent_id,
                stream,
                text,
                span,
                is_final,
                ..
            } => {
                let (kind, tool_key) = match &stream {
                    StreamKind::Assistant => (EntryKind::AssistantMsg, None),
                    StreamKind::Thinking => (EntryKind::Thinking, None),
                    StreamKind::ToolCall { tool_call_id } => {
                        (EntryKind::ToolCall, Some(tool_call_id.clone()))
                    }
                };
                let (id, created) = self.entry_or_create(kind, &agent_id, tool_key.as_deref());
                let mut ops = Vec::new();
                if created {
                    ops.push(EntryOp::Upsert {
                        id: id.clone(),
                        content: EntryContent::new(id.kind, String::new()),
                    });
                }
                ops.push(EntryOp::Append {
                    id,
                    delta: AppendDelta {
                        text,
                        span,
                        is_final,
                    },
                });
                ops
            }
            EngineEvent::TurnEnded { .. } => {
                let mut ops = self.flush_all(true);
                if let Some(turn) = &self.turn {
                    let flushed: std::collections::BTreeSet<EntryId> = ops
                        .iter()
                        .filter_map(|op| match op {
                            EntryOp::Append { id, delta } if delta.is_final => Some(id.clone()),
                            _ => None,
                        })
                        .collect();
                    for id in turn.streams.values() {
                        if !flushed.contains(id) && turn.pendings.contains_key(id) {
                            let next = turn.ordinals.get(id).copied().unwrap_or(0);
                            ops.push(EntryOp::Append {
                                id: id.clone(),
                                delta: AppendDelta {
                                    text: String::new(),
                                    span: SeqSpan::new(next, next),
                                    is_final: true,
                                },
                            });
                        }
                        ops.push(EntryOp::Seal { id: id.clone() });
                    }
                }
                ops.push(EntryOp::Barrier);
                ops
            }
            EngineEvent::LlmStepEnd { .. } => self.flush_all(false),
            EngineEvent::SubagentSpawned {
                subagent_id,
                subagent_name,
                description,
                ..
            } => {
                let Some(turn) = self.turn.as_mut() else {
                    return Vec::new();
                };
                if turn.subagents.contains_key(&subagent_id) {
                    return Vec::new();
                }
                let id = turn.alloc.alloc(EntryKind::Status);
                let label = subagent_name
                    .or_else(|| description.clone())
                    .unwrap_or_else(|| subagent_id.clone());
                turn.subagents
                    .insert(subagent_id.clone(), (id.clone(), label.clone()));
                vec![EntryOp::Upsert {
                    id,
                    content: EntryContent::new(EntryKind::Status, format!("[subagent] {label}")),
                }]
            }
            EngineEvent::SubagentCompleted {
                subagent_id,
                result_summary,
                ..
            } => self.settle_subagent(&subagent_id, result_summary.as_deref(), None),
            EngineEvent::SubagentFailed { subagent_id, error } => {
                self.settle_subagent(&subagent_id, None, Some(&error))
            }
            _ => Vec::new(),
        }
    }

    pub fn note_user_entry(
        &mut self,
        turn_id: &str,
        text: impl Into<String>,
    ) -> Option<(EntryId, Vec<EntryOp>)> {
        if self.turn.as_ref()?.turn_id != turn_id {
            return None;
        }
        let mut ops = self.flush_all(true);
        let turn = self.turn.as_mut()?;
        turn.streams.clear();
        turn.pendings.clear();
        let id = turn.alloc.alloc(EntryKind::UserMsg);
        ops.push(EntryOp::Upsert {
            id: id.clone(),
            content: EntryContent::new(EntryKind::UserMsg, text),
        });
        Some((id, ops))
    }

    fn entry_or_create(
        &mut self,
        kind: EntryKind,
        agent_id: &str,
        tool_key: Option<&str>,
    ) -> (EntryId, bool) {
        let Some(turn) = self.turn.as_mut() else {
            return (EntryId::ui_local(0, kind), false);
        };
        let stream_key = (agent_id.to_owned(), kind);
        let existing = match kind {
            EntryKind::AssistantMsg | EntryKind::Thinking => turn.streams.get(&stream_key).cloned(),
            EntryKind::ToolCall => {
                tool_key.and_then(|key| turn.tools.get(key).map(|(id, _)| id.clone()))
            }
            _ => None,
        };
        if let Some(id) = existing {
            return (id, false);
        }
        let id = turn.alloc.alloc(kind);
        match kind {
            EntryKind::AssistantMsg | EntryKind::Thinking => {
                turn.streams.insert(stream_key, id.clone());
            }
            EntryKind::ToolCall => {
                if let Some(key) = tool_key {
                    turn.tools
                        .insert(key.to_owned(), (id.clone(), String::new()));
                }
            }
            _ => {}
        }
        (id, true)
    }

    fn settle_subagent(
        &mut self,
        subagent_id: &str,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Vec<EntryOp> {
        let Some(turn) = self.turn.as_mut() else {
            return Vec::new();
        };
        let Some((id, label)) = turn.subagents.get(subagent_id).cloned() else {
            return Vec::new();
        };
        let body = match (summary, error) {
            (_, Some(error)) => format!("[subagent] {label} failed: {error}"),
            (Some(summary), None) => format!("[subagent] {label} done: {summary}"),
            (None, None) => format!("[subagent] {label} done"),
        };
        vec![
            EntryOp::Upsert {
                id: id.clone(),
                content: EntryContent::new(EntryKind::Status, body),
            },
            EntryOp::Seal { id },
        ]
    }

    fn tool_entry(&self, tool_call_id: &str) -> Option<EntryId> {
        self.turn
            .as_ref()
            .and_then(|t| t.tools.get(tool_call_id).map(|(id, _)| id.clone()))
    }

    fn tool_label(&self, tool_call_id: &str) -> String {
        self.turn
            .as_ref()
            .and_then(|t| t.tools.get(tool_call_id).map(|(_, name)| name.clone()))
            .unwrap_or_default()
    }

    fn buffer(&mut self, id: EntryId, text: &str, created: bool) -> Vec<EntryOp> {
        let mut ops = Vec::new();
        if created {
            ops.push(EntryOp::Upsert {
                id: id.clone(),
                content: EntryContent::new(id.kind, String::new()),
            });
        }
        let turn = self.turn.as_mut().expect("checked by entry_or_create");
        let ordinal = turn.ordinals.get(&id).copied().unwrap_or(0);
        turn.ordinals.insert(id.clone(), ordinal + 1);
        let pending = turn.pendings.entry(id.clone()).or_default();
        pending.push(text, ordinal);
        if pending.text.len() >= self.policy.max_pending_bytes {
            ops.push(EntryOp::Append {
                id,
                delta: pending.take(false),
            });
        }
        ops
    }

    fn text_delta(&mut self, kind: EntryKind, agent_id: &str, text: &str) -> Vec<EntryOp> {
        let (id, created) = self.entry_or_create(kind, agent_id, None);
        if self.turn.is_none() {
            return Vec::new();
        }
        let mut ops = Vec::new();
        if created {
            let mut content = EntryContent::new(id.kind, String::new());
            content.agent_id =
                (agent_id != crate::callbacks::MAIN_AGENT_ID).then(|| agent_id.to_owned());
            ops.push(EntryOp::Upsert {
                id: id.clone(),
                content,
            });
        }
        ops.extend(self.buffer(id, text, false));
        ops
    }

    fn flush(&mut self, id: &EntryId, is_final: bool) -> Vec<EntryOp> {
        let Some(turn) = self.turn.as_mut() else {
            return Vec::new();
        };
        let Some(pending) = turn.pendings.get_mut(id) else {
            return Vec::new();
        };
        if pending.is_empty() {
            return Vec::new();
        }
        vec![EntryOp::Append {
            id: id.clone(),
            delta: pending.take(is_final),
        }]
    }

    fn flush_all(&mut self, is_final: bool) -> Vec<EntryOp> {
        let Some(turn) = self.turn.as_mut() else {
            return Vec::new();
        };
        let ids: Vec<EntryId> = turn.pendings.keys().cloned().collect();
        let mut ops = Vec::new();
        for id in ids {
            let pending = turn.pendings.get_mut(&id).expect("key collected above");
            if pending.is_empty() {
                continue;
            }
            ops.push(EntryOp::Append {
                id,
                delta: pending.take(is_final),
            });
        }
        ops
    }
}

impl Default for Projector {
    fn default() -> Self {
        Self::new(CoalescingPolicy::default())
    }
}

fn tool_result_text(result: &Value) -> Option<String> {
    let direct = result
        .get("content")
        .or_else(|| result.get("text"))
        .or_else(|| result.get("output"));
    if let Some(Value::String(text)) = direct {
        return Some(text.clone());
    }
    if let Some(Value::Array(blocks)) = result.get("content") {
        let joined: String = blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EngineEvent;
    use serde_json::json;

    #[test]
    fn hidden_thinking_delta_projects_nothing() {
        let mut projector = Projector::new(CoalescingPolicy::default());
        let turn_id = "turn-hidden";
        projector.project(EngineEvent::TurnStarted {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: turn_id.into(),
            prompt: None,
        });

        let hidden = projector.project(EngineEvent::LlmDelta {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: turn_id.into(),
            step: 1,
            part: json!({ "type": "think", "think": "secret summary", "hidden": true }),
        });
        assert!(
            hidden.is_empty(),
            "hidden thinking must not become a display entry"
        );

        let visible = projector.project(EngineEvent::LlmDelta {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: turn_id.into(),
            step: 1,
            part: json!({ "type": "think", "think": "shown" }),
        });
        assert!(
            !visible.is_empty(),
            "a visible thinking delta still opens its entry"
        );
    }
}
