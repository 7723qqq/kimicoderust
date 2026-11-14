use std::collections::BTreeMap;

use super::types::{SessionLineageNode, SessionLineageTrace, SessionRecord};

pub fn trace_lineage(
    records: &[SessionRecord],
    target_id: &str,
) -> Result<SessionLineageTrace, String> {
    let by_id: BTreeMap<&str, &SessionRecord> = records
        .iter()
        .map(|record| (record.id.as_str(), record))
        .collect();
    let target = by_id
        .get(target_id)
        .copied()
        .ok_or_else(|| format!("session \"{target_id}\" not found in the logical corpus"))?;
    let mut ancestors = Vec::new();
    let mut cursor = target.parent_session_id.as_deref();
    let mut unresolved_parent_id = None;
    while let Some(parent_id) = cursor {
        let Some(parent) = by_id.get(parent_id).copied() else {
            unresolved_parent_id = Some(parent_id.to_string());
            break;
        };
        ancestors.push(parent.clone());
        cursor = parent.parent_session_id.as_deref();
    }
    let mut children: BTreeMap<&str, Vec<&SessionRecord>> = BTreeMap::new();
    for record in records {
        if let Some(parent) = record.parent_session_id.as_deref() {
            children.entry(parent).or_default().push(record);
        }
    }
    let descendants = build_descendants(&children, target_id);
    let complete = unresolved_parent_id.is_none();
    Ok(SessionLineageTrace {
        target: target.clone(),
        root: complete
            .then(|| ancestors.last().cloned().unwrap_or_else(|| target.clone())),
        ancestors,
        descendants,
        complete,
        unresolved_parent_id,
    })
}

fn build_descendants(
    children: &BTreeMap<&str, Vec<&SessionRecord>>,
    parent_id: &str,
) -> Vec<SessionLineageNode> {
    children
        .get(parent_id)
        .into_iter()
        .flatten()
        .map(|child| SessionLineageNode {
            session: (*child).clone(),
            descendants: build_descendants(children, &child.id),
        })
        .collect()
}
