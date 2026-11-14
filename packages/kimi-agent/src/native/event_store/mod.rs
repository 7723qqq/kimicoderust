pub mod blobs;
pub mod context_transcript;
pub mod loop_fold;
pub mod migration;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawWireEvent {
    pub id: String,
    pub session_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub is_checkpoint: bool,
    pub is_compaction: bool,
    pub created_at: i64,
}

pub const WIRE_PROTOCOL_VERSION: &str = "1.5";

pub const WIRE_MIN_READER_VERSION: &str = "1.5";

pub fn compare_wire_versions(a: &str, b: &str) -> i32 {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        let l = left.next();
        let r = right.next();
        if l.is_none() && r.is_none() {
            return 0;
        }
        let diff = parse_version_segment(l.unwrap_or("")) - parse_version_segment(r.unwrap_or(""));
        if diff != 0 {
            return diff;
        }
    }
}

fn parse_version_segment(segment: &str) -> i32 {
    segment.parse::<i32>().unwrap_or(0)
}

pub fn is_newer_wire_version(read_version: &str) -> bool {
    compare_wire_versions(read_version, WIRE_PROTOCOL_VERSION) > 0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,

    #[serde(default)]
    pub blocks: serde_json::Value,
    pub tool_calls: Option<serde_json::Value>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum EventStoreError {
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Undo violation: Cannot undo across compaction boundary")]
    UndoCompactionBoundary,
    #[error("Undo failed: Checkpoint not found or history exhausted")]
    CheckpointNotFound,

    #[error(
        "This session requires a newer version of Kimi Code. Upgrade Kimi Code to the latest version to read this session."
    )]
    WireProtocolTooNew { found: String, supported: String },

    #[error(
        "This session requires a newer version of Kimi Code. Upgrade Kimi Code to the latest version to read this session."
    )]
    WireReaderTooLow { required: String, supported: String },

    #[error("{0}")]
    SessionOccupied(String),

    #[error(transparent)]
    WireMigration(#[from] migration::WireMigrationError),
}

pub trait EventStore: Send + Sync {
    fn append_event(&self, event: &RawWireEvent) -> Result<u64, EventStoreError>;
    fn fold_projection(&self, session_id: &str) -> Result<Vec<Message>, EventStoreError>;
    fn checkpoint_compress(&self, session_id: &str, summary: &str) -> Result<(), EventStoreError>;
    fn undo_to_last_checkpoint(&self, session_id: &str) -> Result<usize, EventStoreError>;
}

pub type WireEventFoldRow = (String, serde_json::Value, bool);

pub fn refuse_newer_wire_version(
    conn: &Connection,
    session_id: &str,
) -> Result<(), EventStoreError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT protocol_version, min_protocol_version FROM wire_events WHERE session_id = ?1 \
         AND (protocol_version IS NOT NULL OR min_protocol_version IS NOT NULL)",
    )?;
    let mut rows = stmt.query(params![session_id])?;
    while let Some(row) = rows.next()? {
        let version: Option<String> = row.get(0)?;
        if let Some(version) = version
            && is_newer_wire_version(&version)
        {
            return Err(EventStoreError::WireProtocolTooNew {
                found: version,
                supported: WIRE_PROTOCOL_VERSION.to_string(),
            });
        }
        let required: Option<String> = row.get(1)?;
        if let Some(required) = required
            && compare_wire_versions(WIRE_PROTOCOL_VERSION, &required) < 0
        {
            return Err(EventStoreError::WireReaderTooLow {
                required,
                supported: WIRE_PROTOCOL_VERSION.to_string(),
            });
        }
    }
    Ok(())
}

pub fn read_fold_rows(
    conn: &Connection,
    session_id: &str,
    cache: &blobs::BlobCache,
) -> Result<(Vec<WireEventFoldRow>, usize), EventStoreError> {
    refuse_newer_wire_version(conn, session_id)?;

    let mut stmt = conn.prepare(
        "SELECT seq, event_type, payload, is_compaction, protocol_version FROM wire_events \
         WHERE session_id = ?1 \
         AND seq >= (SELECT COALESCE(MAX(seq), 0) FROM wire_events WHERE session_id = ?1 AND is_compaction = 1) \
         ORDER BY seq ASC",
    )?;
    let mut fold_rows = Vec::new();
    let mut dropped = 0usize;
    let mut rows = stmt.query(params![session_id])?;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let mut event_type: String = row.get(1)?;
        let payload_str: String = row.get(2)?;
        let is_compaction: bool = row.get(3)?;
        let protocol_version: Option<String> = row.get(4)?;
        match serde_json::from_str(&payload_str) {
            Ok(mut payload) => {
                migration::migrate_wire_record(
                    &mut event_type,
                    &mut payload,
                    protocol_version.as_deref(),
                )?;

                blobs::rehydrate_blob_refs(&mut payload, conn, cache)?;
                fold_rows.push((event_type, payload, is_compaction));
            }
            Err(e) => {
                dropped += 1;

                tracing::error!(
                    %session_id,
                    index = seq,
                    event_type = %event_type,
                    %e,
                    "skipping a wire event whose payload is not valid JSON"
                );
            }
        }
    }
    if dropped > 0 {
        tracing::warn!(
            %session_id,
            dropped_count = dropped,
            "session projection rebuilt from a partial wire log; the dropped rows are still in the table"
        );
    }
    Ok((fold_rows, dropped))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireRepairReason {
    Corrupted,

    Truncated,
}

impl WireRepairReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Corrupted => "corrupted",
            Self::Truncated => "truncated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRepairReport {
    pub reason: WireRepairReason,

    pub line_number: u64,

    pub dropped_count: usize,

    pub backup_created: bool,

    pub backup_path: Option<PathBuf>,
}

struct RawWireRow {
    seq: i64,
    id: String,
    event_type: String,
    payload: String,
    is_checkpoint: bool,
    is_compaction: bool,
    created_at: i64,
    protocol_version: Option<String>,
    min_protocol_version: Option<String>,
}

impl RawWireRow {
    fn to_backup_line(&self, session_id: &str) -> String {
        let payload: serde_json::Value = serde_json::from_str(&self.payload)
            .unwrap_or_else(|_| serde_json::Value::String(self.payload.clone()));
        serde_json::json!({
            "seq": self.seq,
            "id": self.id,
            "session_id": session_id,
            "type": self.event_type,
            "payload": payload,
            "is_checkpoint": self.is_checkpoint,
            "is_compaction": self.is_compaction,
            "created_at": self.created_at,
            "protocol_version": self.protocol_version,
            "min_protocol_version": self.min_protocol_version,
        })
        .to_string()
    }
}

#[must_use]
pub fn wire_backup_path(db_path: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut name = db_path.as_os_str().to_os_string();
    name.push(format!(".{safe}.wire.bak"));
    PathBuf::from(name)
}

fn read_raw_wire_rows(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<RawWireRow>, EventStoreError> {
    let mut stmt = conn.prepare(
        "SELECT seq, id, event_type, payload, is_checkpoint, is_compaction, created_at, \
                protocol_version, min_protocol_version \
         FROM wire_events WHERE session_id = ?1 ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map(params![session_id], |row| {
        Ok(RawWireRow {
            seq: row.get(0)?,
            id: row.get(1)?,
            event_type: row.get(2)?,
            payload: row.get(3)?,
            is_checkpoint: row.get(4)?,
            is_compaction: row.get(5)?,
            created_at: row.get(6)?,
            protocol_version: row.get(7)?,
            min_protocol_version: row.get(8)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, rusqlite::Error>>()
        .map_err(EventStoreError::from)
}

fn write_wire_backup(path: &Path, rows: &[RawWireRow], session_id: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("bak.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        for row in rows {
            file.write_all(row.to_backup_line(session_id).as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;

    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

pub fn repair_wire_journal(
    conn: &Connection,
    session_id: &str,
    backup_path: Option<&Path>,
) -> Result<Option<WireRepairReport>, EventStoreError> {
    let rows = read_raw_wire_rows(conn, session_id)?;
    let corrupt: Vec<(usize, i64)> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| serde_json::from_str::<serde_json::Value>(&row.payload).is_err())
        .map(|(index, row)| (index, row.seq))
        .collect();
    if corrupt.is_empty() {
        return Ok(None);
    }

    let reason = if corrupt.last().map(|(index, _)| *index) == Some(rows.len().saturating_sub(1)) {
        WireRepairReason::Truncated
    } else {
        WireRepairReason::Corrupted
    };
    let line_number = corrupt[0].0 as u64 + 1;

    let mut backup_created = false;
    let mut backup_target = None;
    if let Some(path) = backup_path {
        if !path.exists() {
            write_wire_backup(path, &rows, session_id)
                .map_err(|error| EventStoreError::Serialization(serde_json::Error::io(error)))?;
            backup_created = true;
        }
        backup_target = Some(path.to_path_buf());
    }

    let transaction = conn.unchecked_transaction()?;
    for (_, seq) in &corrupt {
        transaction.execute(
            "DELETE FROM wire_events WHERE session_id = ?1 AND seq = ?2",
            params![session_id, seq],
        )?;
    }
    transaction.commit()?;

    let report = WireRepairReport {
        reason,
        line_number,
        dropped_count: corrupt.len(),
        backup_created,
        backup_path: backup_target,
    };
    tracing::warn!(
        code = "wire_repair",
        %session_id,
        line_number = report.line_number,
        reason = report.reason.as_str(),
        dropped_count = report.dropped_count,
        backup_created = report.backup_created,
        backup_path = report.backup_path.as_ref().map(|p| p.display().to_string()),
        "corrupted wire journal truncated to its valid prefix"
    );
    Ok(Some(report))
}

pub fn read_fold_rows_with_repair(
    conn: &Connection,
    session_id: &str,
    cache: &blobs::BlobCache,
    backup_path: Option<&Path>,
) -> Result<(Vec<WireEventFoldRow>, usize, Option<WireRepairReport>), EventStoreError> {
    let (fold_rows, dropped) = read_fold_rows(conn, session_id, cache)?;
    if dropped == 0 {
        return Ok((fold_rows, dropped, None));
    }

    let report = match repair_wire_journal(conn, session_id, backup_path) {
        Ok(report) => report,
        Err(error) => {
            tracing::error!(
                code = "wire_repair",
                %session_id,
                dropped_count = dropped,
                %error,
                "wire journal repair failed; the projection is returned from the rows that decoded"
            );
            None
        }
    };
    Ok((fold_rows, dropped, report))
}

fn add_wire_column(conn: &Connection, column: &str) -> Result<(), EventStoreError> {
    match conn.execute(
        &format!("ALTER TABLE wire_events ADD COLUMN {column} TEXT"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("duplicate column name") => Ok(()),
        Err(error) => Err(EventStoreError::Database(error)),
    }
}

pub struct SqliteEventStore {
    conn: Arc<Mutex<Connection>>,
    blob_cache: blobs::BlobCache,

    path: Option<PathBuf>,
}

#[cfg(debug_assertions)]
static OPEN_JOURNALS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

impl SqliteEventStore {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self, EventStoreError> {
        let path = path.as_ref().to_path_buf();
        #[cfg(debug_assertions)]
        {
            let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let first_open = OPEN_JOURNALS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key.clone());
            if !first_open {
                tracing::warn!(
                    code = "wire.direct_open",
                    path = %key.display(),
                    "the wire journal was opened directly more than once; this bypasses the \
                     session occupancy lock (production paths use SqliteSessionStore)"
                );
            }
        }
        let conn = Connection::open(&path)?;

        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS wire_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                session_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                is_checkpoint BOOLEAN NOT NULL DEFAULT 0,
                is_compaction BOOLEAN NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                protocol_version TEXT,
                min_protocol_version TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_session_seq ON wire_events(session_id, seq);
            "#,
        )?;
        conn.execute_batch(blobs::BLOB_TABLE_SQL)?;

        add_wire_column(&conn, "protocol_version")?;
        add_wire_column(&conn, "min_protocol_version")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            blob_cache: blobs::BlobCache::new(),
            path: Some(path),
        })
    }

    pub fn new_in_memory() -> Result<Self, EventStoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS wire_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE,
                session_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                is_checkpoint BOOLEAN NOT NULL DEFAULT 0,
                is_compaction BOOLEAN NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                protocol_version TEXT,
                min_protocol_version TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_session_seq ON wire_events(session_id, seq);
            "#,
        )?;
        conn.execute_batch(blobs::BLOB_TABLE_SQL)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            blob_cache: blobs::BlobCache::new(),
            path: None,
        })
    }
}

fn extract_content_text(payload: &serde_json::Value, field: &str) -> Option<String> {
    if let Some(val) = payload.get(field) {
        if let Some(s) = val.as_str() {
            return Some(s.to_string());
        }
        if let Some(arr) = val.as_array() {
            let mut buf = String::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(text);
                } else if let Some(img) = item.get("image_url") {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(&format!(
                        "[Image: {}]",
                        img.get("url").and_then(|u| u.as_str()).unwrap_or("inline")
                    ));
                }
            }
            if !buf.is_empty() {
                return Some(buf);
            }
            return Some(val.to_string());
        }
        if val.is_object() {
            return Some(val.to_string());
        }
    }
    None
}

fn extract_blocks(payload: &serde_json::Value) -> serde_json::Value {
    payload
        .get("blocks")
        .cloned()
        .filter(|v| v.is_array())
        .unwrap_or_else(|| serde_json::json!([]))
}

fn compaction_message(value: &serde_json::Value) -> Option<Message> {
    let role = match value.get("role").and_then(|role| role.as_str())? {
        "system" => MessageRole::System,
        "user" => MessageRole::User,
        "assistant" => MessageRole::Assistant,
        "tool" => MessageRole::Tool,
        _ => return None,
    };
    Some(Message {
        role,
        content: value
            .get("content")
            .and_then(|content| content.as_str())
            .unwrap_or_default()
            .to_string(),
        blocks: value
            .get("blocks")
            .cloned()
            .filter(|blocks| blocks.is_array())
            .unwrap_or_else(|| serde_json::json!([])),
        tool_calls: value
            .get("tool_calls")
            .cloned()
            .filter(|calls| !calls.is_null()),
        tool_call_id: value
            .get("tool_call_id")
            .and_then(|id| id.as_str())
            .map(str::to_string),
    })
}

fn strip_think_blocks(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    while let Some(start) = remaining.find("<think>") {
        result.push_str(&remaining[..start]);
        if let Some(end) = remaining[start + 7..].find("</think>") {
            remaining = &remaining[start + 7 + end + 8..];
        } else {
            remaining = "";
            break;
        }
    }
    result.push_str(remaining);
    result.trim().to_string()
}

fn is_vacuous_part(part: &serde_json::Value) -> bool {
    match part.get("type").and_then(|kind| kind.as_str()) {
        Some("text") => part
            .get("text")
            .and_then(|text| text.as_str())
            .is_none_or(|text| text.trim().is_empty()),
        Some("think") => {
            let blank = part
                .get("think")
                .and_then(|think| think.as_str())
                .is_none_or(|think| think.trim().is_empty());
            let signed = part.get("encrypted").is_some() || part.get("signature").is_some();
            blank && !signed
        }
        _ => false,
    }
}

fn is_vacuous_message(msg: &Message) -> bool {
    if msg.role == MessageRole::Tool {
        return false;
    }
    if msg
        .tool_calls
        .as_ref()
        .and_then(|calls| calls.as_array())
        .is_some_and(|calls| !calls.is_empty())
    {
        return false;
    }
    if !msg.content.trim().is_empty() {
        return false;
    }
    msg.blocks
        .as_array()
        .is_none_or(|parts| parts.iter().all(is_vacuous_part))
}

fn sanitize_and_repair_projection(messages: Vec<Message>) -> Vec<Message> {
    let messages: Vec<Message> = messages
        .into_iter()
        .filter(|message| !is_vacuous_message(message))
        .collect();
    if messages.is_empty() {
        return messages;
    }

    let mut declared_tool_call_ids = std::collections::HashSet::new();
    for msg in &messages {
        if msg.role == MessageRole::Assistant
            && let Some(ref calls) = msg.tool_calls
            && let Some(calls_arr) = calls.as_array()
        {
            for c in calls_arr {
                if let Some(id) = c.get("id").and_then(|i| i.as_str()) {
                    declared_tool_call_ids.insert(id.to_string());
                }
            }
        }
    }

    let filtered_tools: Vec<Message> = messages
        .into_iter()
        .filter(|msg| {
            if msg.role == MessageRole::Tool {
                if let Some(ref id) = msg.tool_call_id {
                    declared_tool_call_ids.contains(id)
                } else {
                    false
                }
            } else {
                true
            }
        })
        .collect();

    let mut seen_call_ids = std::collections::HashSet::new();
    let mut deduped_calls: Vec<Message> = Vec::with_capacity(filtered_tools.len());
    for mut msg in filtered_tools {
        if msg.role == MessageRole::Assistant
            && let Some(calls) = msg.tool_calls.as_ref().and_then(|c| c.as_array())
        {
            let kept: Vec<serde_json::Value> = calls
                .iter()
                .filter(|call| {
                    let Some(id) = call.get("id").and_then(|i| i.as_str()) else {
                        return true;
                    };
                    seen_call_ids.insert(id.to_string())
                })
                .cloned()
                .collect();
            if kept.len() != calls.len() {
                if kept.is_empty() && msg.content.trim().is_empty() {
                    continue;
                }
                msg.tool_calls = Some(serde_json::Value::Array(kept));
            }
        }
        deduped_calls.push(msg);
    }

    const INTERRUPTED: &str =
        crate::native::event_store::loop_fold::TOOL_INTERRUPTED_ON_RESUME_OUTPUT;
    let mut result_indexes: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut deduped_results: Vec<Message> = Vec::with_capacity(deduped_calls.len());
    for msg in deduped_calls {
        if msg.role == MessageRole::Tool
            && let Some(id) = msg.tool_call_id.clone()
        {
            if let Some(index) = result_indexes.get(&id).copied() {
                let kept_is_synthesized = deduped_results[index].content == INTERRUPTED;
                if kept_is_synthesized && msg.content != INTERRUPTED {
                    deduped_results[index] = msg;
                }
                continue;
            }
            result_indexes.insert(id, deduped_results.len());
        }
        deduped_results.push(msg);
    }
    let filtered_tools = deduped_results;

    let mut merged_assistants: Vec<Message> = Vec::new();
    for msg in filtered_tools {
        if let Some(last) = merged_assistants.last_mut()
            && last.role == MessageRole::Assistant
            && msg.role == MessageRole::Assistant
        {
            last.content.push_str(&msg.content);
            merge_tool_calls(&mut last.tool_calls, msg.tool_calls);
            continue;
        }
        merged_assistants.push(msg);
    }

    let mut merged_users: Vec<Message> = Vec::new();
    for msg in merged_assistants {
        if let Some(last) = merged_users.last_mut()
            && last.role == MessageRole::User
            && msg.role == MessageRole::User
            && !crate::injection::is_system_reminder(&last.content)
            && !crate::injection::is_system_reminder(&msg.content)
        {
            if !last.content.is_empty() && !msg.content.is_empty() {
                last.content.push_str("\n\n");
            }
            last.content.push_str(&msg.content);
            merge_blocks(&mut last.blocks, msg.blocks);
            continue;
        }
        merged_users.push(msg);
    }

    let mut cleaned_leading = merged_users;
    let leading = cleaned_leading
        .iter()
        .take_while(|m| m.role != MessageRole::User && m.role != MessageRole::System)
        .count();
    cleaned_leading.drain(..leading);

    cleaned_leading
}

fn merge_tool_calls(current: &mut Option<serde_json::Value>, incoming: Option<serde_json::Value>) {
    let Some(incoming) = incoming else {
        return;
    };
    let Some(arr) = incoming.as_array() else {
        return;
    };
    if arr.is_empty() {
        return;
    }
    match current {
        Some(existing) => {
            if let Some(existing_arr) = existing.as_array_mut() {
                existing_arr.extend(arr.iter().cloned());
            } else {
                *current = Some(incoming);
            }
        }
        None => *current = Some(incoming),
    }
}

fn merge_blocks(current: &mut serde_json::Value, incoming: serde_json::Value) {
    let Some(incoming) = incoming.as_array() else {
        return;
    };
    if incoming.is_empty() {
        return;
    }
    match current.as_array_mut() {
        Some(existing) => existing.extend(incoming.iter().cloned()),
        None => *current = serde_json::Value::Array(incoming.clone()),
    }
}

const NON_PROJECTED_EVENT_TYPES: &[&str] = &[
    "turn.started",
    "turn.ended",
    "prompt.submitted",
    "prompt.queued",
    "prompt.started",
];

pub struct WireFoldReport {
    pub messages: Vec<Message>,

    pub unknown_event_types: Vec<String>,
}

pub fn fold_wire_events<'a, I>(events: I) -> Result<Vec<Message>, EventStoreError>
where
    I: IntoIterator<Item = (&'a str, &'a serde_json::Value, bool)>,
{
    Ok(fold_wire_events_with_report(events)?.messages)
}

pub fn fold_wire_events_with_report<'a, I>(events: I) -> Result<WireFoldReport, EventStoreError>
where
    I: IntoIterator<Item = (&'a str, &'a serde_json::Value, bool)>,
{
    let mut messages: Vec<Message> = Vec::new();
    let mut pending_tool_calls: Vec<String> = Vec::new();
    let mut deferred_messages: Vec<Message> = Vec::new();
    let mut unknown_event_types: Vec<String> = Vec::new();

    let flush_hanging_tools =
        |msgs: &mut Vec<Message>, pending: &mut Vec<String>, deferred: &mut Vec<Message>| {
            if !pending.is_empty() {
                for call_id in pending.drain(..) {
                    msgs.push(Message {
                        role: MessageRole::Tool,
                        content:
                            crate::native::event_store::loop_fold::TOOL_INTERRUPTED_ON_RESUME_OUTPUT
                                .to_string(),
                        blocks: serde_json::json!([]),
                        tool_calls: None,
                        tool_call_id: Some(call_id),
                    });
                }
            }
            if !deferred.is_empty() {
                msgs.append(deferred);
            }
        };

    for (event_type, payload, is_compaction) in events {
        if is_compaction {
            messages.clear();
            pending_tool_calls.clear();
            deferred_messages.clear();

            let restored_messages = payload
                .get("result")
                .and_then(|result| result.get("messages"))
                .or_else(|| payload.get("messages"))
                .and_then(|value| value.as_array());
            let mut restored = 0usize;
            if let Some(items) = restored_messages {
                for item in items {
                    if let Some(message) = compaction_message(item) {
                        messages.push(message);
                        restored += 1;
                    }
                }
            }
            if restored == 0
                && let Some(summary) = payload
                    .get("result")
                    .and_then(|result| result.get("summary"))
                    .or_else(|| payload.get("summary"))
                    .and_then(|value| value.as_str())
            {
                messages.push(Message {
                    role: MessageRole::System,
                    content: summary.to_string(),
                    blocks: serde_json::json!([]),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            continue;
        }

        match event_type {
            "message.system" => {
                if let Some(text) = extract_content_text(payload, "content") {
                    messages.push(Message {
                        role: MessageRole::System,
                        content: text,
                        blocks: extract_blocks(payload),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            "message.user" => {
                if let Some(text) = extract_content_text(payload, "content") {
                    let user_msg = Message {
                        role: MessageRole::User,
                        content: text,
                        blocks: extract_blocks(payload),
                        tool_calls: None,
                        tool_call_id: None,
                    };

                    if !pending_tool_calls.is_empty() {
                        deferred_messages.push(user_msg);
                    } else {
                        messages.push(user_msg);
                    }
                }
            }
            "message.assistant" => {
                if !pending_tool_calls.is_empty() {
                    flush_hanging_tools(
                        &mut messages,
                        &mut pending_tool_calls,
                        &mut deferred_messages,
                    );
                }

                let raw_text = extract_content_text(payload, "content").unwrap_or_default();
                let text = strip_think_blocks(&raw_text);
                let tool_calls = payload.get("tool_calls").cloned();
                let is_partial = payload
                    .get("partial")
                    .and_then(|p| p.as_bool())
                    .unwrap_or(false);

                if is_partial
                    && let Some(last) = messages.last_mut()
                    && last.role == MessageRole::Assistant
                    && last.tool_calls.is_none()
                {
                    last.content.push_str(&text);
                    continue;
                }

                if !text.is_empty() || tool_calls.is_some() {
                    if let Some(ref calls) = tool_calls
                        && let Some(calls_arr) = calls.as_array()
                    {
                        for c in calls_arr {
                            if let Some(call_id) = c.get("id").and_then(|id| id.as_str()) {
                                pending_tool_calls.push(call_id.to_string());
                            }
                        }
                    }

                    messages.push(Message {
                        role: MessageRole::Assistant,
                        content: text,
                        blocks: extract_blocks(payload),
                        tool_calls,
                        tool_call_id: None,
                    });
                }
            }
            "tool.result" => {
                let mut tool_call_id = payload
                    .get("tool_call_id")
                    .and_then(|id| id.as_str())
                    .map(String::from);
                let mut content = extract_content_text(payload, "output").unwrap_or_default();

                if content.trim().is_empty() {
                    content = "<system>Tool output is empty.</system>".to_string();
                }

                if tool_call_id.is_none() && !pending_tool_calls.is_empty() {
                    tool_call_id = Some(pending_tool_calls.remove(0));
                } else if let Some(ref id) = tool_call_id
                    && let Some(pos) = pending_tool_calls.iter().position(|x| x == id)
                {
                    pending_tool_calls.remove(pos);
                }

                messages.push(Message {
                    role: MessageRole::Tool,
                    content,
                    blocks: serde_json::json!([]),
                    tool_calls: None,
                    tool_call_id,
                });

                if pending_tool_calls.is_empty() && !deferred_messages.is_empty() {
                    messages.append(&mut deferred_messages);
                }
            }
            "step.begin" => {
                if !pending_tool_calls.is_empty() {
                    flush_hanging_tools(
                        &mut messages,
                        &mut pending_tool_calls,
                        &mut deferred_messages,
                    );
                }
                messages.push(Message {
                    role: MessageRole::Assistant,
                    content: String::new(),
                    blocks: serde_json::json!([]),
                    tool_calls: Some(serde_json::json!([])),
                    tool_call_id: None,
                });
            }
            "content.part" => {
                if let Some(text) = payload.get("text").and_then(|t| t.as_str())
                    && let Some(last) = messages.last_mut()
                    && last.role == MessageRole::Assistant
                {
                    last.content.push_str(text);
                }
            }
            "tool.call" => {
                let call_id = payload
                    .get("tool_call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("call")
                    .to_string();
                let name = payload
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let args = payload
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                pending_tool_calls.push(call_id.clone());
                if let Some(last) = messages.last_mut()
                    && last.role == MessageRole::Assistant
                {
                    let call_obj = serde_json::json!({
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args,
                        }
                    });
                    if let Some(ref mut arr) =
                        last.tool_calls.as_mut().and_then(|v| v.as_array_mut())
                    {
                        arr.push(call_obj);
                    }
                }
            }
            "step.end" => {
                let finish_reason = payload.get("finish_reason").and_then(|r| r.as_str());
                if finish_reason != Some("interrupted")
                    && finish_reason != Some("error")
                    && let Some(last) = messages.last_mut()
                    && last.role == MessageRole::Assistant
                    && let Some(arr) = last.tool_calls.as_ref().and_then(|v| v.as_array())
                    && arr.is_empty()
                {
                    last.tool_calls = None;
                }
            }
            _ => {
                if !NON_PROJECTED_EVENT_TYPES.contains(&event_type)
                    && !unknown_event_types.iter().any(|known| known == event_type)
                {
                    unknown_event_types.push(event_type.to_string());
                }
            }
        }
    }

    flush_hanging_tools(
        &mut messages,
        &mut pending_tool_calls,
        &mut deferred_messages,
    );

    for event_type in &unknown_event_types {
        tracing::warn!(
            code = "wire.unknown_record",
            event_type = %event_type,
            "unknown wire record type skipped while restoring the session projection"
        );
    }

    Ok(WireFoldReport {
        messages: sanitize_and_repair_projection(messages),
        unknown_event_types,
    })
}

impl EventStore for SqliteEventStore {
    fn append_event(&self, event: &RawWireEvent) -> Result<u64, EventStoreError> {
        let conn = self.conn.lock();

        let tx = conn.unchecked_transaction()?;
        let mut payload = event.payload.clone();
        blobs::offload_data_uris(&mut payload, &tx, &self.blob_cache)?;
        let payload_str = serde_json::to_string(&payload)?;
        tx.execute(
            "INSERT INTO wire_events (id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at, protocol_version, min_protocol_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                event.id,
                event.session_id,
                event.event_type,
                payload_str,
                event.is_checkpoint,
                event.is_compaction,
                event.created_at,
                WIRE_PROTOCOL_VERSION,
                WIRE_MIN_READER_VERSION
            ],
        )?;
        let seq = tx.last_insert_rowid() as u64;
        tx.commit()?;
        Ok(seq)
    }

    fn fold_projection(&self, session_id: &str) -> Result<Vec<Message>, EventStoreError> {
        let raw_rows = {
            let conn = self.conn.lock();
            let backup = self
                .path
                .as_deref()
                .map(|path| wire_backup_path(path, session_id));
            read_fold_rows_with_repair(&conn, session_id, &self.blob_cache, backup.as_deref())?.0
        };

        fold_wire_events(raw_rows.iter().map(|(t, p, c)| (t.as_str(), p, *c)))
    }

    fn checkpoint_compress(&self, session_id: &str, summary: &str) -> Result<(), EventStoreError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let event = RawWireEvent {
            id: ulid::Ulid::new().to_string(),
            session_id: session_id.to_string(),
            event_type: "context.compaction".to_string(),
            payload: serde_json::json!({ "summary": summary }),
            is_checkpoint: true,
            is_compaction: true,
            created_at: now,
        };
        self.append_event(&event)?;
        Ok(())
    }

    fn undo_to_last_checkpoint(&self, session_id: &str) -> Result<usize, EventStoreError> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let last_checkpoint: Option<(i64, bool)> = tx
            .query_row(
                "SELECT seq, is_compaction FROM wire_events 
             WHERE session_id = ?1 AND is_checkpoint = 1 
             ORDER BY seq DESC LIMIT 1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        let (target_seq, is_compaction) =
            last_checkpoint.ok_or(EventStoreError::CheckpointNotFound)?;

        if is_compaction {
            return Err(EventStoreError::UndoCompactionBoundary);
        }

        let deleted_count = tx.execute(
            "DELETE FROM wire_events WHERE session_id = ?1 AND seq >= ?2",
            params![session_id, target_seq],
        )?;

        tx.commit()?;
        Ok(deleted_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fold_projection_skips_and_counts_a_corrupt_row() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_session_corrupt";
        for (id, event_type, content) in [
            ("evt_1", "message.user", "first"),
            ("evt_2", "message.assistant", "second"),
            ("evt_3", "message.user", "third"),
        ] {
            store
                .append_event(&RawWireEvent {
                    id: id.into(),
                    session_id: session.into(),
                    event_type: event_type.into(),
                    payload: serde_json::json!({ "content": content }),
                    is_checkpoint: false,
                    is_compaction: false,
                    created_at: 1,
                })
                .unwrap();
        }
        assert_eq!(store.fold_projection(session).unwrap().len(), 3);

        let truncated = "{\"content\": \"sec";
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET payload = ?1 WHERE id = 'evt_2'",
                params![truncated],
            )
            .unwrap();
        }

        let projected = store
            .fold_projection(session)
            .expect("one corrupt row must not fail the whole fold");
        let contents: Vec<&str> = projected.iter().map(|m| m.content.as_str()).collect();

        assert_eq!(contents, vec!["first\n\nthird"]);

        let conn = store.conn.lock();
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
                params![session],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 2, "the corrupt row was dropped");
    }

    #[test]
    fn test_append_dehydrates_large_media_and_read_rehydrates_it() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_blob_session";
        let payload_size = blobs::BLOB_OFFLOAD_THRESHOLD;
        let data_uri = format!("data:image/png;base64,{}", "A".repeat(payload_size));
        let blocks = serde_json::json!([
            { "type": "image_url", "image_url": { "url": data_uri } }
        ]);

        store
            .append_event(&RawWireEvent {
                id: "evt_image".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "see image", "blocks": blocks }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();

        let stored: String = {
            let conn = store.conn.lock();
            conn.query_row(
                "SELECT payload FROM wire_events WHERE id = 'evt_image'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            stored.contains(blobs::BLOBREF_PROTOCOL),
            "the written row keeps a reference, not the bytes: {stored}"
        );
        assert!(
            !stored.contains(&"A".repeat(64)),
            "the inline payload must not be stored twice"
        );

        let messages = store.fold_projection(session).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].blocks[0]["image_url"]["url"],
            serde_json::Value::String(data_uri),
            "the read expands the reference back to the data URI"
        );
    }

    #[test]
    fn test_fold_reports_unknown_event_types_and_skips_them() {
        let unknown = serde_json::json!({ "value": 1 });
        let bookkeeping = serde_json::json!({});
        let user = serde_json::json!({ "content": "kept" });
        let report = fold_wire_events_with_report(vec![
            ("state.test.unknown", &unknown, false),
            ("turn.started", &bookkeeping, false),
            ("message.user", &user, false),
        ])
        .expect("an unknown record must not fail the fold");

        assert_eq!(
            report.unknown_event_types,
            vec!["state.test.unknown"],
            "an unknown record is reported once, and a non-projected known type is not"
        );
        let contents: Vec<&str> = report.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["kept"], "the known event still folds");
    }

    #[test]
    fn test_event_store_projection_and_undo() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_session_1";

        store
            .append_event(&RawWireEvent {
                id: "evt_1".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "Hello Rust" }),
                is_checkpoint: true,
                is_compaction: false,
                created_at: 1000,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_2".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({ "content": "Hi there!" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1001,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[1].role, MessageRole::Assistant);

        store
            .checkpoint_compress(session, "Prior context summary")
            .unwrap();

        let msgs_after_compaction = store.fold_projection(session).unwrap();
        assert_eq!(msgs_after_compaction.len(), 1);
        assert_eq!(msgs_after_compaction[0].role, MessageRole::System);
        assert_eq!(msgs_after_compaction[0].content, "Prior context summary");

        let undo_res = store.undo_to_last_checkpoint(session);
        assert!(matches!(
            undo_res,
            Err(EventStoreError::UndoCompactionBoundary)
        ));
    }

    #[test]
    fn test_compaction_completed_replays_the_spliced_messages() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "session_compaction_shape";

        store
            .append_event(&RawWireEvent {
                id: "evt_1".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "old request" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1000,
            })
            .unwrap();

        let spliced = serde_json::json!([
            { "role": "system", "content": "system prompt" },
            { "role": "user", "content": "verbatim request" },
            {
                "role": "user",
                "content": "<system-reminder>\nsummary\n</system-reminder>",
                "origin": { "kind": "compaction_summary" }
            },
            { "role": "user", "content": "Context compaction is complete" }
        ]);
        store
            .append_event(&RawWireEvent {
                id: "evt_2".into(),
                session_id: session.into(),
                event_type: "compaction.completed".into(),
                payload: serde_json::json!({
                    "type": "compaction.completed",
                    "result": { "summary": "raw summary" },
                    "messages": spliced,
                }),
                is_checkpoint: false,
                is_compaction: true,
                created_at: 1001,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 4, "{msgs:?}");
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[0].content, "system prompt");
        assert_eq!(msgs[1].role, MessageRole::User);
        assert_eq!(msgs[1].content, "verbatim request");
        assert_eq!(msgs[2].role, MessageRole::User);
        assert!(msgs[2].content.contains("summary"));
        assert_eq!(msgs[3].content, "Context compaction is complete");
    }

    #[test]
    fn test_deferral_queue_tool_results_consecutive() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_session_deferral";

        store
            .append_event(&RawWireEvent {
                id: "evt_0".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "start" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 99,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_1".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({
                    "content": "Calling tools",
                    "tool_calls": [
                        { "id": "call_a", "function": { "name": "read" } },
                        { "id": "call_b", "function": { "name": "grep" } }
                    ]
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 100,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_2".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "Interrupted prompt" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 101,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_3".into(),
                session_id: session.into(),
                event_type: "tool.result".into(),
                payload: serde_json::json!({ "tool_call_id": "call_a", "output": "output a" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 102,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_4".into(),
                session_id: session.into(),
                event_type: "tool.result".into(),
                payload: serde_json::json!({ "tool_call_id": "call_b", "output": "output b" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 103,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 5);

        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(msgs[3].role, MessageRole::Tool);
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("call_b"));
        assert_eq!(msgs[4].role, MessageRole::User);
        assert_eq!(msgs[4].content, "Interrupted prompt");
    }

    #[test]
    fn test_hanging_tool_call_recovery() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_session_hanging";

        store
            .append_event(&RawWireEvent {
                id: "evt_0".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "start" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 199,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_1".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({
                    "content": "Executing hanging tool",
                    "tool_calls": [
                        { "id": "call_hang", "function": { "name": "bash" } }
                    ]
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 200,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[1].role, MessageRole::Assistant);

        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_hang"));
        assert_eq!(
            msgs[2].content,
            crate::native::event_store::loop_fold::TOOL_INTERRUPTED_ON_RESUME_OUTPUT
        );
    }

    #[test]
    fn test_system_prompt_and_multimodal_extraction() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_multimodal";

        store
            .append_event(&RawWireEvent {
                id: "evt_sys".into(),
                session_id: session.into(),
                event_type: "message.system".into(),
                payload: serde_json::json!({ "content": "You are Kimi Code CLI." }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 10,
            })
            .unwrap();

        store.append_event(&RawWireEvent {
            id: "evt_multi".into(),
            session_id: session.into(),
            event_type: "message.user".into(),
            payload: serde_json::json!({
                "content": [
                    { "type": "text", "text": "Describe this image" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,mock" } }
                ]
            }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 11,
        }).unwrap();

        store.append_event(&RawWireEvent {
            id: "evt_asst".into(),
            session_id: session.into(),
            event_type: "message.assistant".into(),
            payload: serde_json::json!({
                "content": "<think>Thinking deeply about the pixels...</think>This is a test diagram."
            }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 12,
        }).unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, MessageRole::System);
        assert_eq!(msgs[0].content, "You are Kimi Code CLI.");
        assert_eq!(msgs[1].role, MessageRole::User);
        assert!(msgs[1].content.contains("Describe this image"));
        assert!(
            msgs[1]
                .content
                .contains("[Image: data:image/png;base64,mock]")
        );
        assert_eq!(msgs[2].role, MessageRole::Assistant);

        assert_eq!(msgs[2].content, "This is a test diagram.");
    }

    #[test]
    fn test_blocks_passthrough() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_blocks";

        let blocks_user = serde_json::json!([
            { "type": "text", "text": "Summarize this" },
            { "type": "image_url", "url": "https://example.com/img.png" }
        ]);
        store
            .append_event(&RawWireEvent {
                id: "evt_bu".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "Summarize this", "blocks": blocks_user }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();

        let blocks_asst = serde_json::json!([
            { "type": "think", "think": "reasoning", "encrypted": "sig" }
        ]);
        store
            .append_event(&RawWireEvent {
                id: "evt_ba".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({ "content": "done", "blocks": blocks_asst }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 2,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].blocks, blocks_user);
        assert_eq!(msgs[1].blocks, blocks_asst);

        store
            .append_event(&RawWireEvent {
                id: "evt_plain".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "plain text message" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 3,
            })
            .unwrap();
        let msgs2 = store.fold_projection(session).unwrap();
        let plain = msgs2.last().unwrap();
        assert_eq!(plain.blocks, serde_json::json!([]));
    }

    #[test]
    fn test_tool_result_missing_id_fifo_reconciliation() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_fifo_id";

        store
            .append_event(&RawWireEvent {
                id: "evt_0".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "start" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 29,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_1".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({
                    "content": "Running command",
                    "tool_calls": [
                        { "id": "call_1", "function": { "name": "bash" } }
                    ]
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 30,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_2".into(),
                session_id: session.into(),
                event_type: "tool.result".into(),
                payload: serde_json::json!({
                    "output": "command success"
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 31,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[2].role, MessageRole::Tool);

        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[2].content, "command success");
    }

    #[test]
    fn test_sanitize_and_repair_projection_rules() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_repair_rules";

        store
            .append_event(&RawWireEvent {
                id: "evt_leading_bad".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({ "content": "I am an orphan leading assistant" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_user_1".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "Real first user message" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 2,
            })
            .unwrap();

        store.append_event(&RawWireEvent {
            id: "evt_orphan_tool".into(),
            session_id: session.into(),
            event_type: "tool.result".into(),
            payload: serde_json::json!({ "tool_call_id": "ghost_call_999", "output": "ghost output" }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 3,
        }).unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_asst_1".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({ "content": "Part 1 of response." }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 4,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_asst_2".into(),
                session_id: session.into(),
                event_type: "message.assistant".into(),
                payload: serde_json::json!({ "content": "Part 2 of response." }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 5,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();

        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].content, "Real first user message");

        assert!(
            !msgs
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("ghost_call_999"))
        );

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert!(msgs[1].content.contains("Part 1 of response."));
        assert!(msgs[1].content.contains("Part 2 of response."));
    }

    #[test]
    fn test_event_store_streamed_loop_events() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_streamed_events";

        store
            .append_event(&RawWireEvent {
                id: "evt_u".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "Check weather" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 10,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_sb".into(),
                session_id: session.into(),
                event_type: "step.begin".into(),
                payload: serde_json::json!({ "uuid": "step_stream_1" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 11,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_cp".into(),
                session_id: session.into(),
                event_type: "content.part".into(),
                payload: serde_json::json!({ "text": "Checking " }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 12,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_tc".into(),
                session_id: session.into(),
                event_type: "tool.call".into(),
                payload: serde_json::json!({
                    "tool_call_id": "call_weather_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Beijing\"}"
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 13,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_tr".into(),
                session_id: session.into(),
                event_type: "tool.result".into(),
                payload: serde_json::json!({
                    "tool_call_id": "call_weather_1",
                    "output": "Sunny, 25C"
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 14,
            })
            .unwrap();

        store
            .append_event(&RawWireEvent {
                id: "evt_se".into(),
                session_id: session.into(),
                event_type: "step.end".into(),
                payload: serde_json::json!({ "finish_reason": "stop" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 15,
            })
            .unwrap();

        let msgs = store.fold_projection(session).unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].role, MessageRole::User);
        assert_eq!(msgs[0].content, "Check weather");
        assert_eq!(msgs[1].role, MessageRole::Assistant);
        assert_eq!(msgs[1].content, "Checking ");
        assert!(msgs[1].tool_calls.is_some());
        assert_eq!(msgs[2].role, MessageRole::Tool);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_weather_1"));
        assert_eq!(msgs[2].content, "Sunny, 25C");
    }

    #[test]
    fn wire_versions_compare_numerically_not_as_strings() {
        assert!(compare_wire_versions("1.10", "1.5") > 0);
        assert!(compare_wire_versions("1.5", "1.5") == 0);
        assert!(compare_wire_versions("1.4", "1.5") < 0);
        assert!(compare_wire_versions("2", "1.5") > 0);

        assert!(compare_wire_versions("1", "1.0") == 0);
        assert!(compare_wire_versions("1.5.1", "1.5") > 0);

        assert!(!is_newer_wire_version("next"));
        assert!(!is_newer_wire_version(WIRE_PROTOCOL_VERSION));
        assert!(is_newer_wire_version("1.6"));
    }

    #[test]
    fn append_event_stamps_the_current_protocol_version() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        store
            .append_event(&RawWireEvent {
                id: "evt_stamp".into(),
                session_id: "sess_stamp".into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "hi" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
        let stamped: String = {
            let conn = store.conn.lock();
            conn.query_row(
                "SELECT protocol_version FROM wire_events WHERE id = 'evt_stamp'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(stamped, WIRE_PROTOCOL_VERSION);
    }

    #[test]
    fn a_newer_log_is_refused_before_any_of_it_is_folded() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "sess_too_new";
        store
            .append_event(&RawWireEvent {
                id: "evt_new".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "from the future" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET protocol_version = '9.9' WHERE session_id = ?1",
                params![session],
            )
            .unwrap();
        }
        let err = store.fold_projection(session).unwrap_err();
        match err {
            EventStoreError::WireProtocolTooNew { found, supported } => {
                assert_eq!(found, "9.9");
                assert_eq!(supported, WIRE_PROTOCOL_VERSION);
            }
            other => panic!("expected WireProtocolTooNew, got {other:?}"),
        }
    }

    #[test]
    fn a_minimum_reader_version_above_this_engine_is_refused() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "sess_min_reader";
        store
            .append_event(&RawWireEvent {
                id: "evt_min".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "needs a newer reader" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET min_protocol_version = '9.9' WHERE session_id = ?1",
                params![session],
            )
            .unwrap();
        }
        let err = store.fold_projection(session).unwrap_err();
        match err {
            EventStoreError::WireReaderTooLow {
                required,
                supported,
            } => {
                assert_eq!(required, "9.9");
                assert_eq!(supported, WIRE_PROTOCOL_VERSION);
            }
            other => panic!("expected WireReaderTooLow, got {other:?}"),
        }
    }

    #[test]
    fn a_minimum_reader_version_this_engine_meets_reads_normally() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "sess_current_reader";
        let messages = store.fold_projection(session).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn rows_written_before_the_column_existed_read_as_this_version() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "sess_legacy";
        store
            .append_event(&RawWireEvent {
                id: "evt_legacy".into(),
                session_id: session.into(),
                event_type: "message.user".into(),
                payload: serde_json::json!({ "content": "pre-column" }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET protocol_version = NULL WHERE session_id = ?1",
                params![session],
            )
            .unwrap();
        }

        assert_eq!(store.fold_projection(session).unwrap().len(), 1);
    }

    #[test]
    fn test_projection_dedupes_tool_calls_and_results() {
        let call = serde_json::json!({
            "id": "call_dup",
            "type": "function",
            "function": { "name": "read", "arguments": "{}" },
        });
        let assistant_with_duplicate = Message {
            role: MessageRole::Assistant,
            content: String::new(),
            blocks: serde_json::json!([]),
            tool_calls: Some(serde_json::json!([call, call])),
            tool_call_id: None,
        };
        let interrupted = Message {
            role: MessageRole::Tool,
            content: crate::native::event_store::loop_fold::TOOL_INTERRUPTED_ON_RESUME_OUTPUT
                .to_string(),
            blocks: serde_json::json!([]),
            tool_calls: None,
            tool_call_id: Some("call_dup".into()),
        };
        let real = Message {
            role: MessageRole::Tool,
            content: "the real output".into(),
            blocks: serde_json::json!([]),
            tool_calls: None,
            tool_call_id: Some("call_dup".into()),
        };
        let opener = Message {
            role: MessageRole::User,
            content: "start".into(),
            blocks: serde_json::json!([]),
            tool_calls: None,
            tool_call_id: None,
        };
        let repaired = sanitize_and_repair_projection(vec![
            opener,
            assistant_with_duplicate,
            interrupted,
            real,
        ]);

        assert_eq!(
            repaired.len(),
            3,
            "user, one assistant, one tool: {repaired:?}"
        );
        let calls = repaired[1].tool_calls.as_ref().unwrap().as_array().unwrap();
        assert_eq!(calls.len(), 1, "the duplicate call id is dropped");
        assert_eq!(
            repaired[2].content, "the real output",
            "the real result replaces the synthesized interruption"
        );
    }

    #[test]
    fn test_projection_drops_vacuous_messages() {
        let message = |role: MessageRole, content: &str, blocks: serde_json::Value| Message {
            role,
            content: content.to_string(),
            blocks,
            tool_calls: None,
            tool_call_id: None,
        };
        let repaired = sanitize_and_repair_projection(vec![
            message(MessageRole::User, "  ", serde_json::json!([])),
            message(MessageRole::User, "hello", serde_json::json!([])),
            message(MessageRole::Assistant, "   \n\t", serde_json::json!([])),
            message(
                MessageRole::Assistant,
                "",
                serde_json::json!([{ "type": "text", "text": "  " }]),
            ),
            message(
                MessageRole::Assistant,
                "",
                serde_json::json!([{ "type": "image_url", "image_url": { "url": "inline" } }]),
            ),
            message(MessageRole::User, "and then?", serde_json::json!([])),
            message(
                MessageRole::Assistant,
                "",
                serde_json::json!([{ "type": "think", "think": "", "signature": "sig" }]),
            ),
        ]);
        let roles: Vec<MessageRole> = repaired.iter().map(|m| m.role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                MessageRole::User,
                MessageRole::Assistant,
                MessageRole::User,
                MessageRole::Assistant,
            ],
            "blank messages are gone, the image and signed-think replies stay: {repaired:?}"
        );
        assert_eq!(
            repaired[1].blocks.as_array().map(|parts| parts.len()),
            Some(1),
            "the image reply survives with its block"
        );
        assert_eq!(
            repaired[3].blocks.as_array().map(|parts| parts.len()),
            Some(1),
            "the signed think reply survives with its block"
        );
    }

    fn temporary_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kimi-wire-repair-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn append_user(store: &SqliteEventStore, session: &str, id: &str, content: &str) {
        store
            .append_event(&RawWireEvent {
                id: id.to_string(),
                session_id: session.to_string(),
                event_type: "message.user".to_string(),
                payload: serde_json::json!({ "content": content }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
    }

    #[test]
    fn test_read_fold_rows_migrates_a_row_stamped_with_an_older_version() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_session_migrate";
        store
            .append_event(&RawWireEvent {
                id: "goal_1".into(),
                session_id: session.into(),
                event_type: "goal.create".into(),
                payload: serde_json::json!({
                    "goalId": "g1",
                    "objective": "ship it",
                    "time": 7,
                    "stray": true
                }),
                is_checkpoint: false,
                is_compaction: false,
                created_at: 1,
            })
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET protocol_version = '1.3' WHERE id = 'goal_1'",
                [],
            )
            .unwrap();
        }

        let (rows, dropped) = {
            let conn = store.conn.lock();
            read_fold_rows(&conn, session, &store.blob_cache).unwrap()
        };
        assert_eq!(dropped, 0);
        let payload = &rows[0].1;
        assert_eq!(payload["wallClockResumedAt"], serde_json::json!(7));
        assert!(
            payload.get("stray").is_none(),
            "v1.4 drops fields outside the goal whitelist: {payload}"
        );
    }

    #[test]
    fn test_read_fold_rows_reports_a_missing_migration() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_session_migration_missing";
        append_user(&store, session, "e1", "hello");
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET protocol_version = '0.9' WHERE id = 'e1'",
                [],
            )
            .unwrap();
        }

        let error = {
            let conn = store.conn.lock();
            read_fold_rows(&conn, session, &store.blob_cache).unwrap_err()
        };
        assert!(
            error
                .to_string()
                .contains("Missing wire migration for version"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn test_fold_projection_repairs_a_corrupt_row_and_leaves_a_backup() {
        let dir = temporary_dir();
        let db = dir.join("sessions.db");
        let store = SqliteEventStore::new(&db).unwrap();
        let session = "test_session_repair";
        append_user(&store, session, "e1", "first");
        append_user(&store, session, "e2", "second");
        append_user(&store, session, "e3", "third");
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET payload = '{\"content\": \"thi' WHERE id = 'e3'",
                [],
            )
            .unwrap();
        }

        let projected = store.fold_projection(session).unwrap();
        assert_eq!(
            projected
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>(),
            vec!["first\n\nsecond"]
        );

        let remaining: i64 = {
            let conn = store.conn.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
                params![session],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(remaining, 2, "the undecodable row was removed");

        let backup = wire_backup_path(&db, session);
        let preserved = std::fs::read_to_string(&backup).expect("backup must exist");
        assert_eq!(
            preserved.lines().count(),
            3,
            "the backup preserves the whole original log"
        );
        assert!(preserved.lines().next().unwrap().contains("first"));

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_repair_keeps_the_first_backup_and_reports_the_reason() {
        let dir = temporary_dir();
        let db = dir.join("sessions.db");
        let store = SqliteEventStore::new(&db).unwrap();
        let session = "test_session_repair_twice";
        append_user(&store, session, "e1", "first");
        append_user(&store, session, "e2", "second");
        append_user(&store, session, "e3", "third");
        let backup = wire_backup_path(&db, session);

        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET payload = 'not json' WHERE id = 'e2'",
                [],
            )
            .unwrap();
        }
        let report = {
            let conn = store.conn.lock();
            repair_wire_journal(&conn, session, Some(&backup))
                .unwrap()
                .expect("a corrupt row must produce a report")
        };
        assert_eq!(report.reason, WireRepairReason::Corrupted);
        assert_eq!(report.line_number, 2);
        assert_eq!(report.dropped_count, 1);
        assert!(report.backup_created);
        let first_backup = std::fs::read_to_string(&backup).unwrap();
        assert_eq!(first_backup.lines().count(), 3);

        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET payload = 'oops' WHERE id = 'e3'",
                [],
            )
            .unwrap();
        }
        let report = {
            let conn = store.conn.lock();
            repair_wire_journal(&conn, session, Some(&backup))
                .unwrap()
                .expect("a second corrupt row must produce a report")
        };
        assert_eq!(report.reason, WireRepairReason::Truncated);
        assert!(!report.backup_created, "the first backup is not replaced");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), first_backup);

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_a_failed_append_leaves_no_orphan_blob() {
        let store = SqliteEventStore::new_in_memory().unwrap();
        let session = "test_orphan_blob";
        let data_uri = format!(
            "data:image/png;base64,{}",
            "A".repeat(blobs::BLOB_OFFLOAD_THRESHOLD)
        );
        let payload = serde_json::json!([
            { "type": "image_url", "image_url": { "url": data_uri } }
        ]);
        let event = RawWireEvent {
            id: "evt_dup".into(),
            session_id: session.into(),
            event_type: "message.user".into(),
            payload: payload.clone(),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        };
        store.append_event(&event).unwrap();
        let blobs_after_first: i64 = {
            let conn = store.conn.lock();
            conn.query_row("SELECT COUNT(*) FROM wire_blobs", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(blobs_after_first, 1);

        let data_uri = format!(
            "data:image/png;base64,{}",
            "B".repeat(blobs::BLOB_OFFLOAD_THRESHOLD)
        );
        let duplicate = RawWireEvent {
            id: "evt_dup".into(),
            payload: serde_json::json!([
                { "type": "image_url", "image_url": { "url": data_uri } }
            ]),
            ..event
        };
        assert!(
            store.append_event(&duplicate).is_err(),
            "a duplicate id must be rejected"
        );

        let conn = store.conn.lock();
        let blobs_after_failure: i64 = conn
            .query_row("SELECT COUNT(*) FROM wire_blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            blobs_after_failure, 1,
            "the rejected append must not leave its offloaded media behind"
        );
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
                params![session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn test_a_failed_repair_still_returns_the_projection() {
        let dir = temporary_dir();
        let db = dir.join("sessions.db");
        let store = SqliteEventStore::new(&db).unwrap();
        let session = "test_session_repair_failure";
        append_user(&store, session, "e1", "first");
        append_user(&store, session, "e2", "second");
        {
            let conn = store.conn.lock();
            conn.execute(
                "UPDATE wire_events SET payload = 'not json' WHERE id = 'e1'",
                [],
            )
            .unwrap();
        }

        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let unwritable = blocker.join("nested").join("session.wire.bak");

        let (rows, dropped, report) = {
            let conn = store.conn.lock();
            read_fold_rows_with_repair(&conn, session, &store.blob_cache, Some(&unwritable))
                .expect("a failed repair must not discard the folded projection")
        };
        assert_eq!(dropped, 1);
        assert!(report.is_none(), "no repair report when the repair failed");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "message.user");

        let remaining: i64 = {
            let conn = store.conn.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
                params![session],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(remaining, 2, "a failed repair deletes nothing");

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }
}
