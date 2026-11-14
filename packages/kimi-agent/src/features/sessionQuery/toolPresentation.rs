use super::events::{SessionEventSearchHit, SessionSearchHit};
use super::types::{SessionLineageNode, SessionLineageTrace};

fn format_time(timestamp: i64) -> String {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| {
                    ctx.eval::<String, _>(format!("new Date({timestamp}).toISOString()"))
                })
            })
        })
        .unwrap_or_else(|_| panic!("Invalid time value"))
}

pub fn format_empty_session_search() -> String {
    "No matching sessions found.".to_string()
}

pub fn format_session_search(items: &[SessionSearchHit], capped: bool) -> String {
    if items.is_empty() {
        return format_empty_session_search();
    }
    let mut lines = vec![format!("Session search results ({}):", items.len())];
    for (index, hit) in items.iter().enumerate() {
        let availability = [
            hit.session.live.then_some("live"),
            hit.session.persisted.then_some("persisted"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let availability = if availability.is_empty() {
            "unavailable"
        } else {
            &availability
        };
        lines.push(String::new());
        lines.push(format!(
            "{}. Session {} — {}",
            index + 1,
            hit.session.id,
            hit.session.cwd.as_deref().unwrap_or("(no cwd)")
        ));
        lines.push(format!("   Created: {}", format_time(hit.session.created_at)));
        lines.push(format!(
            "   Parent: {}",
            hit.session.parent_session_id.as_deref().unwrap_or("root")
        ));
        lines.push(format!("   Availability: {availability}"));
        lines.push(format!(
            "   Best match: seq {} | {} | {}",
            hit.best_match.record.seq,
            hit.best_match.record.r#type,
            format_time(hit.best_match.record.time)
        ));
        lines.push(format!("   Snippet: {}", hit.best_match.snippet));
    }
    if capped {
        lines.push(String::new());
        lines.push(
            "Result cap reached. Narrow the query or add filters to find additional matches."
                .to_string(),
        );
    }
    lines.join("\n")
}

pub fn format_event_search(
    session_id: &str,
    items: &[SessionEventSearchHit],
    capped: bool,
) -> String {
    if items.is_empty() {
        return format!("No matching events in session {session_id}.");
    }
    let mut lines = vec![format!(
        "Event search results for session {session_id} ({}):",
        items.len()
    )];
    for (index, hit) in items.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "{}. seq {} | {} | {}",
            index + 1,
            hit.record.seq,
            hit.record.r#type,
            format_time(hit.record.time)
        ));
        lines.push(format!("   Snippet: {}", hit.snippet));
    }
    if capped {
        lines.push(String::new());
        lines.push(
            "Result cap reached. Narrow the query or add filters to find additional matches."
                .to_string(),
        );
    }
    lines.join("\n")
}

pub fn format_session_trace(trace: &SessionLineageTrace) -> String {
    let mut lines = vec![format!("Session {}:", trace.target.id)];
    let mut ancestry: Vec<String> = trace
        .ancestors
        .iter()
        .rev()
        .map(|record| record.id.clone())
        .collect();
    if trace.complete {
        if let Some(root) = &trace.root {
            ancestry.push(root.id.clone());
        }
    } else if let Some(unresolved) = &trace.unresolved_parent_id {
        ancestry.push(unresolved.clone());
    }
    let boundary = if trace.complete { "complete" } else { "incomplete" };
    lines.push(format!(
        "   Ancestors: {} ({boundary})",
        ancestry.join(" → ")
    ));
    lines.push(format!(
        "   Descendants: {}",
        format_descendants(&trace.descendants, 1)
    ));
    lines.join("\n")
}

fn format_descendants(nodes: &[SessionLineageNode], depth: usize) -> String {
    if nodes.is_empty() {
        return "(none)".to_string();
    }
    nodes
        .iter()
        .map(|node| {
            format!(
                "\n{}{}{}",
                "   ".repeat(depth),
                node.session.id,
                if node.descendants.is_empty() {
                    String::new()
                } else {
                    format_descendants(&node.descendants, depth + 1)
                }
            )
        })
        .collect()
}
