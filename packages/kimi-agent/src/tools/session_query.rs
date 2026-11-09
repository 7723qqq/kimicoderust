use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

use crate::native::event_store::EventStoreError;
use crate::session::sqlite_store::{SessionSummary, SqliteSessionStore};
use crate::turn_loop::types::ExecutableToolResult;

use super::{err_result, ok_result};

pub const TOOL_NAME: &str = "session_query";

const MAX_SEARCH_RESULTS: usize = 20;

const SNIPPET_RADIUS: usize = 40;

const WIRE_PAGE: usize = 512;

const ERROR_INDEX_UNAVAILABLE: &str = "session_query.index_unavailable";
const ERROR_SESSION_NOT_FOUND: &str = "session_query.session_not_found";
const ERROR_INVALID_FILTER: &str = "session_query.invalid_filter";

static ISO_TIMESTAMP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2})(:\d{2}(?:\.\d+)?)?(Z|[+-]\d{2}:\d{2})$")
        .expect("the ISO 8601 timestamp pattern is a valid regex")
});

#[derive(Debug)]
enum QueryError {
    InvalidFilter(String),

    SessionNotFound(String),

    IndexUnavailable(String),

    ReadFailed(String),

    Corrupt(String),
}

impl QueryError {
    fn render(&self) -> String {
        match self {
            Self::InvalidFilter(detail) => format!("{ERROR_INVALID_FILTER}: {detail}"),
            Self::SessionNotFound(id) => {
                format!("{ERROR_SESSION_NOT_FOUND}: session \"{id}\" not found")
            }
            Self::IndexUnavailable(detail) => format!("{ERROR_INDEX_UNAVAILABLE}: {detail}"),
            Self::ReadFailed(detail) => detail.clone(),
            Self::Corrupt(detail) => detail.clone(),
        }
    }
}

struct EventDocument {
    seq: u64,
    event_type: String,
    time: i64,
    text: String,
}

#[derive(Default)]
struct EventFilters {
    seq_from: Option<u64>,
    seq_to: Option<u64>,
    time_from: Option<i64>,
    time_to: Option<i64>,
    event_types: Option<Vec<String>>,
}

impl EventFilters {
    fn matches(&self, document: &EventDocument) -> bool {
        if self.seq_from.is_some_and(|from| document.seq < from)
            || self.seq_to.is_some_and(|to| document.seq > to)
            || self.time_from.is_some_and(|from| document.time < from)
            || self.time_to.is_some_and(|to| document.time > to)
        {
            return false;
        }
        self.event_types.as_ref().is_none_or(|types| {
            types
                .iter()
                .any(|candidate| candidate == &document.event_type)
        })
    }
}

#[derive(Default)]
struct SessionFilters {
    ids: Option<Vec<String>>,
    created_from: Option<i64>,
    created_to: Option<i64>,

    parents: Option<Vec<Option<String>>>,
    availability: Option<Vec<Availability>>,
}

#[derive(PartialEq, Eq)]
enum Availability {
    Live,
    Persisted,
}

impl SessionFilters {
    fn matches(&self, record: &SessionSummary, live: bool) -> bool {
        if self
            .ids
            .as_ref()
            .is_some_and(|ids| !ids.iter().any(|id| id == &record.session_id))
        {
            return false;
        }
        if self
            .created_from
            .is_some_and(|from| record.created_at < from)
            || self.created_to.is_some_and(|to| record.created_at > to)
        {
            return false;
        }
        if self.parents.as_ref().is_some_and(|parents| {
            !parents.iter().any(|parent| match parent {
                Some(parent) => record.parent_session_id.as_deref() == Some(parent.as_str()),
                None => record.parent_session_id.is_none(),
            })
        }) {
            return false;
        }
        if self.availability.as_ref().is_some_and(|values| {
            !values.iter().any(|value| match value {
                Availability::Live => live,
                Availability::Persisted => true,
            })
        }) {
            return false;
        }
        true
    }
}

struct SessionHit {
    record: SessionSummary,
    live: bool,
    best: EventDocument,
    snippet: String,
}

struct LineageNode {
    session: SessionSummary,
    descendants: Vec<LineageNode>,
}

struct LineageTrace {
    target: SessionSummary,

    ancestors: Vec<SessionSummary>,
    descendants: Vec<LineageNode>,
    complete: bool,
    root: Option<SessionSummary>,
    unresolved_parent_id: Option<String>,
}

pub async fn execute_session_query(
    store: Option<Arc<SqliteSessionStore>>,
    session_id: Option<&str>,
    root: &Path,
    args: &Value,
) -> ExecutableToolResult {
    let Some(store) = store else {
        return err_result(format!(
            "{ERROR_INDEX_UNAVAILABLE}: this turn has no session store to read; \
             session history is unavailable until one is attached."
        ));
    };
    let Some(session_id) = session_id else {
        return err_result(format!(
            "{ERROR_INDEX_UNAVAILABLE}: this turn has no session id to read history for."
        ));
    };

    let query_store = Arc::clone(&store);
    let query_session = session_id.to_string();
    let query_root = root.to_path_buf();
    let query_args = args.clone();

    let outcome = tokio::task::spawn_blocking(move || {
        query_sync(&query_store, &query_session, &query_root, &query_args)
    })
    .await;

    match outcome {
        Err(join) => err_result(join.to_string()),
        Ok(Err(error)) => err_result(error.render()),
        Ok(Ok(text)) => ok_result(text),
    }
}

fn query_sync(
    store: &SqliteSessionStore,
    session_id: &str,
    root: &Path,
    args: &Value,
) -> Result<String, QueryError> {
    let (operation, operation_args) = parse_top_level(args)?;
    let mut summaries = store
        .list_sessions()
        .map_err(|error| classify_sql_error("listing sessions", error))?;

    summaries.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    let current = summaries
        .iter()
        .find(|summary| summary.session_id == session_id)
        .cloned()
        .ok_or_else(|| QueryError::SessionNotFound(session_id.to_string()))?;

    match operation.as_str() {
        "session_search" => session_search(store, &summaries, &current, root, &operation_args),
        "event_search" => event_search(store, &summaries, &current, &operation_args),
        "session_trace" => session_trace(&summaries, &current, &operation_args),

        other => Err(QueryError::InvalidFilter(format!(
            "unknown operation \"{other}\""
        ))),
    }
}

fn session_search(
    store: &SqliteSessionStore,
    summaries: &[SessionSummary],
    current: &SessionSummary,
    root: &Path,
    args: &Map<String, Value>,
) -> Result<String, QueryError> {
    reject_unknown(
        args,
        &[
            "query",
            "session_ids",
            "created_at_from",
            "created_at_to",
            "parent_session_ids",
            "include_root_sessions",
            "availability",
            "event_seq_from",
            "event_seq_to",
            "event_time_from",
            "event_time_to",
            "event_types",
        ],
    )?;
    let query = normalize_query(&required_string(args, "query")?)?;
    let terms = query_terms(&query)?;
    let session_filters = build_session_filters(args)?;
    let event_filters = build_event_filters(
        args,
        "event_seq_from",
        "event_seq_to",
        "event_time_from",
        "event_time_to",
        "event_types",
        "sequence",
    )?;

    let cwd = current
        .cwd
        .clone()
        .or_else(|| current.environment_cwd.clone())
        .unwrap_or_else(|| root.to_string_lossy().to_string());

    let mut hits: Vec<SessionHit> = Vec::new();
    for record in summaries {
        if record.cwd.as_deref() != Some(cwd.as_str()) {
            continue;
        }
        let live = record.session_id == current.session_id;
        if !session_filters.matches(record, live) {
            continue;
        }
        let documents = read_documents(store, &record.session_id)?;
        let best = documents
            .iter()
            .filter(|document| event_filters.matches(document))
            .filter_map(|document| {
                let score = score_document(&document.text, &terms);
                (score > 0).then_some(document)
            })
            .fold(None::<&EventDocument>, |best, document| match best {
                Some(current) if current.time >= document.time => Some(current),
                _ => Some(document),
            });
        if let Some(best) = best {
            hits.push(SessionHit {
                record: record.clone(),
                live,
                snippet: build_snippet(&best.text, &terms),
                best: EventDocument {
                    seq: best.seq,
                    event_type: best.event_type.clone(),
                    time: best.time,
                    text: best.text.clone(),
                },
            });
        }
    }

    hits.sort_by(|a, b| b.best.time.cmp(&a.best.time));
    let capped = hits.len() > MAX_SEARCH_RESULTS;
    hits.truncate(MAX_SEARCH_RESULTS);
    Ok(render_session_search(&hits, capped))
}

fn event_search(
    store: &SqliteSessionStore,
    summaries: &[SessionSummary],
    current: &SessionSummary,
    args: &Map<String, Value>,
) -> Result<String, QueryError> {
    reject_unknown(
        args,
        &[
            "session_id",
            "query",
            "seq_from",
            "seq_to",
            "time_from",
            "time_to",
            "event_types",
        ],
    )?;
    let target_id =
        optional_string(args, "session_id")?.unwrap_or_else(|| current.session_id.clone());
    if !summaries
        .iter()
        .any(|summary| summary.session_id == target_id)
    {
        return Err(QueryError::SessionNotFound(target_id));
    }
    let query = normalize_query(&required_string(args, "query")?)?;
    let terms = query_terms(&query)?;
    let filters = build_event_filters(
        args,
        "seq_from",
        "seq_to",
        "time_from",
        "time_to",
        "event_types",
        "sequence",
    )?;

    let documents = read_documents(store, &target_id)?;
    let mut hits: Vec<(EventDocument, String)> = documents
        .into_iter()
        .filter(|document| filters.matches(document))
        .filter_map(|document| {
            let score = score_document(&document.text, &terms);
            (score > 0).then(|| {
                let snippet = build_snippet(&document.text, &terms);
                (document, snippet)
            })
        })
        .collect();
    hits.sort_by(|a, b| b.0.time.cmp(&a.0.time));
    let capped = hits.len() > MAX_SEARCH_RESULTS;
    hits.truncate(MAX_SEARCH_RESULTS);
    Ok(render_event_search(&target_id, &hits, capped))
}

fn session_trace(
    summaries: &[SessionSummary],
    current: &SessionSummary,
    args: &Map<String, Value>,
) -> Result<String, QueryError> {
    reject_unknown(args, &["session_id"])?;
    let target_id =
        optional_string(args, "session_id")?.unwrap_or_else(|| current.session_id.clone());
    let trace = trace_lineage(summaries, &target_id)?;
    Ok(render_trace(&trace))
}

fn trace_lineage(
    summaries: &[SessionSummary],
    target_id: &str,
) -> Result<LineageTrace, QueryError> {
    let by_id: HashMap<&str, &SessionSummary> = summaries
        .iter()
        .map(|summary| (summary.session_id.as_str(), summary))
        .collect();
    let target = by_id
        .get(target_id)
        .map(|summary| (*summary).clone())
        .ok_or_else(|| QueryError::SessionNotFound(target_id.to_string()))?;

    let mut ancestors = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor = target.parent_session_id.clone();
    let mut complete = true;
    let mut unresolved_parent_id = None;
    while let Some(parent_id) = cursor {
        if !seen.insert(parent_id.clone()) {
            complete = false;
            unresolved_parent_id = Some(parent_id);
            break;
        }
        match by_id.get(parent_id.as_str()) {
            Some(parent) => {
                ancestors.push((*parent).clone());
                cursor = parent.parent_session_id.clone();
            }
            None => {
                complete = false;
                unresolved_parent_id = Some(parent_id);
                break;
            }
        }
    }

    let mut children: HashMap<String, Vec<SessionSummary>> = HashMap::new();
    for record in summaries {
        if let Some(parent) = &record.parent_session_id {
            children
                .entry(parent.clone())
                .or_default()
                .push(record.clone());
        }
    }
    let mut visited = HashSet::new();
    visited.insert(target_id.to_string());
    let descendants = build_descendants(&children, target_id, &mut visited);

    let root = if complete {
        ancestors.last().cloned().or_else(|| Some(target.clone()))
    } else {
        None
    };

    Ok(LineageTrace {
        target,
        ancestors,
        descendants,
        complete,
        root,
        unresolved_parent_id,
    })
}

fn build_descendants(
    children: &HashMap<String, Vec<SessionSummary>>,
    parent_id: &str,
    visited: &mut HashSet<String>,
) -> Vec<LineageNode> {
    let Some(direct) = children.get(parent_id) else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for child in direct {
        if !visited.insert(child.session_id.clone()) {
            continue;
        }
        nodes.push(LineageNode {
            session: child.clone(),
            descendants: build_descendants(children, &child.session_id, visited),
        });
    }
    nodes
}

fn read_documents(
    store: &SqliteSessionStore,
    session_id: &str,
) -> Result<Vec<EventDocument>, QueryError> {
    let mut documents = Vec::new();
    let mut cursor = 0u64;
    loop {
        let page = store
            .search_wire_events(session_id, cursor, WIRE_PAGE)
            .map_err(|error| classify_store_error(session_id, error))?;
        if page.is_empty() {
            break;
        }
        for record in &page {
            cursor = record.seq;

            if record.event_type == "metadata" {
                continue;
            }
            documents.push(EventDocument {
                seq: documents.len() as u64,
                event_type: record.event_type.clone(),
                time: wire_record_time(&record.payload, record.created_at),
                text: wire_record_text(&record.payload),
            });
        }
        if page.len() < WIRE_PAGE {
            break;
        }
    }
    Ok(documents)
}

fn wire_record_time(payload: &Value, created_at: i64) -> i64 {
    match payload.get("time") {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64))
            .unwrap_or(created_at),
        _ => created_at,
    }
}

fn wire_record_text(payload: &Value) -> String {
    const TEXT_FIELDS: [&str; 10] = [
        "text",
        "prompt",
        "output",
        "content",
        "arguments",
        "input",
        "delta",
        "result",
        "reason",
        "description",
    ];
    let mut parts: Vec<String> = Vec::new();
    for field in TEXT_FIELDS {
        if let Some(text) = payload.get(field).and_then(Value::as_str) {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
    if let Some(content) = payload.get("content").and_then(Value::as_array) {
        push_content_parts(content, &mut parts);
    }
    if let Some(message) = payload.get("message").and_then(Value::as_object) {
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            push_content_parts(content, &mut parts);
        }
        if let Some(text) = message.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
    parts.join("\n")
}

fn push_content_parts(content: &[Value], parts: &mut Vec<String>) {
    for part in content {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
            if !text.is_empty() {
                parts.push(text.to_string());
            }
        }
    }
}

fn query_terms(query: &str) -> Result<Vec<String>, QueryError> {
    let mut seen = HashSet::new();
    let terms: Vec<String> = crate::tools::memory_search::tokenize(query)
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect();
    if terms.is_empty() {
        return Err(QueryError::InvalidFilter(
            "command search query must contain searchable text".to_string(),
        ));
    }
    Ok(terms)
}

fn score_document(text: &str, terms: &[String]) -> usize {
    if text.is_empty() {
        return 0;
    }
    let document_terms = crate::tools::memory_search::tokenize(text);
    terms
        .iter()
        .map(|term| {
            document_terms
                .iter()
                .filter(|candidate| *candidate == term)
                .count()
        })
        .sum()
}

fn build_snippet(text: &str, terms: &[String]) -> String {
    let lower = text.to_lowercase();
    let mut first_match: Option<usize> = None;
    for term in terms {
        if let Some(byte_index) = lower.find(term) {
            let char_index = lower[..byte_index].chars().count();
            first_match = Some(match first_match {
                Some(current) => current.min(char_index),
                None => char_index,
            });
        }
    }
    let chars: Vec<char> = text.chars().collect();
    let Some(first_match) = first_match else {
        return chars.into_iter().take(SNIPPET_RADIUS * 2).collect();
    };
    let start = first_match.saturating_sub(SNIPPET_RADIUS);
    let end = (first_match + SNIPPET_RADIUS).min(chars.len());
    let prefix = if start > 0 { "…" } else { "" };
    let suffix = if end < chars.len() { "…" } else { "" };
    format!(
        "{prefix}{}{suffix}",
        collapse_whitespace(&chars[start..end])
    )
}

fn collapse_whitespace(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut previous_whitespace = false;
    for ch in chars {
        if ch.is_whitespace() {
            if !previous_whitespace {
                out.push(' ');
            }
            previous_whitespace = true;
        } else {
            out.push(*ch);
            previous_whitespace = false;
        }
    }
    out
}

fn render_session_search(hits: &[SessionHit], capped: bool) -> String {
    if hits.is_empty() {
        return "No matching sessions found.\nNext: run event_search on the current session, \
                or widen the created_at/event filters."
            .to_string();
    }
    let mut lines = vec![format!("Session search results ({}):", hits.len())];
    for (index, hit) in hits.iter().enumerate() {
        let availability = if hit.live {
            "live, persisted"
        } else {
            "persisted"
        };
        lines.push(String::new());
        lines.push(format!(
            "{}. Session {} — {}",
            index + 1,
            hit.record.session_id,
            hit.record.cwd.as_deref().unwrap_or("(no cwd)")
        ));
        lines.push(format!(
            "   Created: {}",
            format_time(hit.record.created_at)
        ));
        lines.push(format!(
            "   Parent: {}",
            hit.record.parent_session_id.as_deref().unwrap_or("root")
        ));
        lines.push(format!("   Availability: {availability}"));
        lines.push(format!(
            "   Best match: seq {} | {} | {}",
            hit.best.seq,
            hit.best.event_type,
            format_time(hit.best.time)
        ));
        lines.push(format!("   Snippet: {}", hit.snippet));
    }
    if capped {
        lines.push(String::new());
        lines.push(
            "Result cap reached. Narrow the query or add filters to find additional matches."
                .to_string(),
        );
        lines.push(
            "Next: add event_types or a narrower created_at range to rank the remaining \
             sessions."
                .to_string(),
        );
    }
    lines.join("\n")
}

fn render_event_search(session_id: &str, hits: &[(EventDocument, String)], capped: bool) -> String {
    if hits.is_empty() {
        return format!(
            "No matching events in session {session_id}.\nNext: run session_search with the \
             same query to search across sessions, or widen the seq/time/type filters."
        );
    }
    let mut lines = vec![format!(
        "Event search results for session {session_id} ({}):",
        hits.len()
    )];
    for (index, (document, snippet)) in hits.iter().enumerate() {
        lines.push(String::new());
        lines.push(format!(
            "{}. seq {} | {} | {}",
            index + 1,
            document.seq,
            document.event_type,
            format_time(document.time)
        ));
        lines.push(format!("   Snippet: {snippet}"));
    }
    if capped {
        lines.push(String::new());
        lines.push(
            "Result cap reached. Narrow the query or add filters to find additional matches."
                .to_string(),
        );
        lines.push(
            "Next: raise precision with event_types or a narrower seq/time range.".to_string(),
        );
    }
    lines.join("\n")
}

fn render_trace(trace: &LineageTrace) -> String {
    let mut lines = vec![format!("Session {}:", trace.target.session_id)];
    let mut ancestry: Vec<&str> = trace
        .ancestors
        .iter()
        .map(|record| record.session_id.as_str())
        .collect();

    ancestry.reverse();
    let mut chain: Vec<String> = ancestry.into_iter().map(str::to_string).collect();
    let boundary = if trace.complete {
        if let Some(root) = &trace.root {
            chain.push(root.session_id.clone());
        }
        "complete"
    } else {
        if let Some(unresolved) = &trace.unresolved_parent_id {
            chain.push(unresolved.clone());
        }
        "incomplete"
    };
    lines.push(format!("   Ancestors: {} ({boundary})", chain.join(" → ")));
    let mut descendants = String::new();
    format_descendants(&trace.descendants, 1, &mut descendants);
    lines.push(format!("   Descendants: {descendants}"));
    lines.join("\n")
}

fn format_descendants(nodes: &[LineageNode], depth: usize, out: &mut String) {
    if nodes.is_empty() {
        out.push_str("(none)");
        return;
    }
    for node in nodes {
        out.push('\n');
        for _ in 0..depth {
            out.push_str("   ");
        }
        out.push_str(&node.session.session_id);
        if !node.descendants.is_empty() {
            format_descendants(&node.descendants, depth + 1, out);
        }
    }
}

fn format_time(milliseconds: i64) -> String {
    DateTime::from_timestamp_millis(milliseconds)
        .map(|time| {
            time.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        })
        .unwrap_or_else(|| milliseconds.to_string())
}

fn parse_top_level(args: &Value) -> Result<(String, Map<String, Value>), QueryError> {
    let object = args
        .as_object()
        .ok_or_else(|| QueryError::InvalidFilter("arguments must be an object".to_string()))?;
    reject_unknown(object, &["operation", "operationArgs"])?;
    let operation = object
        .get("operation")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            QueryError::InvalidFilter(
                "Invalid arguments: `operation` is required and must be a string".to_string(),
            )
        })?
        .to_string();
    if !matches!(
        operation.as_str(),
        "session_search" | "event_search" | "session_trace"
    ) {
        return Err(QueryError::InvalidFilter(
            "Unknown session_query operation.".to_string(),
        ));
    }
    let operation_args = match object.get("operationArgs") {
        None => Map::new(),
        Some(Value::Object(args)) => args.clone(),
        Some(_) => {
            return Err(QueryError::InvalidFilter(
                "Invalid arguments: `operationArgs` must be an object".to_string(),
            ));
        }
    };
    Ok((operation, operation_args))
}

fn build_session_filters(args: &Map<String, Value>) -> Result<SessionFilters, QueryError> {
    let mut filters = SessionFilters::default();
    if let Some(ids) = optional_string_array(args, "session_ids")? {
        if ids.is_empty() {
            return Err(invalid(
                "session_ids must contain at least one value when supplied",
            ));
        }
        filters.ids = Some(ids);
    }
    let created_from = optional_iso(args, "created_at_from")?;
    let created_to = optional_iso(args, "created_at_to")?;
    if let (Some(from), Some(to)) = (created_from, created_to) {
        if from > to {
            return Err(invalid(
                "created_at range from must be less than or equal to to",
            ));
        }
    }
    filters.created_from = created_from;
    filters.created_to = created_to;

    let include_root = optional_bool(args, "include_root_sessions")?.unwrap_or(false);
    if let Some(parents) = optional_string_array(args, "parent_session_ids")? {
        if parents.is_empty() {
            return Err(invalid(
                "parent_session_ids must contain at least one value when supplied",
            ));
        }
        let mut values: Vec<Option<String>> = Vec::new();
        for parent in parents {
            let value = Some(parent);
            if !values.contains(&value) {
                values.push(value);
            }
        }
        if include_root {
            values.push(None);
        }
        filters.parents = Some(values);
    } else if include_root {
        filters.parents = Some(vec![None]);
    }

    if let Some(availability) = optional_string_array(args, "availability")? {
        if availability.is_empty() {
            return Err(invalid(
                "availability must contain at least one value when supplied",
            ));
        }
        let mut values = Vec::new();
        for value in availability {
            match value.as_str() {
                "live" => values.push(Availability::Live),
                "persisted" => values.push(Availability::Persisted),
                other => {
                    return Err(invalid(&format!(
                        "availability filter contains unknown value \"{other}\""
                    )));
                }
            }
        }
        filters.availability = Some(values);
    }
    Ok(filters)
}

fn build_event_filters(
    args: &Map<String, Value>,
    seq_from_key: &str,
    seq_to_key: &str,
    time_from_key: &str,
    time_to_key: &str,
    types_key: &str,
    range_name: &str,
) -> Result<EventFilters, QueryError> {
    let seq_from = optional_u64(args, seq_from_key)?;
    let seq_to = optional_u64(args, seq_to_key)?;
    if let (Some(from), Some(to)) = (seq_from, seq_to) {
        if from > to {
            return Err(invalid(&format!(
                "{range_name} range from must be less than or equal to to"
            )));
        }
    }
    let time_from = optional_iso(args, time_from_key)?;
    let time_to = optional_iso(args, time_to_key)?;
    if let (Some(from), Some(to)) = (time_from, time_to) {
        if from > to {
            return Err(invalid("time range from must be less than or equal to to"));
        }
    }
    let event_types = optional_string_array(args, types_key)?;
    if event_types.as_ref().is_some_and(Vec::is_empty) {
        return Err(invalid(
            "event_types must contain at least one value when supplied",
        ));
    }
    Ok(EventFilters {
        seq_from,
        seq_to,
        time_from,
        time_to,
        event_types,
    })
}

fn normalize_query(value: &str) -> Result<String, QueryError> {
    let query = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.is_empty() {
        return Err(invalid(
            "session-search query must contain non-whitespace text",
        ));
    }
    if query.contains('\0') {
        return Err(invalid("session-search query must not contain NUL"));
    }
    Ok(query)
}

fn invalid(detail: &str) -> QueryError {
    QueryError::InvalidFilter(format!("session {detail}"))
}

fn reject_unknown(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), QueryError> {
    for key in args.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(QueryError::InvalidFilter(format!(
                "Invalid arguments: unknown field \"{key}\""
            )));
        }
    }
    Ok(())
}

fn required_string(args: &Map<String, Value>, key: &str) -> Result<String, QueryError> {
    optional_string(args, key)?
        .ok_or_else(|| QueryError::InvalidFilter(format!("Invalid arguments: `{key}` is required")))
}

fn optional_string(args: &Map<String, Value>, key: &str) -> Result<Option<String>, QueryError> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(QueryError::InvalidFilter(format!(
            "Invalid arguments: `{key}` must be a string"
        ))),
    }
}

fn optional_string_array(
    args: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, QueryError> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Array(values)) => {
            let mut strings = Vec::with_capacity(values.len());
            for value in values {
                match value.as_str() {
                    Some(value) => strings.push(value.to_string()),
                    None => {
                        return Err(QueryError::InvalidFilter(format!(
                            "Invalid arguments: `{key}` must be an array of strings"
                        )));
                    }
                }
            }
            Ok(Some(strings))
        }
        Some(_) => Err(QueryError::InvalidFilter(format!(
            "Invalid arguments: `{key}` must be an array of strings"
        ))),
    }
}

fn optional_bool(args: &Map<String, Value>, key: &str) -> Result<Option<bool>, QueryError> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(QueryError::InvalidFilter(format!(
            "Invalid arguments: `{key}` must be a boolean"
        ))),
    }
}

fn optional_u64(args: &Map<String, Value>, key: &str) -> Result<Option<u64>, QueryError> {
    match args.get(key) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            QueryError::InvalidFilter(format!(
                "Invalid arguments: `{key}` must be a non-negative integer"
            ))
        }),
    }
}

fn optional_iso(args: &Map<String, Value>, key: &str) -> Result<Option<i64>, QueryError> {
    let Some(value) = optional_string(args, key)? else {
        return Ok(None);
    };
    parse_iso_timestamp(key, &value).map(Some)
}

fn parse_iso_timestamp(name: &str, value: &str) -> Result<i64, QueryError> {
    let captures = ISO_TIMESTAMP.captures(value).ok_or_else(|| {
        invalid(&format!(
            "{name} must be an ISO 8601 timestamp with Z or a numeric offset"
        ))
    })?;

    let normalized = format!(
        "{}{}{}",
        &captures[1],
        captures.get(2).map_or(":00", |seconds| seconds.as_str()),
        &captures[3]
    );
    let parsed = DateTime::<FixedOffset>::parse_from_rfc3339(&normalized)
        .map_err(|_| invalid(&format!("{name} must be a valid ISO 8601 timestamp")))?;
    Ok(parsed.timestamp_millis())
}

fn classify_store_error(session_id: &str, error: EventStoreError) -> QueryError {
    let context = format!("reading session {session_id}");
    match &error {
        EventStoreError::WireProtocolTooNew { .. }
        | EventStoreError::WireReaderTooLow { .. }
        | EventStoreError::SessionOccupied(_) => {
            QueryError::IndexUnavailable(format!("{context}: {error}"))
        }
        EventStoreError::Serialization(_) | EventStoreError::WireMigration(_) => {
            QueryError::Corrupt(format!("{context}: {error}"))
        }
        EventStoreError::Database(inner) => {
            if matches!(inner, rusqlite::Error::FromSqlConversionFailure(..)) {
                QueryError::Corrupt(format!("{context}: {error}"))
            } else {
                QueryError::ReadFailed(format!("{context}: {error}"))
            }
        }
        _ => QueryError::ReadFailed(format!("{context}: {error}")),
    }
}

fn classify_sql_error(context: &str, error: rusqlite::Error) -> QueryError {
    if matches!(error, rusqlite::Error::FromSqlConversionFailure(..)) {
        QueryError::Corrupt(format!("{context}: {error}"))
    } else {
        QueryError::ReadFailed(format!("{context}: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store_with_sessions(
        sessions: &[(&str, Option<&str>)],
    ) -> (tempfile::TempDir, Arc<SqliteSessionStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteSessionStore::open(&dir.path().join("sessions.db")).unwrap());
        let workspace = store.create_workspace("/work", None).unwrap();
        for (id, parent) in sessions {
            store
                .create_session_with_workspace(id, None, Some(&workspace.id))
                .unwrap();
            if let Some(parent) = parent {
                store.set_parent_session_id(id, parent).unwrap();
            }
        }
        (dir, store)
    }

    fn append(
        store: &SqliteSessionStore,
        session_id: &str,
        event_type: &str,
        payload: Value,
        created_at: i64,
    ) {
        store
            .append_wire_event_json(
                &format!("{session_id}-{created_at}"),
                session_id,
                event_type,
                &payload.to_string(),
                false,
                false,
                created_at,
            )
            .unwrap();
    }

    fn run(store: &SqliteSessionStore, args: Value) -> Result<String, QueryError> {
        query_sync(store, "sess-a", Path::new("/work"), &args)
    }

    #[test]
    fn session_search_finds_matching_history() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None), ("sess-b", None)]);
        append(
            &store,
            "sess-a",
            "context.append_message",
            json!({"type": "context.append_message", "time": 100, "text": "the token pipeline"}),
            100,
        );
        append(
            &store,
            "sess-b",
            "llm.request",
            json!({"type": "llm.request", "time": 200, "prompt": "count the token budget"}),
            200,
        );

        let output = run(
            &store,
            json!({"operation": "session_search", "operationArgs": {"query": "token"}}),
        )
        .unwrap();

        assert!(output.contains("Session search results (2):"), "{output}");
        assert!(output.contains("Session sess-a"), "{output}");
        assert!(output.contains("Snippet: the token pipeline"), "{output}");
        assert!(output.contains("Session sess-b"), "{output}");
    }

    #[test]
    fn search_reports_an_empty_corpus_as_no_matches() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);
        let output = run(
            &store,
            json!({"operation": "session_search", "operationArgs": {"query": "zzz"}}),
        )
        .unwrap();
        assert!(
            output.starts_with("No matching sessions found."),
            "{output}"
        );
        assert!(output.contains("Next: run event_search"), "{output}");
    }

    #[test]
    fn event_search_returns_hits_newest_first() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);
        append(
            &store,
            "sess-a",
            "context.append_message",
            json!({"type": "context.append_message", "time": 100, "text": "token early"}),
            100,
        );
        append(
            &store,
            "sess-a",
            "llm.response",
            json!({"type": "llm.response", "time": 300, "text": "token late"}),
            300,
        );
        append(
            &store,
            "sess-a",
            "llm.response",
            json!({"type": "llm.response", "time": 200, "text": "unrelated"}),
            200,
        );

        let output = run(
            &store,
            json!({"operation": "event_search", "operationArgs": {"query": "token"}}),
        )
        .unwrap();
        assert!(
            output.starts_with("Event search results for session sess-a (2):"),
            "{output}"
        );
        let late = output.find("token late").unwrap();
        let early = output.find("token early").unwrap();
        assert!(late < early, "newest first: {output}");
    }

    #[test]
    fn event_search_is_capped_and_says_so() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);
        for index in 0..25 {
            append(
                &store,
                "sess-a",
                "context.append_message",
                json!({
                    "type": "context.append_message",
                    "time": index,
                    "text": format!("token number {index}")
                }),
                index,
            );
        }
        let output = run(
            &store,
            json!({"operation": "event_search", "operationArgs": {"query": "token"}}),
        )
        .unwrap();
        assert!(
            output.starts_with("Event search results for session sess-a (20):"),
            "{output}"
        );
        assert!(output.contains("Result cap reached"), "{output}");
        assert!(output.contains("Next: raise precision"), "{output}");
    }

    #[test]
    fn event_filters_narrow_the_result() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);
        append(
            &store,
            "sess-a",
            "context.append_message",
            json!({"type": "context.append_message", "time": 100, "text": "token"}),
            100,
        );
        append(
            &store,
            "sess-a",
            "llm.response",
            json!({"type": "llm.response", "time": 200, "text": "token"}),
            200,
        );
        let output = run(
            &store,
            json!({
                "operation": "event_search",
                "operationArgs": {"query": "token", "event_types": ["llm.response"]}
            }),
        )
        .unwrap();
        assert!(output.contains("(1):"), "{output}");
        assert!(output.contains("llm.response"), "{output}");
        assert!(!output.contains("context.append_message"), "{output}");
    }

    #[test]
    fn session_trace_walks_the_lineage() {
        let (_dir, store) = store_with_sessions(&[
            ("sess-a", None),
            ("sess-root", None),
            ("sess-mid", Some("sess-root")),
            ("sess-leaf", Some("sess-mid")),
        ]);
        let output = run(
            &store,
            json!({"operation": "session_trace", "operationArgs": {"session_id": "sess-leaf"}}),
        )
        .unwrap();
        assert!(output.starts_with("Session sess-leaf:"), "{output}");
        assert!(
            output.contains("Ancestors: sess-root → sess-mid → sess-root"),
            "{output}"
        );
        assert!(output.contains("(complete)"), "{output}");
        assert!(output.contains("Descendants: (none)"), "{output}");
    }

    #[test]
    fn corrupt_rows_are_reported_as_corrupt_not_empty() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);
        store
            .append_wire_event_json(
                "broken",
                "sess-a",
                "context.append_message",
                "{not json",
                false,
                false,
                1,
            )
            .unwrap();
        let error = run(
            &store,
            json!({"operation": "event_search", "operationArgs": {"query": "token"}}),
        )
        .unwrap_err();
        let rendered = error.render();
        assert!(matches!(&error, QueryError::Corrupt(_)), "{error:?}");
        assert!(!rendered.contains("No matching"), "{rendered}");

        assert!(!rendered.contains("session_query."), "{rendered}");
    }

    #[test]
    fn read_failure_and_corruption_are_distinguished() {
        let corrupt = classify_store_error(
            "sess-a",
            EventStoreError::Serialization(serde_json::from_str::<Value>("{").unwrap_err()),
        );
        assert!(matches!(&corrupt, QueryError::Corrupt(_)), "{corrupt:?}");
        assert!(!corrupt.render().contains("session_query."), "{corrupt:?}");

        let failed =
            classify_sql_error("listing sessions", rusqlite::Error::ExecuteReturnedResults);
        assert!(matches!(&failed, QueryError::ReadFailed(_)), "{failed:?}");
        assert!(!failed.render().contains("session_query."), "{failed:?}");

        let unavailable = classify_store_error(
            "sess-a",
            EventStoreError::WireProtocolTooNew {
                found: "9".to_string(),
                supported: "1".to_string(),
            },
        );
        assert!(unavailable.render().starts_with(ERROR_INDEX_UNAVAILABLE));
    }

    #[test]
    fn missing_target_session_is_not_found() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);
        let error = run(
            &store,
            json!({"operation": "event_search", "operationArgs": {"query": "token", "session_id": "ghost"}}),
        )
        .unwrap_err();
        assert!(
            error.render().starts_with(ERROR_SESSION_NOT_FOUND),
            "{}",
            error.render()
        );
    }

    #[test]
    fn invalid_arguments_are_rejected() {
        let (_dir, store) = store_with_sessions(&[("sess-a", None)]);

        let missing_query = run(
            &store,
            json!({"operation": "session_search", "operationArgs": {}}),
        )
        .unwrap_err();
        assert!(missing_query.render().starts_with(ERROR_INVALID_FILTER));
        assert!(missing_query.render().contains("Invalid arguments"));

        let empty_ids = run(
            &store,
            json!({"operation": "session_search", "operationArgs": {"query": "x", "session_ids": []}}),
        )
        .unwrap_err();
        assert!(
            empty_ids.render().contains("at least one value"),
            "{}",
            empty_ids.render()
        );

        let inverted = run(
            &store,
            json!({
                "operation": "event_search",
                "operationArgs": {
                    "query": "x",
                    "time_from": "2026-08-02T00:00:00Z",
                    "time_to": "2026-08-01T00:00:00Z"
                }
            }),
        )
        .unwrap_err();
        assert!(
            inverted.render().contains("less than or equal to"),
            "{}",
            inverted.render()
        );

        let unknown_operation = run(&store, json!({"operation": "nope"})).unwrap_err();
        assert!(
            unknown_operation
                .render()
                .contains("Unknown session_query operation."),
            "{}",
            unknown_operation.render()
        );
    }

    #[tokio::test]
    async fn a_missing_store_is_index_unavailable_not_empty() {
        let result = execute_session_query(
            None,
            Some("sess-a"),
            Path::new("/work"),
            &json!({"operation": "session_trace"}),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with(ERROR_INDEX_UNAVAILABLE),
            "{}",
            result.content
        );
    }
}
