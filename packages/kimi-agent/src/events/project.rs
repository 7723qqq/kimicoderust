//! Projection layer: `EngineEvent` → `Vec<EntryOp>` (design
//! `reports/design/RUST-UI-DECOUPLING.md` §1.1, batching addendum §8).
//!
//! A pure state machine — no IO, no clock, no terminal. Same events in ⇒ same
//! ops out, which is what makes recorded-stream replay a valid acceptance
//! harness.
//!
//! ## Relationship to the transcript projector (deliberate divergence, audited
//! 2026-11-04)
//!
//! `server/transcript/project.rs` also consumes `EngineEvent`, into the
//! `TranscriptOperation` wire the REST/WS clients render. Two consumers, two
//! output contracts — merging them is wrong. What must not diverge is the
//! *interpretation* of a shared event; the 2026-11-04 audit of all twelve
//! events both handle found exactly one drift, fixed here (`tool.native`'s
//! `is_error` was dropped, so a failed call looked like a success). The shared
//! decisions live in `kimi_ui_core` (`classify_delta_part` for what a raw
//! `llm.delta` part is; the projection classification tables) and are gated by
//! `bun run check:event-coverage`.
//!
//! Recorded as *decisions*, not gaps:
//!
//! - **`llm.delta`**: the transcript projector does not handle it. The WS
//!   clients render `assistant.delta` / `stream.segment` (the typed deltas);
//!   `llm.delta` is the raw napi/stdio transport shape and reaches only this
//!   projector. Both names it as `Entry` in the classification table, so the
//!   gate would fail if either projector stopped being reachable.
//! - **`tool.call.completed`**: the transcript fold keeps `result` structured
//!   (`frame.output = Some(result)`); this fold extracts display text
//!   (`tool_result_text`) because an `EntryOp` body is a string. Same event,
//!   same input, different carrier — the extraction rules are the only thing
//!   that could drift, and the slice test pins them.
//! - **`tool.call.started` without a prior announce**: the transcript
//!   projection *invents* a frame (a snapshot catch-up may see the completion
//!   first); this fold drops it (an engine-ordering bug must not become an
//!   entry). Different contracts, different failure posture.
//! - **`turn.started`**: both create the turn; the transcript one also
//!   re-anchors its cursor for steers, which `EntryOp`'s linear stream has no
//!   notion of.
//!
//! ## Coalescing
//!
//! Raw per-token deltas are merged engine-side, here in the projector (this
//! crate *is* engine-side), per `CoalescingPolicy`: a pending buffer per entry
//! stream flushes when it exceeds `max_pending_bytes` or at a boundary (step
//! end, stream switch, turn end). The time-window trigger
//! (`flush_interval`) belongs to the drive/emission layer and is out of scope
//! for a pure function.
//!
//! ## Spans
//!
//! `SeqSpan` ordinals are stream-local: each entry's deltas are numbered from
//! 0 by the projector, so interleaved thinking/assistant streams never
//! false-positive the store's gap check. When emission-side stamping lands
//! (phase 2, `stream.segment`), spans arrive on the event and pass through
//! unchanged.
//!
//! ## Not projected (deliberate)
//!
//! Transport/accounting events produce no entries: `llm.step.begin`,
//! `llm.step.end` (see `UiVisibility::TransportOnly`), `event.session.*`,
//! `session.meta.updated`, `event.config.changed`, `cron.fired`,
//! `goal.budget.limit_reached`, `subagent.*`. Session/meta chrome (title bar,
//! status line) is host state, not transcript structure; the list-structure
//! coverage of this layer is turn/tool lifecycle (`turn.started`,
//! `tool.call.*`, `turn.ended`). `llm.step.end` still triggers a boundary
//! flush — that is bookkeeping, not projection of the event.

use std::collections::{BTreeMap, HashMap};

use kimi_ui_core::{
    AppendDelta, CoalescingPolicy, EntryContent, EntryId, EntryIdAllocator, EntryKind, EntryOp,
    SeqSpan, StreamKind, delta_part_text,
};

use super::types::EngineEvent;
use serde_json::Value;

/// Buffered text of one open entry stream.
///
/// `first` is the stream-local ordinal of the first buffered delta; `count`
/// deltas are buffered, so the flushed span is `[first, first + count - 1]`.
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

/// Per-turn projection state.
#[derive(Debug)]
struct TurnState {
    alloc: EntryIdAllocator,
    assistant: Option<EntryId>,
    thinking: Option<EntryId>,
    /// tool_call_id → (entry, tool name). The name is kept so the snapshot
    /// fold can keep it visible above the folded result.
    tools: HashMap<String, (EntryId, String)>,
    /// Open text streams keyed by entry, so interleaved streams coalesce
    /// independently. BTreeMap: flush order must be deterministic.
    pendings: BTreeMap<EntryId, Pending>,
    /// Stream-local ordinal the next delta of each entry takes.
    ordinals: BTreeMap<EntryId, u64>,
    /// subagent_id → (the entry that member's card lives on, the name to show).
    /// The name is kept because a completion event carries only the result:
    /// without it the card would settle to "done: <summary>" and lose which
    /// member produced it. A member is an
    /// entry of its own: it has its own lifecycle and its own output, and
    /// folding it into the parent's text is how a swarm becomes one long
    /// paragraph with no way to tell which member said what.
    subagents: HashMap<String, (EntryId, String)>,
}

impl TurnState {
    fn new(turn_id: String) -> Self {
        Self {
            alloc: EntryIdAllocator::engine(turn_id),
            assistant: None,
            thinking: None,
            tools: HashMap::new(),
            pendings: BTreeMap::new(),
            ordinals: BTreeMap::new(),
            subagents: HashMap::new(),
        }
    }
}

/// The projection state machine. One instance spans a session; turn state is
/// reset per `turn.started`.
#[derive(Debug)]
pub struct Projector {
    policy: CoalescingPolicy,
    turn: Option<TurnState>,
}

impl Projector {
    pub fn new(policy: CoalescingPolicy) -> Self {
        Self { policy, turn: None }
    }

    /// Project one event. Takes the event by value so large payloads (tool
    /// results) move into the store exactly once — no copies at the seam.
    pub fn project(&mut self, event: EngineEvent) -> Vec<EntryOp> {
        match event {
            EngineEvent::TurnStarted {
                turn_id, prompt, ..
            } => {
                self.turn = Some(TurnState::new(turn_id));
                let mut ops = Vec::new();
                if let Some(prompt) = prompt {
                    let (id, created) = self.entry_or_create(EntryKind::UserMsg, None);
                    debug_assert!(created);
                    ops.push(EntryOp::Upsert {
                        id: id.clone(),
                        content: EntryContent::new(EntryKind::UserMsg, prompt),
                    });
                    ops.push(EntryOp::Seal { id });
                }
                ops
            }
            EngineEvent::AssistantDelta { delta, .. } => {
                self.text_delta(EntryKind::AssistantMsg, &delta)
            }
            EngineEvent::ThinkingDelta { delta, .. } => {
                self.text_delta(EntryKind::Thinking, &delta)
            }
            EngineEvent::LlmDelta { part, .. } => {
                // The raw transport shape: `{ "type": "text", "text": … }` or
                // `{ "type": "think", "think": … }` (+ provider aliases). What
                // a part *is* is one shared answer (kimi_ui_core), so the
                // transcript projection cannot disagree with this one by
                // omission.
                match delta_part_text(&part) {
                    Some(("assistant", text)) => self.text_delta(EntryKind::AssistantMsg, text),
                    Some(("thinking", think)) => self.text_delta(EntryKind::Thinking, think),
                    _ => Vec::new(),
                }
            }
            EngineEvent::ToolCallStarted {
                tool_call_id, name, ..
            } => {
                let mut ops = self.flush_all(false);
                let (id, _) = self.entry_or_create(EntryKind::ToolCall, Some(&tool_call_id));
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
                let (id, created) = self.entry_or_create(EntryKind::ToolCall, Some(&tool_call_id));
                self.buffer(id, &part, created)
            }
            EngineEvent::ToolNative {
                tool_call_id,
                content,
                is_error,
                ..
            } => {
                // The delta → snapshot fold: flush the argument stream, then
                // replace the entry wholesale with the full result. A native
                // result without a matching `tool.call.started` is an engine
                // ordering bug; the fold drops it rather than invent an entry.
                let Some(id) = self.tool_entry(&tool_call_id) else {
                    return Vec::new();
                };
                let mut ops = self.flush(&id, true);
                let label = self.tool_label(&tool_call_id);
                // `EntryOp` has no error arm: the transcript projection keeps
                // `is_error` structurally (`frame.state = Error`), this one can
                // only say it in text. Dropping the flag entirely (the previous
                // behaviour) made a failed native tool call indistinguishable
                // from a successful one in the entry stream.
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
                // The completion carries the tool's own output. Sealing without
                // folding it left the card showing only the tool's name: the
                // user could see that Read ran, never what it found.
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
                stream,
                text,
                span,
                is_final,
                ..
            } => {
                // The reserved batched wire shape: spans arrive engine-stamped
                // and pass straight through; the projector adds only the entry
                // mapping.
                let (kind, tool_key) = match &stream {
                    StreamKind::Assistant => (EntryKind::AssistantMsg, None),
                    StreamKind::Thinking => (EntryKind::Thinking, None),
                    StreamKind::ToolCall { tool_call_id } => {
                        (EntryKind::ToolCall, Some(tool_call_id.clone()))
                    }
                };
                let (id, created) = self.entry_or_create(kind, tool_key.as_deref());
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
                    for id in [&turn.thinking, &turn.assistant].into_iter().flatten() {
                        ops.push(EntryOp::Seal { id: id.clone() });
                    }
                }
                ops.push(EntryOp::Barrier);
                ops
            }
            // Boundary flush only: step end is transport-only, never an entry.
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
                // A member's card is created once and keeps its identity for
                // the member's whole life, so completion and failure update the
                // same entry rather than appending new ones.
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

    /// Resolve the entry for a stream, creating it on first use. Returns the
    /// id and whether it was created now. Without an active turn, falls back
    /// to a throwaway UI-local id: deltas before `turn.started` are an engine
    /// ordering bug and must not crash the projection.
    fn entry_or_create(&mut self, kind: EntryKind, tool_key: Option<&str>) -> (EntryId, bool) {
        let Some(turn) = self.turn.as_mut() else {
            return (EntryId::ui_local(0, kind), false);
        };
        let existing = match kind {
            EntryKind::AssistantMsg => turn.assistant.clone(),
            EntryKind::Thinking => turn.thinking.clone(),
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
            EntryKind::AssistantMsg => turn.assistant = Some(id.clone()),
            EntryKind::Thinking => turn.thinking = Some(id.clone()),
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

    /// Fold a member's terminal state onto the card its spawn created.
    ///
    /// A member that never announced itself has no card: inventing one here
    /// would put a result on screen with no context for what produced it, so
    /// the terminal event is dropped exactly like a tool result without a
    /// `tool.call.started`.
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

    /// Buffer a text delta onto a stream, flushing when the byte cap trips.
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

    fn text_delta(&mut self, kind: EntryKind, text: &str) -> Vec<EntryOp> {
        let (id, created) = self.entry_or_create(kind, None);
        if self.turn.is_none() {
            return Vec::new();
        }
        self.buffer(id, text, created)
    }

    /// Flush one stream, if it has buffered bytes.
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

    /// Flush every open stream in deterministic (entry id) order.
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

/// The text a `tool.call.completed` result carries.
///
/// The wire shape is a JSON value because tools return different things: most
/// send `{ content: "..." }`, an image read sends a descriptor, an MCP tool
/// sends the server's own envelope. A result with no textual field renders as
/// no line rather than as a raw JSON dump — putting a serialized envelope in
/// front of the user is worse than showing nothing, and the structured form
/// arrives with the tool-card slice.
fn tool_result_text(result: &Value) -> Option<String> {
    let direct = result
        .get("content")
        .or_else(|| result.get("text"))
        .or_else(|| result.get("output"));
    if let Some(Value::String(text)) = direct {
        return Some(text.clone());
    }
    // An array of content blocks (`[{type:"text", text:"..."}]`) is the other
    // shape the wire uses; concatenate the text blocks.
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
