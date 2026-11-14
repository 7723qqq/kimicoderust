use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

pub(crate) const MAIN_BRANCH: &str = "main";
const AGENT_SWITCHED_TYPE: &str = "agent.switched";

#[derive(Debug, Clone)]
pub(crate) struct WireLine {
    pub record: Value,
    pub line: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SwitchEdge {
    line: usize,
    branch: String,
    base: (String, usize),
    legacy_undo_line: Option<usize>,
}

#[derive(Debug, Clone)]
struct TreeSegment {
    branch: String,
    edge: Option<SwitchEdge>,
    from_line: usize,
    to_line: usize,
}

pub(crate) struct TreeDiagnostics {
    pub malformed_switch_lines: Vec<usize>,
    pub duplicate_branches: Vec<String>,
}

pub(crate) struct WireTree {
    pub edges: Vec<SwitchEdge>,
    segments: Vec<TreeSegment>,
    pub paired_legacy_undo_lines: HashSet<usize>,
    pub active_branch: String,
    pub diagnostics: TreeDiagnostics,
}

pub(crate) fn entries_from_records(records: &[Value]) -> Vec<WireLine> {
    records
        .iter()
        .enumerate()
        .map(|(index, record)| WireLine {
            record: record.clone(),
            line: index + 1,
        })
        .collect()
}

fn read_switch_edge(record: &Value, line: usize) -> Option<SwitchEdge> {
    if record.get("type").and_then(Value::as_str) != Some(AGENT_SWITCHED_TYPE) {
        return None;
    }
    let branch = record.get("branch").and_then(Value::as_str)?.to_string();
    let base = record.get("base").and_then(Value::as_object)?;
    let base_branch = base.get("branch").and_then(Value::as_str)?.to_string();
    let base_line = base.get("line").and_then(Value::as_u64)? as usize;
    Some(SwitchEdge {
        line,
        branch,
        base: (base_branch, base_line),
        legacy_undo_line: record
            .get("legacyUndoLine")
            .and_then(Value::as_u64)
            .map(|line| line as usize),
    })
}

pub(crate) fn parse_tree(entries: &[WireLine], last_line: usize) -> WireTree {
    let mut edges = Vec::new();
    let mut paired_legacy_undo_lines = HashSet::new();
    let mut malformed_switch_lines = Vec::new();
    for entry in entries {
        if entry.record.get("type").and_then(Value::as_str) != Some(AGENT_SWITCHED_TYPE) {
            continue;
        }
        let Some(edge) = read_switch_edge(&entry.record, entry.line) else {
            malformed_switch_lines.push(entry.line);
            continue;
        };
        if let Some(line) = edge.legacy_undo_line {
            paired_legacy_undo_lines.insert(line);
        }
        edges.push(edge);
    }
    let mut segments = Vec::new();
    let mut duplicate_branches = Vec::new();
    let mut seen_branches = HashSet::from([MAIN_BRANCH.to_string()]);
    let mut branch = MAIN_BRANCH.to_string();
    let mut from_line = 1usize;
    let mut opening = None;
    for edge in &edges {
        segments.push(TreeSegment {
            branch,
            edge: opening,
            from_line,
            to_line: edge.line,
        });
        if seen_branches.contains(&edge.branch) && !duplicate_branches.contains(&edge.branch) {
            duplicate_branches.push(edge.branch.clone());
        }
        seen_branches.insert(edge.branch.clone());
        branch = edge.branch.clone();
        opening = Some(edge.clone());
        from_line = edge.line + 1;
    }
    segments.push(TreeSegment {
        branch: branch.clone(),
        edge: opening,
        from_line,
        to_line: last_line.max(from_line.saturating_sub(1)),
    });
    WireTree {
        edges,
        segments,
        paired_legacy_undo_lines,
        active_branch: branch,
        diagnostics: TreeDiagnostics {
            malformed_switch_lines,
            duplicate_branches,
        },
    }
}

pub(crate) fn branch_for_line(tree: &WireTree, line: usize) -> String {
    let mut owner = MAIN_BRANCH.to_string();
    for segment in &tree.segments {
        if segment.from_line > line {
            break;
        }
        owner = segment.branch.clone();
    }
    owner
}

fn first_line_at_or_after(entries: &[WireLine], line: usize) -> usize {
    let mut low = 0usize;
    let mut high = entries.len();
    while low < high {
        let mid = (low + high) >> 1;
        if entries[mid].line < line {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    low
}

pub(crate) fn active_chain(entries: &[WireLine], tree: &WireTree) -> Result<Vec<WireLine>, String> {
    let segments: HashMap<&str, &TreeSegment> = tree
        .segments
        .iter()
        .map(|segment| (segment.branch.as_str(), segment))
        .collect();
    let mut windows = Vec::new();
    let mut visited = HashSet::new();
    let mut branch = Some(tree.active_branch.as_str());
    let mut upto = None;
    while let Some(current) = branch {
        if !visited.insert(current.to_string()) {
            return Err(format!(
                "agent.switched base chain cycles at branch '{current}'"
            ));
        }
        let Some(segment) = segments.get(current) else {
            return Err(format!(
                "agent.switched base chain references unknown branch '{current}'"
            ));
        };
        windows.push((
            segment.edge.as_ref().map_or(1, |edge| edge.line + 1),
            upto.unwrap_or(segment.to_line),
        ));
        if let Some(edge) = &segment.edge {
            upto = Some(edge.base.1);
            branch = Some(edge.base.0.as_str());
        } else {
            branch = None;
        }
    }
    let mut out = Vec::new();
    for (from_line, to_line) in windows.into_iter().rev() {
        for entry in entries
            .iter()
            .skip(first_line_at_or_after(entries, from_line))
        {
            if entry.line > to_line {
                break;
            }
            if entry.record.get("type").and_then(Value::as_str) == Some(AGENT_SWITCHED_TYPE) {
                continue;
            }
            out.push(entry.clone());
        }
    }
    Ok(out)
}

fn is_context_message(value: &Value) -> bool {
    value
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|_| value.get("content").and_then(Value::as_array).is_some())
}

fn is_undo_anchor(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(origin) = message.get("origin") else {
        return true;
    };
    if origin.get("inTurn").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match origin.get("kind").and_then(Value::as_str) {
        None | Some("user") => true,
        Some("skill_activation" | "plugin_command") => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

fn is_prompt_owned_injection(message: &Value, prompt: &Value) -> bool {
    let Some(origin) = message.get("origin") else {
        return false;
    };
    if origin.get("kind").and_then(Value::as_str) != Some("injection") {
        return false;
    }
    let Some(owner) = origin.get("ownerPromptId").and_then(Value::as_str) else {
        return false;
    };
    prompt.get("id").and_then(Value::as_str) == Some(owner)
}

pub(crate) fn restorable_chain(
    entries: &[WireLine],
    tree: &WireTree,
) -> Result<Vec<WireLine>, String> {
    let chain: Vec<WireLine> = active_chain(entries, tree)?
        .into_iter()
        .filter(|entry| {
            let record_type = entry.record.get("type").and_then(Value::as_str);
            if record_type == Some("context.undone") {
                return false;
            }
            if record_type == Some("context.undo")
                && tree.paired_legacy_undo_lines.contains(&entry.line)
            {
                return false;
            }
            true
        })
        .collect();
    let chain_lines: HashSet<usize> = chain.iter().map(|entry| entry.line).collect();
    let mut surviving_prompt_ids = HashSet::new();
    for entry in &chain {
        if entry.record.get("type").and_then(Value::as_str) != Some("context.append_message") {
            continue;
        }
        let Some(message) = entry
            .record
            .get("message")
            .filter(|value| is_context_message(value))
        else {
            continue;
        };
        if !is_undo_anchor(message) {
            continue;
        }
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            surviving_prompt_ids.insert(id.to_string());
        }
    }
    let mut re_included = Vec::new();
    for entry in entries {
        if chain_lines.contains(&entry.line)
            || entry.record.get("type").and_then(Value::as_str) != Some("context.append_message")
        {
            continue;
        }
        let Some(message) = entry
            .record
            .get("message")
            .filter(|value| is_context_message(value))
        else {
            continue;
        };
        let Some(origin) = message.get("origin") else {
            continue;
        };
        if origin.get("kind").and_then(Value::as_str) != Some("injection") {
            continue;
        }
        let include = match origin.get("ownerPromptId") {
            None => true,
            Some(Value::String(owner)) => surviving_prompt_ids.contains(owner),
            Some(_) => false,
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

#[derive(Debug)]
pub(crate) enum ForkLineError {
    CompactionBoundary,
    Insufficient,
}

#[derive(Debug, Clone)]
struct NumberedMessage {
    message: Value,
    line: usize,
}

fn valid_undo_count(value: &Value) -> Option<usize> {
    let count = value.as_f64()?;
    if count > 0.0 && count <= 9_007_199_254_740_991.0 && count.fract() == 0.0 {
        Some(count as usize)
    } else {
        None
    }
}

fn fold_unpaired_undo(messages: &mut Vec<NumberedMessage>, count: usize) {
    let mut removed = 0usize;
    for index in (0..messages.len()).rev() {
        let message = &messages[index].message;
        if message
            .get("origin")
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str)
            == Some("injection")
        {
            continue;
        }
        if message
            .get("origin")
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str)
            == Some("compaction_summary")
        {
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
        while cut_index > 0 && is_prompt_owned_injection(&messages[cut_index - 1].message, message)
        {
            cut_index -= 1;
        }
        messages.truncate(cut_index);
        return;
    }
}

pub(crate) fn compute_fork_line(
    chain: &[WireLine],
    paired_legacy_undo_lines: &HashSet<usize>,
    turns: usize,
) -> Result<usize, ForkLineError> {
    let clear_floor = chain
        .iter()
        .filter(|entry| entry.record.get("type").and_then(Value::as_str) == Some("context.clear"))
        .map(|entry| entry.line)
        .max()
        .unwrap_or(0);
    let compaction_marker = json!({
        "role": "user",
        "content": [],
        "toolCalls": [],
        "origin": { "kind": "compaction_summary" },
    });
    let mut messages = Vec::new();
    for entry in chain {
        if entry.line <= clear_floor {
            continue;
        }
        match entry.record.get("type").and_then(Value::as_str) {
            Some("context.append_message") => {
                if let Some(message) = entry
                    .record
                    .get("message")
                    .filter(|value| is_context_message(value))
                {
                    messages.push(NumberedMessage {
                        message: message.clone(),
                        line: entry.line,
                    });
                }
            }
            Some("context.apply_compaction") => messages.push(NumberedMessage {
                message: compaction_marker.clone(),
                line: entry.line,
            }),
            Some("context.undo") if !paired_legacy_undo_lines.contains(&entry.line) => {
                if let Some(count) = entry.record.get("count").and_then(valid_undo_count) {
                    fold_unpaired_undo(&mut messages, count);
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
        let kind = message
            .get("origin")
            .and_then(|origin| origin.get("kind"))
            .and_then(Value::as_str);
        if kind == Some("injection") {
            continue;
        }
        if kind == Some("compaction_summary") {
            return Err(ForkLineError::CompactionBoundary);
        }
        if is_undo_anchor(message) {
            remaining -= 1;
            let mut cut = index;
            while cut > 0 && is_prompt_owned_injection(&messages[cut - 1].message, message) {
                cut -= 1;
            }
            cut_index = Some(cut);
        }
    }
    let Some(cut_index) = cut_index.filter(|_| remaining == 0) else {
        return Err(ForkLineError::Insufficient);
    };
    Ok(messages[cut_index].line - 1)
}
