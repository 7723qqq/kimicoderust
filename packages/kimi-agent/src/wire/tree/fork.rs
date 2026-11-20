use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::sync::LazyLock;

use serde_json::{Map, Value, json};

use crate::agent::context_memory::conversation_time::{
    is_prompt_owned_injection, is_undo_anchor, is_valid_undo_count,
};
use crate::agent::context_memory::types::ContextMessage;
use crate::wire::record::WireRecord;

use super::tree::{AGENT_SWITCHED_TYPE, WireLine, WireTree, active_chain, parse_tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkLineFailure {
    CompactionBoundary,
    Insufficient,
}

impl ForkLineFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompactionBoundary => "compaction_boundary",
            Self::Insufficient => "insufficient",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkLineError {
    pub reason: ForkLineFailure,
    pub available: u64,
}

impl ForkLineError {
    pub fn new(reason: ForkLineFailure, available: u64) -> Self {
        Self { reason, available }
    }
}

impl fmt::Display for ForkLineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason.as_str())
    }
}

impl Error for ForkLineError {}

static COMPACTION_SUMMARY_MARKER: LazyLock<ContextMessage> = LazyLock::new(|| {
    serde_json::from_value(json!({
        "role": "user",
        "content": [],
        "toolCalls": [],
        "origin": { "kind": "compaction_summary" }
    }))
    .expect("compaction summary marker must deserialize")
});

fn is_context_message(value: &Value) -> bool {
    value.as_object().is_some_and(|message| {
        message.get("role").is_some_and(Value::is_string)
            && message.get("content").is_some_and(Value::is_array)
    })
}

fn context_message_of(value: &Value) -> Option<ContextMessage> {
    if !is_context_message(value) {
        return None;
    }
    serde_json::from_value(value.clone()).ok()
}

#[derive(Clone)]
struct NumberedMessage {
    message: ContextMessage,
    line: u64,
}

fn origin_kind(message: &ContextMessage) -> Option<&str> {
    message.origin.as_ref().map(|origin| origin.kind())
}

fn fold_unpaired_undo(messages: &mut Vec<NumberedMessage>, count: u64) {
    let mut removed = 0;
    for index in (0..messages.len()).rev() {
        let message = &messages[index].message;
        if origin_kind(message) == Some("injection") {
            continue;
        }
        if origin_kind(message) == Some("compaction_summary") {
            return;
        }
        if !is_undo_anchor(message) {
            continue;
        }
        removed += 1;
        if removed < count {
            continue;
        }
        let mut cut_index = index;
        while cut_index > 0
            && is_prompt_owned_injection(&messages[cut_index - 1].message, message)
        {
            cut_index -= 1;
        }
        messages.truncate(cut_index);
        return;
    }
}

pub fn compute_fork_line(
    chain: &[WireLine],
    paired_legacy_undo_lines: &HashSet<u64>,
    turns: u64,
) -> Result<u64, ForkLineError> {
    let mut clear_floor = 0;
    for WireLine { record, line } in chain {
        if record.get("type").and_then(Value::as_str) == Some("context.clear") {
            clear_floor = *line;
        }
    }
    let mut messages = Vec::new();
    for WireLine { record, line } in chain {
        if *line <= clear_floor {
            continue;
        }
        match record.get("type").and_then(Value::as_str) {
            Some("context.append_message") => {
                let Some(message) = record.get("message") else {
                    continue;
                };
                if let Some(parsed) = context_message_of(message) {
                    messages.push(NumberedMessage {
                        message: parsed,
                        line: *line,
                    });
                }
            }
            Some("context.apply_compaction") => messages.push(NumberedMessage {
                message: COMPACTION_SUMMARY_MARKER.clone(),
                line: *line,
            }),
            Some("context.undo") if !paired_legacy_undo_lines.contains(line) => {
                let count = record.get("count").and_then(Value::as_f64).unwrap_or(f64::NAN);
                if is_valid_undo_count(count) {
                    fold_unpaired_undo(&mut messages, count as u64);
                }
            }
            _ => {}
        }
    }

    let mut remaining = turns;
    let mut cut_index = None;
    for index in (0..messages.len()).rev() {
        if remaining == 0 {
            break;
        }
        let message = &messages[index].message;
        if origin_kind(message) == Some("injection") {
            continue;
        }
        if origin_kind(message) == Some("compaction_summary") {
            return Err(ForkLineError::new(
                ForkLineFailure::CompactionBoundary,
                turns - remaining,
            ));
        }
        if is_undo_anchor(message) {
            remaining -= 1;
            let mut anchor_index = index;
            while anchor_index > 0
                && is_prompt_owned_injection(&messages[anchor_index - 1].message, message)
            {
                anchor_index -= 1;
            }
            cut_index = Some(anchor_index);
        }
    }
    let Some(cut_index) = cut_index.filter(|_| remaining == 0) else {
        return Err(ForkLineError::new(
            ForkLineFailure::Insufficient,
            turns - remaining,
        ));
    };
    Ok(messages[cut_index].line - 1)
}

pub fn restorable_chain(
    entries: &[WireLine],
    tree: &WireTree,
) -> Result<Vec<WireLine>, Box<dyn Error + Send + Sync>> {
    let chain: Vec<WireLine> = active_chain(entries, tree)?
        .into_iter()
        .filter(|WireLine { record, line }| {
            let record_type = record.get("type").and_then(Value::as_str);
            record_type != Some("context.undone")
                && !(record_type == Some("context.undo")
                    && tree.paired_legacy_undo_lines.contains(line))
        })
        .collect();
    let chain_lines: HashSet<u64> = chain.iter().map(|entry| entry.line).collect();
    let mut surviving_prompt_ids = Vec::new();
    for WireLine { record, .. } in &chain {
        if record.get("type").and_then(Value::as_str) != Some("context.append_message") {
            continue;
        }
        let Some(message) = record.get("message") else {
            continue;
        };
        let Some(message) = context_message_of(message) else {
            continue;
        };
        let Some(id) = message.id.clone() else {
            continue;
        };
        if is_undo_anchor(&message) {
            surviving_prompt_ids.push(Value::String(id));
        }
    }
    let mut re_included = Vec::new();
    for entry in entries {
        if chain_lines.contains(&entry.line)
            || entry.record.get("type").and_then(Value::as_str)
                != Some("context.append_message")
        {
            continue;
        }
        let Some(message) = entry.record.get("message") else {
            continue;
        };
        if !is_context_message(message) {
            continue;
        }
        let Some(origin) = message.get("origin") else {
            continue;
        };
        if origin.get("kind").and_then(Value::as_str) != Some("injection") {
            continue;
        }
        let include = match origin.get("ownerPromptId") {
            None => true,
            Some(owner) => surviving_prompt_ids.contains(owner),
        };
        if include {
            re_included.push(entry.clone());
        }
    }
    let mut out = chain;
    out.extend(re_included);
    out.sort_by_key(|entry| entry.line);
    Ok(out)
}

pub fn flatten_chain(
    records: &[WireRecord],
) -> Result<Vec<WireRecord>, Box<dyn Error + Send + Sync>> {
    let entries: Vec<WireLine> = records
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, record)| WireLine {
            record,
            line: index as u64 + 1,
        })
        .collect();
    let tree = parse_tree(&entries, entries.len() as u64);
    Ok(restorable_chain(&entries, &tree)?
        .into_iter()
        .map(|entry| entry.record)
        .collect())
}

#[derive(Debug, Clone, PartialEq)]
pub struct UndoSwitchRecords {
    pub switched: WireRecord,
    pub legacy_undo: WireRecord,
    pub undone: WireRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildUndoSwitchRecordsInput {
    pub agent_id: String,
    pub branch: String,
    pub reason: String,
    pub base: super::tree::BranchBase,
    pub turns: u64,
    pub edge_line: u64,
    pub from_turn_id: Option<u64>,
    pub time: i64,
}

pub fn build_undo_switch_records(input: BuildUndoSwitchRecordsInput) -> UndoSwitchRecords {
    let BuildUndoSwitchRecordsInput {
        agent_id,
        branch,
        reason,
        base,
        turns,
        edge_line,
        from_turn_id,
        time,
    } = input;
    let switched = json!({
        "type": AGENT_SWITCHED_TYPE,
        "agentId": agent_id,
        "branch": branch,
        "reason": reason,
        "base": { "branch": base.branch, "line": base.line },
        "turns": turns,
        "legacyUndoLine": edge_line + 1,
        "time": time,
    })
    .as_object()
    .cloned()
    .unwrap();
    let legacy_undo = json!({
        "type": "context.undo",
        "agentId": agent_id,
        "count": turns,
        "time": time,
    })
    .as_object()
    .cloned()
    .unwrap();
    let mut undone = Map::new();
    undone.insert("type".to_string(), "context.undone".into());
    undone.insert("agentId".to_string(), agent_id.into());
    undone.insert("turns".to_string(), turns.into());
    if let Some(from_turn_id) = from_turn_id {
        undone.insert("fromTurnId".to_string(), from_turn_id.into());
    }
    undone.insert("time".to_string(), time.into());
    UndoSwitchRecords {
        switched,
        legacy_undo,
        undone,
    }
}
