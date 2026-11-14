use std::collections::{HashMap, HashSet};
use std::error::Error;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wire::record::WireRecord;

pub const MAIN_BRANCH: &str = "main";
pub const AGENT_SWITCHED_TYPE: &str = "agent.switched";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireLine {
    pub record: WireRecord,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchBase {
    pub branch: String,
    pub line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchEdge {
    pub line: u64,
    pub branch: String,
    pub base: BranchBase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_undo_line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeSegment {
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<SwitchEdge>,
    pub from_line: u64,
    pub to_line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeDiagnostics {
    pub malformed_switch_lines: Vec<u64>,
    pub duplicate_branches: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireTree {
    pub edges: Vec<SwitchEdge>,
    pub segments: Vec<TreeSegment>,
    pub paired_legacy_undo_lines: HashSet<u64>,
    pub active_branch: String,
    pub diagnostics: TreeDiagnostics,
}

fn read_switch_edge(record: &WireRecord, line: u64) -> Option<SwitchEdge> {
    if record.get("type").and_then(Value::as_str) != Some(AGENT_SWITCHED_TYPE) {
        return None;
    }
    let branch = record.get("branch")?.as_str()?.to_string();
    let base = record.get("base")?.as_object()?;
    let base_branch = base.get("branch")?.as_str()?.to_string();
    let base_line = base.get("line")?.as_u64()?;
    let reason = record
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_string);
    let legacy_undo_line = record.get("legacyUndoLine").and_then(Value::as_u64);
    Some(SwitchEdge {
        line,
        branch,
        base: BranchBase {
            branch: base_branch,
            line: base_line,
        },
        reason,
        legacy_undo_line,
    })
}

pub fn branch_for_line(tree: &WireTree, line: u64) -> String {
    let mut owner = MAIN_BRANCH;
    for segment in &tree.segments {
        if segment.from_line > line {
            break;
        }
        owner = &segment.branch;
    }
    owner.to_string()
}

pub fn parse_tree(entries: &[WireLine], last_line: u64) -> WireTree {
    let mut edges = Vec::new();
    let mut paired_legacy_undo_lines = HashSet::new();
    let mut malformed_switch_lines = Vec::new();
    for WireLine { record, line } in entries {
        if record.get("type").and_then(Value::as_str) != Some(AGENT_SWITCHED_TYPE) {
            continue;
        }
        let Some(edge) = read_switch_edge(record, *line) else {
            malformed_switch_lines.push(*line);
            continue;
        };
        if let Some(legacy_undo_line) = edge.legacy_undo_line {
            paired_legacy_undo_lines.insert(legacy_undo_line);
        }
        edges.push(edge);
    }

    let mut segments = Vec::new();
    let mut duplicate_branches = Vec::new();
    let mut seen_branches = HashSet::from([MAIN_BRANCH.to_string()]);
    let mut branch = MAIN_BRANCH.to_string();
    let mut from_line = 1;
    let mut opening = None;
    for edge in edges.iter().cloned() {
        segments.push(TreeSegment {
            branch: branch.clone(),
            edge: opening,
            from_line,
            to_line: edge.line,
        });
        if seen_branches.contains(&edge.branch)
            && !duplicate_branches.contains(&edge.branch)
        {
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
        to_line: last_line.max(from_line - 1),
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

fn first_line_at_or_after(entries: &[WireLine], line: u64) -> usize {
    let mut low = 0;
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

pub fn active_chain(
    entries: &[WireLine],
    tree: &WireTree,
) -> Result<Vec<WireLine>, Box<dyn Error + Send + Sync>> {
    let segments: HashMap<&str, &TreeSegment> = tree
        .segments
        .iter()
        .map(|segment| (segment.branch.as_str(), segment))
        .collect();
    let mut windows = Vec::new();
    let mut visited = HashSet::new();
    let mut branch = Some(tree.active_branch.clone());
    let mut upto = None;
    while let Some(current_branch) = branch {
        if !visited.insert(current_branch.clone()) {
            return Err(format!(
                "agent.switched base chain cycles at branch '{current_branch}'"
            )
            .into());
        }
        let Some(segment) = segments.get(current_branch.as_str()) else {
            return Err(format!(
                "agent.switched base chain references unknown branch '{current_branch}'"
            )
            .into());
        };
        windows.push((
            segment.edge.as_ref().map_or(1, |edge| edge.line + 1),
            upto.unwrap_or(segment.to_line),
        ));
        upto = segment.edge.as_ref().map(|edge| edge.base.line);
        branch = segment
            .edge
            .as_ref()
            .map(|edge| edge.base.branch.clone());
    }

    let mut out = Vec::new();
    for (from_line, to_line) in windows.into_iter().rev() {
        for entry in &entries[first_line_at_or_after(entries, from_line)..] {
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
