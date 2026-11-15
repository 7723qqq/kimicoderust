use std::fmt;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::_base::utils::workdir_slug::workspace_root_key;
use crate::app::session_index::session_index::session_summary_from_row;
use crate::features::fileHistory::fileHistoryService::reclaim_file_history_space;
pub use crate::features::fileHistory::fileHistoryService::{
    FILE_HISTORY_MAX_CONTENT_BYTES, compute_line_diff,
};
use crate::native::event_store::{
    EventStore, EventStoreError, Message, RawWireEvent, WIRE_MIN_READER_VERSION,
    WIRE_PROTOCOL_VERSION, fold_wire_events, read_fold_rows_with_repair, refuse_newer_wire_version,
    wire_backup_path,
};
use kimi_core::types::TokenUsage;
pub use crate::session::session_metadata::prompt_metadata::{
    DEFAULT_SESSION_TITLE, is_untitled,
};
use crate::turn_loop::types::LLMMessage;

#[derive(Debug)]
struct StoredJsonDecodeError {
    what: String,
    source: serde_json::Error,
}

impl fmt::Display for StoredJsonDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is not valid JSON: {}", self.what, self.source)
    }
}

impl std::error::Error for StoredJsonDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
struct StoredJsonEncodeError {
    what: String,
    source: serde_json::Error,
}

impl fmt::Display for StoredJsonEncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} could not be encoded: {}", self.what, self.source)
    }
}

impl std::error::Error for StoredJsonEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn encode_stored_json<T: Serialize>(
    value: &T,
    what: impl Into<String>,
) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|source| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(StoredJsonEncodeError {
            what: what.into(),
            source,
        }))
    })
}

fn dehydrate_state_value<'a>(
    value: &'a Value,
    conn: &rusqlite::Connection,
    cache: &crate::native::event_store::blobs::BlobCache,
) -> rusqlite::Result<std::borrow::Cow<'a, Value>> {
    if !crate::native::event_store::blobs::has_inline_data_uri(value) {
        return Ok(std::borrow::Cow::Borrowed(value));
    }
    let mut owned = value.clone();
    crate::native::event_store::blobs::offload_data_uris(&mut owned, conn, cache)?;
    Ok(std::borrow::Cow::Owned(owned))
}

fn rehydrate_state_value(
    value: &mut Value,
    conn: &rusqlite::Connection,
    cache: &crate::native::event_store::blobs::BlobCache,
) -> rusqlite::Result<()> {
    crate::native::event_store::blobs::rehydrate_blob_refs(value, conn, cache)?;
    Ok(())
}

fn read_state_entry(
    conn: &Connection,
    cache: &crate::native::event_store::blobs::BlobCache,
    domain: &str,
    key: &str,
) -> rusqlite::Result<Option<Value>> {
    let raw = {
        let mut stmt =
            conn.prepare("SELECT value FROM state_entries WHERE domain = ?1 AND key = ?2")?;
        let mut rows = stmt.query(params![domain, key])?;
        match rows.next()? {
            Some(row) => {
                let s: String = row.get(0)?;
                Some(decode_stored_json(
                    &s,
                    format!("state entry {domain}/{key}"),
                )?)
            }
            None => None,
        }
    };
    match raw {
        Some(mut value) => {
            rehydrate_state_value(&mut value, conn, cache)?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

fn write_state_entry(
    conn: &Connection,
    cache: &crate::native::event_store::blobs::BlobCache,
    domain: &str,
    key: &str,
    value: &Value,
) -> rusqlite::Result<()> {
    let now = chrono::Utc::now().timestamp_millis();

    let tx = conn.unchecked_transaction()?;
    let value = dehydrate_state_value(value, &tx, cache)?;
    let val_str = encode_stored_json(value.as_ref(), format!("state entry {domain}/{key}"))?;
    tx.execute(
        "INSERT INTO state_entries (domain, key, value, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(domain, key) DO UPDATE SET
            value = ?3,
            updated_at = ?4",
        params![domain, key, val_str, now],
    )?;
    tx.commit()
}

pub(crate) fn decode_json_column<T: serde::de::DeserializeOwned>(
    raw: Option<String>,
    what: String,
) -> rusqlite::Result<Option<T>> {
    match raw {
        None => Ok(None),
        Some(raw) if raw.is_empty() => Ok(None),
        Some(raw) => {
            let value = decode_stored_json(&raw, what.clone())?;
            serde_json::from_value(value).map(Some).map_err(|source| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(StoredJsonDecodeError { what, source }),
                )
            })
        }
    }
}

fn decode_stored_json(raw: &str, what: impl Into<String>) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoredJsonDecodeError {
                what: what.into(),
                source,
            }),
        )
    })
}

fn table_columns(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    Ok(names.filter_map(std::result::Result::ok).collect())
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    columns: &std::collections::HashSet<String>,
    column: &str,
    definition: &str,
) -> rusqlite::Result<()> {
    if columns.contains(column) {
        return Ok(());
    }
    match conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("duplicate column name") => Ok(()),
        Err(error) => Err(error),
    }
}

fn format_iso(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn wire_export_line(
    event_type: &str,
    payload: &str,
    conn: &Connection,
    cache: &crate::native::event_store::blobs::BlobCache,
) -> rusqlite::Result<String> {
    let mut value: Value = serde_json::from_str(payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    crate::native::event_store::blobs::rehydrate_blob_refs(&mut value, conn, cache)?;
    let mut record = match value {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("payload".to_string(), other);
            map
        }
    };
    record.insert("type".to_string(), Value::String(event_type.to_string()));
    Ok(Value::Object(record).to_string())
}

pub use crate::_base::workdir_slug::encode_workdir_key;

pub const BLOB_AUDIT_MAX_ROWS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobUsage {
    pub stored: u64,

    pub bytes: u64,

    pub unreferenced: u64,

    pub scanned: u64,

    pub truncated: bool,
}

fn wire_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<WireEventRecord>> {
    let seq: u64 = row.get(0)?;
    let payload_str: String = row.get(4)?;
    let payload = match decode_stored_json(&payload_str, format!("wire event seq {seq}")) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(
                code = "wire.malformed_record",
                seq,
                %error,
                "skipping a wire event whose payload is not valid JSON"
            );
            return Ok(None);
        }
    };
    Ok(Some(WireEventRecord {
        seq,
        id: row.get(1)?,
        session_id: row.get(2)?,
        event_type: row.get(3)?,
        payload,
        is_checkpoint: row.get(5)?,
        is_compaction: row.get(6)?,
        created_at: row.get(7)?,
    }))
}

fn collect_wire_rows<I>(rows: I) -> rusqlite::Result<(Vec<WireEventRecord>, usize)>
where
    I: Iterator<Item = rusqlite::Result<Option<WireEventRecord>>>,
{
    let mut records = Vec::new();
    let mut dropped = 0usize;
    for row in rows {
        match row? {
            Some(record) => records.push(record),
            None => dropped += 1,
        }
    }
    Ok((records, dropped))
}

fn collect_logged_wire_rows<I>(
    rows: I,
    session_id: &str,
    slice: &str,
) -> Result<Vec<WireEventRecord>, EventStoreError>
where
    I: Iterator<Item = rusqlite::Result<Option<WireEventRecord>>>,
{
    let (records, dropped) = collect_wire_rows(rows)?;
    if dropped > 0 {
        tracing::warn!(
            code = "wire.malformed_record",
            %session_id,
            slice,
            dropped,
            "wire read skipped records whose payload did not parse"
        );
    }
    Ok(records)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEventRecord {
    pub seq: u64,
    pub id: String,
    pub session_id: String,
    pub event_type: String,
    pub payload: Value,
    pub is_checkpoint: bool,
    pub is_compaction: bool,
    pub created_at: i64,
}

pub const SESSION_META_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_cwd: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExport {
    pub session: SessionSummary,
    pub messages: Vec<LLMMessage>,
    pub turns_count: usize,
    pub exported_at: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,

    #[serde(default, skip_serializing)]
    pub wire_jsonl: Vec<String>,

    #[serde(default, skip_serializing)]
    pub agent_wires: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub id: String,
    pub root: String,
    pub name: String,
    pub created_at: String,
    pub last_opened_at: String,
    pub session_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: String,
    pub workspace_id: String,
    pub session_title: String,
    pub agent_id: String,
    pub role: String,
    pub snippet: String,
    pub time: String,
    pub turn: usize,
    pub step_id: String,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistoryChange {
    pub path: String,
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistoryContent {
    pub version: usize,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
}

fn extract_snippet(text: &str, query: &str, max_len: usize) -> String {
    let lower_text = text.to_lowercase();
    let lower_query = query.to_lowercase();
    if let Some(idx) = lower_text.find(&lower_query) {
        let start = idx.saturating_sub(max_len / 4);
        let end = (idx + query.len() + max_len * 3 / 4).min(text.len());
        let mut snippet = String::new();
        if start > 0 {
            snippet.push_str("...");
        }
        snippet.push_str(&text[start..end]);
        if end < text.len() {
            snippet.push_str("...");
        }
        snippet
    } else if text.len() > max_len {
        format!("{}...", &text[..max_len])
    } else {
        text.to_string()
    }
}

pub const COMPACT_TURN_ID: &str = "turn-compact";

#[derive(Debug)]
pub enum ForkError {
    InvalidTurnIndex,

    TurnNotFound {
        turn_index: u64,
        available_turns: usize,
    },

    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for ForkError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e)
    }
}

#[derive(Debug)]
pub enum SessionRemovalError {
    Occupied { message: String },

    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for SessionRemovalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error)
    }
}

impl std::fmt::Display for SessionRemovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Occupied { message } => write!(f, "{message}"),
            Self::Store(error) => write!(f, "{error}"),
        }
    }
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTurnIndex => {
                write!(
                    f,
                    "forkSession turnIndex must be a non-negative safe integer"
                )
            }
            Self::TurnNotFound {
                turn_index,
                available_turns,
            } => write!(
                f,
                "Turn {turn_index} was not found in the session ({available_turns} available)"
            ),
            Self::Store(e) => write!(f, "{e}"),
        }
    }
}

fn forked_turn_id(source_turn_id: &str, new_session_id: &str, number: u32) -> String {
    if turn_is_compaction(source_turn_id) {
        return format!("{new_session_id}-{COMPACT_TURN_ID}");
    }
    format!("{new_session_id}-turn-{number}")
}

fn turn_is_compaction(turn_id: &str) -> bool {
    turn_id == COMPACT_TURN_ID || turn_id.ends_with(&format!("-{COMPACT_TURN_ID}"))
}

fn turn_is_user_visible(origin: Option<&Value>, opening_role_is_user: bool) -> bool {
    if !opening_role_is_user {
        return false;
    }
    let Some(origin) = origin else {
        return true;
    };
    let Some(kind) = origin.get("kind").and_then(Value::as_str) else {
        return true;
    };
    match kind {
        "user" => true,
        "skill_activation" | "plugin_command" => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        "shell_command" => origin.get("phase").and_then(Value::as_str) == Some("input"),
        _ => false,
    }
}

fn turn_origin_is_undo_anchor(origin: Option<&Value>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    if origin.get("inTurn").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    match origin.get("kind").and_then(Value::as_str) {
        None | Some("user") => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

fn wire_row_is_fork_anchor(event_type: &str, payload: &Value) -> bool {
    match event_type {
        "turn.prompt" => turn_origin_is_undo_anchor(payload.get("origin")),
        "context.append_message" => {
            let message = payload.get("message").unwrap_or(payload);
            message.get("role").and_then(Value::as_str) == Some("user")
                && turn_origin_is_undo_anchor(message.get("origin"))
        }
        _ => false,
    }
}

pub struct SqliteSessionStore {
    pub(crate) conn: Mutex<Connection>,
    blob_cache: crate::native::event_store::blobs::BlobCache,

    path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CompactionReport {
    pub messages_before: usize,
    pub removed: usize,

    pub compacted_count: u32,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub kept_user_message_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub message: LLMMessage,
    pub created_at: i64,

    pub turn_id: String,
}

#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub turn_id: String,
    pub turn_number: u32,
    pub status: String,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub usage: Option<kimi_core::types::TokenUsage>,

    pub origin: Option<Value>,
}

impl CompactionReport {
    fn skipped() -> Self {
        Self {
            messages_before: 0,
            removed: 0,
            compacted_count: 0,
            tokens_before: 0,
            tokens_after: 0,
            kept_user_message_count: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRevertError {
    Conflict { path: String, reason: String },

    Io(String),
}

impl std::fmt::Display for FileRevertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { path, reason } => write!(
                f,
                "cannot revert {path}: {reason}; restoring would overwrite that change"
            ),
            Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for FileRevertError {}

impl SqliteSessionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self, rusqlite::Error> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                title TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                parent_session_id TEXT,
                last_turn_reason TEXT,
                title_kind TEXT,
                last_prompt TEXT,
                archived_at INTEGER,
                forked_from TEXT,
                version INTEGER,
                cwd TEXT,
                environment_id TEXT,
                environment_cwd TEXT,
                agents TEXT,
                custom TEXT
            );

            CREATE TABLE IF NOT EXISTS turns (
                turn_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                turn_number INTEGER NOT NULL,
                status TEXT NOT NULL,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                usage TEXT,
                origin TEXT,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                turn_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_calls TEXT,
                tool_call_id TEXT,
                blocks TEXT,
                prompt_id TEXT,
                tools TEXT,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS state_entries (
                domain TEXT NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (domain, key)
            );

            CREATE TABLE IF NOT EXISTS checkpoints (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                name TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS workspaces (
                workspace_id TEXT PRIMARY KEY,
                root TEXT NOT NULL,
                name TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_opened_at INTEGER NOT NULL,
                trusted INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS session_file_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                turn_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                status TEXT NOT NULL,
                content_before TEXT,
                content_after TEXT,
                additions INTEGER NOT NULL DEFAULT 0,
                deletions INTEGER NOT NULL DEFAULT 0,
                oversize INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_file_history_session_turn ON session_file_history(session_id, turn_id);

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
                min_protocol_version TEXT,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_wire_session_seq ON wire_events(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id);
            CREATE TABLE IF NOT EXISTS wire_blobs (
                hash TEXT PRIMARY KEY,
                mime TEXT NOT NULL,
                bytes BLOB NOT NULL
            );
            ",
        )?;

        let messages = table_columns(&conn, "messages")?;
        for column in ["tool_calls", "tool_call_id", "blocks", "prompt_id", "tools"] {
            ensure_column(&conn, "messages", &messages, column, "TEXT")?;
        }

        let sessions = table_columns(&conn, "sessions")?;
        ensure_column(&conn, "sessions", &sessions, "workspace_id", "TEXT")?;
        ensure_column(
            &conn,
            "sessions",
            &sessions,
            "archived",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&conn, "sessions", &sessions, "parent_session_id", "TEXT")?;
        ensure_column(&conn, "sessions", &sessions, "last_turn_reason", "TEXT")?;

        for (column, definition) in [
            ("title_kind", "TEXT"),
            ("last_prompt", "TEXT"),
            ("archived_at", "INTEGER"),
            ("forked_from", "TEXT"),
            ("version", "INTEGER"),
            ("cwd", "TEXT"),
            ("environment_id", "TEXT"),
            ("environment_cwd", "TEXT"),
            ("agents", "TEXT"),
            ("custom", "TEXT"),
        ] {
            ensure_column(&conn, "sessions", &sessions, column, definition)?;
        }

        let turns = table_columns(&conn, "turns")?;
        ensure_column(&conn, "turns", &turns, "origin", "TEXT")?;

        let wire = table_columns(&conn, "wire_events")?;
        ensure_column(&conn, "wire_events", &wire, "protocol_version", "TEXT")?;
        ensure_column(&conn, "wire_events", &wire, "min_protocol_version", "TEXT")?;

        conn.execute(
            "UPDATE sessions
                SET title_kind = CASE
                    WHEN title IS NULL OR trim(title) = '' THEN NULL
                    ELSE 'replaceable'
                END
              WHERE title_kind IS NULL",
            [],
        )?;
        conn.execute(
            "UPDATE sessions SET version = ?1 WHERE version IS NULL",
            params![SESSION_META_VERSION],
        )?;

        let file_history = table_columns(&conn, "session_file_history")?;
        ensure_column(
            &conn,
            "session_file_history",
            &file_history,
            "oversize",
            "INTEGER NOT NULL DEFAULT 0",
        )?;

        let state = table_columns(&conn, "state_entries")?;
        if !state.contains("session_id") {
            ensure_column(&conn, "state_entries", &state, "session_id", "TEXT")?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_state_entries_session ON state_entries(session_id)",
                [],
            )?;
        }

        Ok(Self {
            conn: Mutex::new(conn),
            blob_cache: crate::native::event_store::blobs::BlobCache::new(),
            path,
        })
    }

    const SESSION_SUMMARY_COLUMNS: &'static str = "session_id, title, created_at, updated_at, workspace_id, archived, parent_session_id, title_kind, last_prompt, archived_at, forked_from, version, cwd, environment_id, environment_cwd, agents, custom, last_turn_reason";

    pub fn create_session(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.create_session_with_workspace(session_id, title, None)
    }

    pub fn create_session_with_workspace(
        &self,
        session_id: &str,
        title: Option<&str>,
        workspace_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        Self::insert_session_row(&conn, session_id, title, workspace_id)
    }

    fn insert_session_row(
        conn: &Connection,
        session_id: &str,
        title: Option<&str>,
        workspace_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let now = chrono::Utc::now().timestamp_millis();

        conn.execute(
            "INSERT INTO sessions
                (session_id, title, created_at, updated_at, workspace_id, cwd, version, agents, custom)
             VALUES (?1, ?2, ?3, ?3, ?4,
                (SELECT root FROM workspaces WHERE workspace_id = ?4),
                ?5, '{}', '{}')
             ON CONFLICT(session_id) DO UPDATE SET
                title = coalesce(?2, title),
                updated_at = ?3,
                workspace_id = coalesce(?4, workspace_id),
                cwd = coalesce(cwd, (SELECT root FROM workspaces WHERE workspace_id = ?4)),
                version = coalesce(?5, version)",
            params![session_id, title, now, workspace_id, SESSION_META_VERSION],
        )?;
        Ok(())
    }

    pub fn set_session_workspace(
        &self,
        session_id: &str,
        workspace_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET
                workspace_id = ?2,
                -- v2 `SessionMeta.cwd` is the workspace root at creation, not
                -- whatever directory the daemon happened to start in.
                cwd = coalesce(
                    (SELECT root FROM workspaces WHERE workspace_id = ?2), cwd),
                updated_at = updated_at
             WHERE session_id = ?1",
            params![session_id, workspace_id],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM sessions ORDER BY updated_at DESC",
            Self::SESSION_SUMMARY_COLUMNS
        ))?;
        let rows = stmt.query_map([], session_summary_from_row)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM sessions WHERE session_id = ?1",
            Self::SESSION_SUMMARY_COLUMNS
        ))?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(session_summary_from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn export_session(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExport>, rusqlite::Error> {
        let session = match self.get_session(session_id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let messages = self.load_session_history(session_id)?;

        let turns_count: usize = {
            let folded = crate::session::wire_journal::fold_session(self, session_id)
                .map(|fold| fold.turns.len())
                .filter(|count| *count > 0);
            match folded {
                Some(count) => count,
                None => {
                    let conn = self.conn.lock();
                    conn.query_row(
                        "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
                        params![session_id],
                        |r| r.get(0),
                    )?
                }
            }
        };

        let (first_activity, last_activity): (Option<i64>, Option<i64>) = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT MIN(created_at), MAX(created_at) FROM wire_events WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
        };
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let wire_jsonl = {
            let conn = self.conn.lock();
            let records: Vec<(String, String)> = {
                let mut stmt = conn.prepare(
                    "SELECT event_type, payload FROM wire_events WHERE session_id = ?1 ORDER BY seq ASC",
                )?;
                let rows = stmt.query_map(params![session_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            records
                .into_iter()
                .map(|(event_type, payload)| {
                    wire_export_line(&event_type, &payload, &conn, &self.blob_cache)
                })
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let agent_wires = self.export_agent_wires(session_id);
        let wire_jsonl = agent_wires
            .iter()
            .find(|(agent_id, _)| agent_id == crate::callbacks::MAIN_AGENT_ID)
            .map(|(_, lines)| lines.clone())
            .unwrap_or(wire_jsonl);
        Ok(Some(SessionExport {
            first_activity_at: first_activity.or(Some(session.created_at)).map(format_iso),
            last_activity_at: last_activity.or(Some(session.updated_at)).map(format_iso),
            session,
            messages,
            turns_count,
            exported_at: now_iso,
            wire_jsonl,
            agent_wires,
        }))
    }

    fn export_agent_wires(&self, session_id: &str) -> Vec<(String, Vec<String>)> {
        let Some(agents_dir) = self
            .wire_journal_path(session_id, crate::callbacks::MAIN_AGENT_ID)
            .and_then(|path| {
                path.parent()
                    .and_then(|path| path.parent())
                    .map(Path::to_path_buf)
            })
        else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(&agents_dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let agent_id = entry.file_name().to_string_lossy().into_owned();
            let path = entry
                .path()
                .join(crate::session::wire_journal::WIRE_FILE_NAME);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut lines = Vec::new();
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(mut value) => {
                        let _ = self.rehydrate_payload(&mut value);
                        lines.push(
                            serde_json::to_string(&value).unwrap_or_else(|_| line.to_string()),
                        );
                    }
                    Err(_) => lines.push(line.to_string()),
                }
            }
            if !lines.is_empty() {
                out.push((agent_id, lines));
            }
        }
        out.sort_by(|left, right| left.0.cmp(&right.0));
        out
    }

    fn resolve_workspace_id(&self, root: &str) -> Result<String, rusqlite::Error> {
        let key = workspace_root_key(root);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT workspace_id, root FROM workspaces")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, row_root) = row?;
            if workspace_root_key(&row_root) == key {
                return Ok(id);
            }
        }
        Ok(encode_workdir_key(root))
    }

    pub fn create_workspace(
        &self,
        root: &str,
        name: Option<&str>,
    ) -> Result<WorkspaceSummary, rusqlite::Error> {
        let id = self.resolve_workspace_id(root)?;
        let normalized_root = root.replace('\\', "/").trim_end_matches('/').to_string();
        let base = normalized_root
            .split('/')
            .next_back()
            .unwrap_or(&normalized_root)
            .to_string();
        let ws_name = name.unwrap_or(&base);
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO workspaces (workspace_id, root, name, created_at, last_opened_at, trusted)
             VALUES (?1, ?2, ?3, ?4, ?4, 0)
             ON CONFLICT(workspace_id) DO UPDATE SET
                name = coalesce(?3, name),
                last_opened_at = ?4",
            params![id, normalized_root, ws_name, now],
        )?;

        let session_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE workspace_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);

        let (created_at, last_opened_at): (i64, i64) = conn.query_row(
            "SELECT created_at, last_opened_at FROM workspaces WHERE workspace_id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        Ok(WorkspaceSummary {
            id,
            root: normalized_root,
            name: ws_name.to_string(),
            created_at: format_iso(created_at),
            last_opened_at: format_iso(last_opened_at),
            session_count,
        })
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT w.workspace_id, w.root, w.name, w.created_at, w.last_opened_at,
                    (SELECT COUNT(*) FROM sessions s WHERE s.workspace_id = w.workspace_id)
             FROM workspaces w ORDER BY w.last_opened_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let root: String = row.get(1)?;
            let name: String = row.get(2)?;
            let created_at: i64 = row.get(3)?;
            let last_opened_at: i64 = row.get(4)?;
            let session_count: usize = row.get(5)?;
            Ok(WorkspaceSummary {
                id,
                root,
                name,
                created_at: format_iso(created_at),
                last_opened_at: format_iso(last_opened_at),
                session_count,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT w.workspace_id, w.root, w.name, w.created_at, w.last_opened_at,
                    (SELECT COUNT(*) FROM sessions s WHERE s.workspace_id = w.workspace_id)
             FROM workspaces w WHERE w.workspace_id = ?1",
        )?;
        let mut rows = stmt.query(params![workspace_id])?;
        if let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let root: String = row.get(1)?;
            let name: String = row.get(2)?;
            let created_at: i64 = row.get(3)?;
            let last_opened_at: i64 = row.get(4)?;
            let session_count: usize = row.get(5)?;
            Ok(Some(WorkspaceSummary {
                id,
                root,
                name,
                created_at: format_iso(created_at),
                last_opened_at: format_iso(last_opened_at),
                session_count,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn update_workspace_name(
        &self,
        workspace_id: &str,
        name: &str,
    ) -> Result<Option<WorkspaceSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE workspaces SET name = ?1, last_opened_at = ?2 WHERE workspace_id = ?3",
            params![name, chrono::Utc::now().timestamp_millis(), workspace_id],
        )?;
        drop(conn);
        if affected > 0 {
            self.get_workspace(workspace_id)
        } else {
            Ok(None)
        }
    }

    pub fn delete_workspace(&self, workspace_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "DELETE FROM workspaces WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        Ok(affected > 0)
    }

    pub fn is_workspace_trusted(&self, workspace_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let trusted: Option<i64> = conn
            .query_row(
                "SELECT trusted FROM workspaces WHERE workspace_id = ?1",
                params![workspace_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(trusted.map(|v| v != 0).unwrap_or(false))
    }

    pub fn set_workspace_trusted(
        &self,
        workspace_id: &str,
        trusted: bool,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE workspaces SET trusted = ?2 WHERE workspace_id = ?1",
            params![workspace_id, if trusted { 1 } else { 0 }],
        )?;
        Ok(affected > 0)
    }

    pub fn blob_usage(&self) -> Result<BlobUsage, rusqlite::Error> {
        let conn = self.conn.lock();
        let stored: u64 =
            conn.query_row("SELECT COUNT(*) FROM wire_blobs", [], |row| row.get(0))?;
        let bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(bytes)), 0) FROM wire_blobs",
            [],
            |row| row.get(0),
        )?;
        let mut unreferenced = 0u64;
        let mut scanned = 0u64;
        {
            let mut blobs =
                conn.prepare("SELECT hash FROM wire_blobs ORDER BY hash ASC LIMIT ?1")?;
            let hashes = blobs.query_map(params![BLOB_AUDIT_MAX_ROWS as i64], |row| {
                row.get::<_, String>(0)
            })?;
            let mut referenced = conn.prepare(
                "SELECT EXISTS(SELECT 1 FROM wire_events WHERE payload LIKE '%' || ?1 || '%')
                     OR EXISTS(SELECT 1 FROM state_entries WHERE value LIKE '%' || ?1 || '%')",
            )?;
            for hash in hashes {
                let hash = hash?;
                scanned += 1;
                let used: bool = referenced.query_row(params![hash], |row| row.get(0))?;
                if !used {
                    unreferenced += 1;
                }
            }
        }
        Ok(BlobUsage {
            stored,
            bytes: u64::try_from(bytes).unwrap_or(0),
            unreferenced,
            scanned,
            truncated: stored > scanned,
        })
    }

    fn ensure_not_occupied(session_id: &str) -> Result<(), SessionRemovalError> {
        match crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id) {
            Some(holder) => Err(SessionRemovalError::Occupied {
                message: crate::session::lock::session_occupied_message(Some(&holder)),
            }),
            None => Ok(()),
        }
    }

    pub fn delete_session(&self, session_id: &str) -> Result<bool, SessionRemovalError> {
        Self::ensure_not_occupied(session_id)?;
        let conn = self.conn.lock();

        let file_history_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM session_file_history WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "DELETE FROM state_entries
             WHERE session_id = ?1
                OR (domain IN ('agent_config', 'metadata') AND key = ?1)",
            params![session_id],
        )?;
        let affected = conn.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;

        if affected > 0 && file_history_rows > 0 {
            reclaim_file_history_space(&conn)?;
        }
        Ok(affected > 0)
    }

    pub fn fork_session(
        &self,
        source_session_id: &str,
        new_session_id: &str,
        title: Option<&str>,
        turn_index: Option<u64>,
    ) -> Result<bool, ForkError> {
        self.fork_session_with_metadata(source_session_id, new_session_id, title, turn_index, None)
    }

    pub fn fork_session_with_metadata(
        &self,
        source_session_id: &str,
        new_session_id: &str,
        title: Option<&str>,
        turn_index: Option<u64>,
        metadata: Option<&Value>,
    ) -> Result<bool, ForkError> {
        let source = self.get_session(source_session_id)?;
        let Some(source) = source else {
            return Ok(false);
        };
        let fold = crate::session::wire_journal::fold_session(self, source_session_id)
            .filter(|fold| !fold.turns.is_empty());
        let (turns, messages, fork_records) = if let Some(fold) = &fold {
            let source_turns = self.list_turns(source_session_id)?;
            let turns = Self::fold_turn_records(fold, &source_turns);
            let messages = Self::fold_stored_messages(fold, &turns);
            (turns, messages, Some(fold.records.clone()))
        } else {
            (
                self.list_turns(source_session_id)?,
                self.load_session_messages(source_session_id)?,
                None,
            )
        };
        let opening_role = |turn_id: &str| -> bool {
            messages
                .iter()
                .find(|m| m.turn_id == turn_id)
                .is_some_and(|m| m.message.role == kimi_core::types::Role::User)
        };

        let visible: Vec<usize> = turns
            .iter()
            .enumerate()
            .filter(|(_, t)| turn_is_user_visible(t.origin.as_ref(), opening_role(&t.turn_id)))
            .map(|(i, _)| i)
            .collect();
        let retained_upto_row = match turn_index {
            None => turns.len(),
            Some(index) => {
                let start = usize::try_from(index)
                    .ok()
                    .and_then(|i| visible.get(i).copied());
                match start {
                    None => {
                        return Err(ForkError::TurnNotFound {
                            turn_index: index,
                            available_turns: visible.len(),
                        });
                    }
                    Some(start_row) => start_row + 1,
                }
            }
        };

        let retained: Vec<(&TurnRecord, Vec<LLMMessage>)> = turns
            .iter()
            .take(retained_upto_row)
            .map(|turn| {
                let turn_messages: Vec<LLMMessage> = messages
                    .iter()
                    .filter(|m| m.turn_id == turn.turn_id)
                    .map(|m| m.message.clone())
                    .collect();
                (turn, turn_messages)
            })
            .filter(|(_, msgs)| !msgs.is_empty())
            .collect();

        let effective_title = title.or(source.title.as_deref());

        let last_prompt = retained
            .iter()
            .rev()
            .flat_map(|(_, msgs)| msgs.iter().rev())
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .filter(|text| !text.trim().is_empty());

        let title_kind = if title.is_some() {
            Some("custom")
        } else {
            source.normalized_title_kind()
        };
        let mut custom = source
            .custom
            .clone()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(extra) = metadata.and_then(Value::as_object)
            && let Some(object) = custom.as_object_mut()
        {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }

        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let cut_line = match turn_index {
            None => None,
            Some(index) => {
                let keep = usize::try_from(index)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1);
                let anchors = Self::fork_anchor_lines(&tx, source_session_id)?;
                if keep < anchors.len() {
                    Some(anchors[keep])
                } else {
                    None
                }
            }
        };
        Self::insert_session_row(
            &tx,
            new_session_id,
            effective_title,
            source.workspace_id.as_deref(),
        )?;
        tx.execute(
            "UPDATE sessions
                SET title_kind = ?2, last_prompt = ?3, forked_from = ?4,
                    version = ?5, cwd = ?6, environment_id = ?7,
                    environment_cwd = ?8, agents = ?9, custom = ?10,
                    parent_session_id = ?4
              WHERE session_id = ?1",
            params![
                new_session_id,
                title_kind,
                last_prompt,
                source_session_id,
                SESSION_META_VERSION,
                source.cwd,
                source.environment_id,
                source.environment_cwd,
                source
                    .agents
                    .as_ref()
                    .map(|agents| encode_stored_json(agents, "fork agents"))
                    .transpose()?,
                encode_stored_json(&custom, "fork custom")?,
            ],
        )?;

        for (number, (turn, turn_messages)) in retained.iter().enumerate() {
            Self::save_turn_in(
                &tx,
                new_session_id,
                &forked_turn_id(&turn.turn_id, new_session_id, number as u32 + 1),
                number as u32 + 1,
                turn_messages,
                turn.usage.as_ref(),
                turn.origin.as_ref(),
            )?;
        }

        if turn_index.is_none() {
            let mut copy = tx.prepare(
                "INSERT INTO session_file_history
                     (session_id, turn_id, path, status, content_before, content_after,
                      additions, deletions, oversize, created_at)
                 SELECT ?2, ?3, path, status, content_before, content_after,
                        additions, deletions, oversize, created_at
                   FROM session_file_history
                  WHERE session_id = ?1 AND turn_id = ?4
                  ORDER BY id ASC",
            )?;
            for (number, (turn, _)) in retained.iter().enumerate() {
                copy.execute(params![
                    source_session_id,
                    new_session_id,
                    number as i64 + 1,
                    i64::from(turn.turn_number)
                ])?;
            }
        }

        tx.execute(
            "INSERT INTO wire_events
                 (id, session_id, event_type, payload, is_checkpoint, is_compaction,
                  created_at, protocol_version, min_protocol_version)
             SELECT ?2 || ':' || id, ?2, event_type, payload, is_checkpoint, is_compaction,
                    created_at, protocol_version, min_protocol_version
               FROM (
                     SELECT seq, id, event_type, payload, is_checkpoint, is_compaction,
                            created_at, protocol_version, min_protocol_version,
                            ROW_NUMBER() OVER (ORDER BY seq ASC) AS line
                       FROM wire_events
                      WHERE session_id = ?1
               )
              WHERE ?3 IS NULL OR line < ?3
              ORDER BY seq ASC",
            params![source_session_id, new_session_id, cut_line],
        )?;
        tx.commit()?;
        drop(conn);
        if let Some(path) = self.wire_journal_path(new_session_id, crate::callbacks::MAIN_AGENT_ID)
        {
            let records = match fork_records {
                Some(records) => {
                    let cut = fold
                        .as_ref()
                        .and_then(|fold| fold.turns.get(retained_upto_row).map(|turn| turn.start))
                        .unwrap_or(records.len())
                        .min(records.len());
                    let mut records = records[..cut].to_vec();
                    records.retain(|record| {
                        record.get("type").and_then(Value::as_str) != Some("agent.switched")
                    });
                    records
                }
                None => crate::session::wire_journal::records_from_store(
                    self,
                    new_session_id,
                    crate::callbacks::MAIN_AGENT_ID,
                ),
            };
            if !records.is_empty()
                && let Err(error) =
                    crate::session::wire_journal::write_journal_file(&path, &records)
            {
                tracing::warn!(
                    %new_session_id,
                    %error,
                    "could not materialize the fork's v2 wire journal"
                );
            }
        }
        Ok(true)
    }

    fn fold_turn_records(
        fold: &crate::session::wire_journal::RestorableFold,
        source_turns: &[TurnRecord],
    ) -> Vec<TurnRecord> {
        fold.turns
            .iter()
            .enumerate()
            .map(|(index, turn)| {
                let number = turn
                    .turn_id
                    .filter(|id| *id > 0)
                    .unwrap_or(index as i64 + 1) as u32;
                let source = source_turns
                    .iter()
                    .find(|record| record.turn_number == number);
                TurnRecord {
                    turn_id: format!("fold-turn-{index}"),
                    turn_number: number,
                    status: "completed".to_string(),
                    started_at: source.map(|record| record.started_at).unwrap_or(0),
                    completed_at: source.and_then(|record| record.completed_at),
                    usage: source.and_then(|record| record.usage.clone()),
                    origin: turn
                        .origin
                        .clone()
                        .or_else(|| source.and_then(|record| record.origin.clone())),
                }
            })
            .collect()
    }

    fn fold_stored_messages(
        fold: &crate::session::wire_journal::RestorableFold,
        turns: &[TurnRecord],
    ) -> Vec<StoredMessage> {
        fold.turns
            .iter()
            .zip(turns)
            .flat_map(|(turn, record)| {
                let end = turn.last_message.min(fold.history.len());
                let start = turn.first_message.min(end);
                fold.history[start..end]
                    .iter()
                    .cloned()
                    .map(move |message| StoredMessage {
                        message,
                        created_at: 0,
                        turn_id: record.turn_id.clone(),
                    })
            })
            .collect()
    }

    fn fork_anchor_lines(conn: &Connection, session_id: &str) -> rusqlite::Result<Vec<i64>> {
        let mut stmt = conn.prepare(
            "SELECT event_type, payload, ROW_NUMBER() OVER (ORDER BY seq ASC)
               FROM wire_events
              WHERE session_id = ?1
              ORDER BY seq ASC",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        let mut prompts = Vec::new();
        let mut native = Vec::new();
        while let Some(row) = rows.next()? {
            let event_type: String = row.get(0)?;
            let payload = decode_stored_json(&row.get::<_, String>(1)?, "wire payload")?;
            let line: i64 = row.get(2)?;
            if event_type == "message.user" {
                native.push(line);
            } else if wire_row_is_fork_anchor(&event_type, &payload) {
                prompts.push(line);
            }
        }
        Ok(if prompts.is_empty() { native } else { prompts })
    }

    pub fn archive_session(&self, session_id: &str) -> Result<bool, SessionRemovalError> {
        Self::ensure_not_occupied(session_id)?;
        Ok(self.set_session_archived(session_id, true)?)
    }

    pub fn restore_session(&self, session_id: &str) -> Result<bool, SessionRemovalError> {
        Self::ensure_not_occupied(session_id)?;
        Ok(self.set_session_archived(session_id, false)?)
    }

    pub fn list_children(&self, session_id: &str) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM sessions WHERE parent_session_id = ?1 ORDER BY updated_at DESC",
            Self::SESSION_SUMMARY_COLUMNS
        ))?;
        let rows = stmt.query_map(params![session_id], session_summary_from_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn set_parent_session_id(
        &self,
        session_id: &str,
        parent_session_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET parent_session_id = ?1 WHERE session_id = ?2",
            params![parent_session_id, session_id],
        )?;
        Ok(())
    }

    pub fn generate_title(
        &self,
        session_id: &str,
        source: Option<&str>,
    ) -> Result<Option<String>, String> {
        let history = self
            .load_session_history(session_id)
            .map_err(|e| e.to_string())?;
        derive_session_title(&history, source)
    }

    fn select_undo_turns(
        conn: &rusqlite::Connection,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<(String, i64)>, String> {
        if count == 0 {
            return Err("undo refused: count must be a positive integer".into());
        }

        let mut stmt = conn
            .prepare(
                "SELECT t.turn_id, t.turn_number, t.origin,
                        (SELECT m.role FROM messages m
                          WHERE m.session_id = t.session_id AND m.turn_id = t.turn_id
                          ORDER BY m.id ASC LIMIT 1) AS opening_role
                 FROM turns t
                 WHERE t.session_id = ?1
                 ORDER BY t.turn_number DESC",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![session_id]).map_err(|e| e.to_string())?;

        let mut selected: Vec<(String, i64)> = Vec::new();
        let mut anchors = 0usize;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let turn_id: String = row.get(0).map_err(|e| e.to_string())?;
            let turn_number: i64 = row.get(1).map_err(|e| e.to_string())?;
            let origin: Option<Value> = row
                .get::<_, Option<String>>(2)
                .map_err(|e| e.to_string())?
                .and_then(|raw| serde_json::from_str(&raw).ok());
            let opening_role: Option<String> = row.get(3).map_err(|e| e.to_string())?;
            selected.push((turn_id.clone(), turn_number));

            if turn_is_compaction(&turn_id) {
                return Err(format!(
                    "undo refused: crossing the compaction boundary turn '{COMPACT_TURN_ID}' would corrupt the session projection"
                ));
            }
            if opening_role.as_deref() == Some("user")
                && turn_origin_is_undo_anchor(origin.as_ref())
            {
                anchors += 1;
                if anchors >= count {
                    break;
                }
            }
        }

        if anchors < count {
            let message = if anchors == 0 {
                "undo refused: nothing to undo (no user turn)".to_string()
            } else {
                format!("undo refused: only {anchors} of {count} requested turn(s) available")
            };
            return Err(message);
        }
        Ok(selected)
    }

    pub fn plan_undo_turns(&self, session_id: &str, count: usize) -> Result<Vec<i64>, String> {
        Ok(self
            .select_undo_turns_for(session_id, count)?
            .into_iter()
            .map(|(_, turn_number)| turn_number)
            .collect())
    }

    pub fn refresh_last_prompt_after_undo(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, String> {
        let folded = if crate::session::wire_journal::journal_authoritative(self, session_id) {
            crate::session::wire_journal::fold_session(self, session_id)
        } else {
            None
        };
        let last_prompt = if let Some(fold) = folded {
            fold.history
                .iter()
                .rev()
                .filter(|message| crate::compaction::is_real_user_input(message))
                .map(|message| message.content.clone())
                .find(|text| !text.trim().is_empty())
        } else {
            let conn = self.conn.lock();
            let mut stmt = conn
                .prepare(
                    "SELECT t.origin, m.content
                       FROM messages m
                       JOIN turns t ON t.session_id = m.session_id AND t.turn_id = m.turn_id
                      WHERE m.session_id = ?1 AND m.role = 'user'
                      ORDER BY m.id DESC",
                )
                .map_err(|e| e.to_string())?;
            let mut rows = stmt.query(params![session_id]).map_err(|e| e.to_string())?;
            let mut found: Option<String> = None;
            while let Some(row) = rows.next().map_err(|e| e.to_string())? {
                let origin: Option<Value> = row
                    .get::<_, Option<String>>(0)
                    .map_err(|e| e.to_string())?
                    .and_then(|raw| serde_json::from_str(&raw).ok());
                if !turn_origin_is_undo_anchor(origin.as_ref()) {
                    continue;
                }
                let content: String = row.get(1).map_err(|e| e.to_string())?;
                if content.trim().is_empty() {
                    continue;
                }
                found = Some(content);
                break;
            }
            found
        };
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "UPDATE sessions SET last_prompt = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, last_prompt, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(last_prompt)
    }

    pub fn undo_turns(&self, session_id: &str, count: usize) -> Result<usize, String> {
        let selected = self.select_undo_turns_for(session_id, count)?;
        let journal_backed = crate::session::wire_journal::journal_authoritative(self, session_id);
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        for (turn_id, turn_number) in &selected {
            tx.execute(
                "DELETE FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2",
                params![session_id, turn_number],
            )
            .map_err(|e| e.to_string())?;
            if !journal_backed {
                tx.execute(
                    "DELETE FROM messages WHERE session_id = ?1 AND turn_id = ?2",
                    params![session_id, turn_id],
                )
                .map_err(|e| e.to_string())?;
                tx.execute(
                    "DELETE FROM turns WHERE turn_id = ?1 AND session_id = ?2",
                    params![turn_id, session_id],
                )
                .map_err(|e| e.to_string())?;
            }
        }

        let now = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;

        Ok(selected.len())
    }

    fn select_undo_turns_for(
        &self,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<(String, i64)>, String> {
        if crate::session::wire_journal::journal_authoritative(self, session_id) {
            return self.select_undo_turns_fold(session_id, count);
        }
        let conn = self.conn.lock();
        Self::select_undo_turns(&conn, session_id, count)
    }

    fn select_undo_turns_fold(
        &self,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<(String, i64)>, String> {
        if count == 0 {
            return Err("undo refused: count must be a positive integer".into());
        }
        let Some(fold) = crate::session::wire_journal::fold_session(self, session_id) else {
            return Err("undo refused: nothing to undo (no user turn)".into());
        };
        let mut remaining = count;
        let mut indices = Vec::new();
        for index in (0..fold.history.len()).rev() {
            let message = &fold.history[index];
            if message
                .origin
                .as_ref()
                .and_then(|origin| origin.get("kind"))
                .and_then(Value::as_str)
                == Some("injection")
            {
                continue;
            }
            if crate::compaction::is_compaction_summary_message(message) {
                return Err(format!(
                    "undo refused: crossing the compaction boundary turn '{COMPACT_TURN_ID}' would corrupt the session projection"
                ));
            }
            if crate::compaction::is_real_user_input(message) {
                remaining -= 1;
                indices.push(index);
                if remaining == 0 {
                    break;
                }
            }
        }
        if remaining > 0 {
            let available = count - remaining;
            return Err(if available == 0 {
                "undo refused: nothing to undo (no user turn)".to_string()
            } else {
                format!("undo refused: only {available} of {count} requested turn(s) available")
            });
        }
        let mut selected = Vec::new();
        for index in indices {
            let Some((position, turn)) = fold.turns.iter().enumerate().rev().find(|(_, turn)| {
                turn.first_message <= index && index < turn.last_message.max(turn.first_message + 1)
            }) else {
                continue;
            };
            let number = turn.turn_id.unwrap_or(position as i64 + 1);
            selected.push((format!("turn-{number}"), number));
        }
        Ok(selected)
    }

    pub fn compact_session(
        &self,
        session_id: &str,
        instruction: Option<&str>,
    ) -> Result<CompactionReport, String> {
        Ok(self
            .compact_session_with_summary(session_id, instruction, None)?
            .0)
    }

    pub fn compact_session_with_summary(
        &self,
        session_id: &str,
        instruction: Option<&str>,
        summary: Option<String>,
    ) -> Result<(CompactionReport, Vec<LLMMessage>), String> {
        let history = self
            .load_session_history(session_id)
            .map_err(|e| e.to_string())?;
        if history.len() <= 2 {
            return Ok((CompactionReport::skipped(), history));
        }
        let config = crate::compaction::CompactionConfig::default();
        let count = crate::compaction::compute_compact_count_manual(&history, &config);
        if count == 0 {
            return Ok((CompactionReport::skipped(), history));
        }
        let summary = summary
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| crate::compaction::CompactionError::EmptySummary.to_string())?;
        let compacted =
            crate::compaction::apply_compaction_with_summary(&history, count, summary, false);
        if compacted.len() >= history.len() {
            return Ok((CompactionReport::skipped(), history));
        }
        let removed = history.len() - compacted.len();
        let report = CompactionReport {
            messages_before: history.len(),
            removed,
            compacted_count: count,
            tokens_before: crate::compaction::estimate_messages_tokens(&history),
            tokens_after: crate::compaction::estimate_messages_tokens(&compacted),
            kept_user_message_count: compacted
                .iter()
                .filter(|message| message.role == kimi_core::types::Role::User)
                .count(),
        };
        self.save_checkpoint(
            session_id,
            &format!(
                "compaction-{session_id}-{}",
                chrono::Utc::now().timestamp_millis()
            ),
            "compaction-boundary",
            &json!({
                "messages_before": report.messages_before,
                "removed": report.removed,
                "tokens_before": report.tokens_before,
                "tokens_after": report.tokens_after,
                "kept_user_message_count": report.kept_user_message_count,
                "instruction": instruction,
            }),
        )
        .map_err(|e| e.to_string())?;
        if !crate::session::wire_journal::journal_authoritative(self, session_id) {
            let conn = self.conn.lock();
            conn.execute(
                "DELETE FROM messages WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| e.to_string())?;
            drop(conn);
            self.save_turn(session_id, COMPACT_TURN_ID, 1, &compacted, None, None)
                .map_err(|e| e.to_string())?;
        }
        Ok((report, compacted))
    }

    pub fn update_session_title(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE sessions SET title = ?2, title_kind = 'custom', version = ?3 WHERE session_id = ?1",
            params![session_id, title, SESSION_META_VERSION],
        )?;
        Ok(affected > 0)
    }

    pub fn set_generated_title_if_uncustomized(
        &self,
        session_id: &str,
        title: &str,
        force: bool,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = if force {
            conn.execute(
                "UPDATE sessions SET title = ?2, title_kind = 'generated', version = ?3 WHERE session_id = ?1",
                params![session_id, title, SESSION_META_VERSION],
            )?
        } else {
            conn.execute(
                "UPDATE sessions
                    SET title = ?2, title_kind = 'generated', version = ?3
                  WHERE session_id = ?1
                    AND (title_kind IS NULL OR title_kind <> 'custom')",
                params![session_id, title, SESSION_META_VERSION],
            )?
        };
        Ok(affected > 0)
    }

    pub fn set_session_archived(
        &self,
        session_id: &str,
        archived: bool,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let affected = if archived {
            conn.execute(
                "UPDATE sessions SET archived = 1, archived_at = ?2, version = ?3 WHERE session_id = ?1",
                params![session_id, now, SESSION_META_VERSION],
            )?
        } else {
            conn.execute(
                "UPDATE sessions SET archived = 0, archived_at = NULL, version = ?2 WHERE session_id = ?1",
                params![session_id, SESSION_META_VERSION],
            )?
        };
        Ok(affected > 0)
    }

    pub fn migrate_legacy_session_metadata(
        &self,
        session_id: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let current: Option<(Option<String>, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT title, title_kind, cwd FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((title, title_kind, cwd)) = current else {
            return Ok(false);
        };
        let stored: Option<String> = conn
            .query_row(
                "SELECT value FROM state_entries WHERE domain = 'metadata' AND key = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut metadata =
            match decode_json_column::<Value>(stored, format!("session {session_id} metadata"))? {
                Some(Value::Object(map)) => map,
                _ => serde_json::Map::new(),
            };

        let mut next_cwd = cwd.clone();
        if next_cwd.is_none()
            && let Some(work_dir) = metadata
                .get("workDir")
                .and_then(Value::as_str)
                .filter(|dir| !dir.trim().is_empty())
        {
            next_cwd = Some(work_dir.to_string());
        }

        let mut next_title = title.clone();
        let mut next_kind: Option<String> = match title_kind.as_deref() {
            Some("custom") => Some("custom".into()),
            Some("generated") => Some("generated".into()),
            Some("replaceable") => Some("replaceable".into()),
            _ => None,
        };
        if next_kind.is_none() {
            let legacy_custom = metadata.get("isCustomTitle").and_then(Value::as_bool);
            let legacy_kind: Option<String> = metadata
                .get("titleKind")
                .and_then(Value::as_str)
                .filter(|kind| matches!(*kind, "custom" | "generated" | "replaceable"))
                .map(str::to_string);
            if title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                next_kind = Some(match legacy_custom {
                    Some(true) => "custom".to_string(),
                    _ if legacy_kind.is_some() => legacy_kind.unwrap_or_default(),
                    _ => "replaceable".to_string(),
                });
            } else if let Some(custom_title) = metadata.get("customTitle").and_then(Value::as_str) {
                next_title = Some(custom_title.to_string());
                next_kind = Some("custom".to_string());
            } else if legacy_kind.is_some() {
                next_kind = legacy_kind;
            }
        }

        let next_custom = next_kind.as_deref() == Some("custom");
        let legacy_present = ["workDir", "customTitle", "titleSource"]
            .iter()
            .any(|key| metadata.contains_key(*key));
        metadata.remove("workDir");
        metadata.remove("customTitle");
        metadata.remove("titleSource");
        let metadata_changed = legacy_present
            || metadata.get("isCustomTitle").and_then(Value::as_bool) != Some(next_custom)
            || metadata.get("titleKind").and_then(Value::as_str) != next_kind.as_deref();
        metadata.insert("isCustomTitle".into(), Value::Bool(next_custom));
        match next_kind.as_deref() {
            Some(kind) => {
                metadata.insert("titleKind".into(), Value::String(kind.to_string()));
            }
            None => {
                metadata.remove("titleKind");
            }
        }

        let title_changed = next_title != title || next_kind != title_kind;
        let cwd_changed = next_cwd != cwd;
        if !title_changed && !cwd_changed && !metadata_changed {
            return Ok(false);
        }
        if title_changed || cwd_changed {
            conn.execute(
                "UPDATE sessions
                    SET title = ?2, title_kind = ?3, cwd = ?4, version = ?5
                  WHERE session_id = ?1",
                params![
                    session_id,
                    next_title,
                    next_kind,
                    next_cwd,
                    SESSION_META_VERSION
                ],
            )?;
        }
        let encoded = encode_stored_json(
            &Value::Object(metadata),
            format!("session {session_id} metadata"),
        )?;
        conn.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at, session_id)
             VALUES ('metadata', ?1, ?2, ?3, ?1)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?2,
                updated_at = ?3,
                session_id = ?1",
            params![session_id, encoded, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(true)
    }

    pub fn put_session_metadata(
        &self,
        session_id: &str,
        metadata: &Value,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        let kind: Option<Option<String>> = conn
            .query_row(
                "SELECT title_kind FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let mut metadata = metadata.clone();
        if let Some(kind) = kind
            && let Some(map) = metadata.as_object_mut()
        {
            let custom = matches!(kind.as_deref(), Some("custom"))
                || (kind.is_none()
                    && map.get("isCustomTitle").and_then(Value::as_bool) == Some(true));
            map.insert("isCustomTitle".into(), Value::Bool(custom));
        }
        let value = dehydrate_state_value(&metadata, &conn, &self.blob_cache)?;
        let encoded =
            encode_stored_json(value.as_ref(), format!("state entry metadata/{session_id}"))?;
        conn.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at, session_id)
             VALUES ('metadata', ?1, ?2, ?3, ?1)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?2,
                updated_at = ?3,
                session_id = ?1",
            params![session_id, encoded, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(())
    }

    pub fn register_session_agent(
        &self,
        session_id: &str,
        agent_id: &str,
        meta: &Value,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT agents FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = current else {
            return Ok(false);
        };
        let mut agents = decode_json_column::<Value>(raw, format!("session {session_id} agents"))?
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let Some(object) = agents.as_object_mut() else {
            return Ok(false);
        };
        if object.get(agent_id) == Some(meta) {
            return Ok(false);
        }
        object.insert(agent_id.to_string(), meta.clone());
        let encoded = encode_stored_json(&agents, format!("session {session_id} agents"))?;
        conn.execute(
            "UPDATE sessions SET agents = ?2, version = ?3 WHERE session_id = ?1",
            params![session_id, encoded, SESSION_META_VERSION],
        )?;
        Ok(true)
    }

    pub fn set_session_custom(
        &self,
        session_id: &str,
        custom: &Value,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let encoded = encode_stored_json(custom, format!("session {session_id} custom"))?;
        let affected = conn.execute(
            "UPDATE sessions SET custom = ?2, version = ?3 WHERE session_id = ?1",
            params![session_id, encoded, SESSION_META_VERSION],
        )?;
        Ok(affected > 0)
    }

    pub fn set_session_environment(
        &self,
        session_id: &str,
        environment_id: Option<&str>,
        environment_cwd: Option<&str>,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "UPDATE sessions
                SET environment_id = ?2, environment_cwd = ?3, version = ?4
              WHERE session_id = ?1",
            params![
                session_id,
                environment_id,
                environment_cwd,
                SESSION_META_VERSION
            ],
        )?;
        Ok(affected > 0)
    }

    pub fn set_session_environment_snapshot(
        &self,
        session_id: &str,
        environment_id: &str,
        environment_cwd: Option<&str>,
        env: &std::collections::BTreeMap<String, String>,
    ) -> Result<bool, rusqlite::Error> {
        let updated =
            self.set_session_environment(session_id, Some(environment_id), environment_cwd)?;
        let value = serde_json::json!({
            "environmentId": environment_id,
            "cwd": environment_cwd,
            "env": env,
        });
        let encoded = encode_stored_json(&value, format!("environment/{session_id}"))?;
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at, session_id)
             VALUES ('environment', ?1, ?2, ?3, ?1)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?2,
                updated_at = ?3,
                session_id = ?1",
            params![session_id, encoded, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(updated)
    }

    pub fn get_session_environment_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Option<crate::environment::StoredEnvironmentSnapshot>, rusqlite::Error> {
        let Some(session) = self.get_session(session_id)? else {
            return Ok(None);
        };
        let Some(environment_id) = session.environment_id else {
            return Ok(None);
        };

        let cwd = session.environment_cwd.or(session.cwd);
        let env = match self.get_state("environment", session_id)? {
            Some(value) => value
                .get("env")
                .and_then(serde_json::Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter_map(|(key, value)| {
                            value.as_str().map(|value| (key.clone(), value.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            None => std::collections::BTreeMap::new(),
        };
        Ok(Some(crate::environment::StoredEnvironmentSnapshot {
            binding: crate::environment::EnvironmentBinding {
                environment_id,
                cwd: cwd.map(std::path::PathBuf::from),
            },
            env,
        }))
    }

    pub fn reconcile_last_turn_reason(
        &self,
        session_id: &str,
    ) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.conn.lock();
        let current: Option<Option<String>> = conn
            .query_row(
                "SELECT last_turn_reason FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(persisted) = current.clone() else {
            return Ok(None);
        };
        let turn_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let reconciled = if turn_count == 0 {
            None
        } else if persisted.is_some() {
            persisted
        } else {
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM turns WHERE session_id = ?1 ORDER BY turn_number DESC LIMIT 1",
                    params![session_id],
                    |row| row.get(0),
                )
                .optional()?;

            status.filter(|status| {
                let status = status.as_str();
                !status.is_empty() && status != "running" && status != "in_progress"
            })
        };
        if reconciled != current.flatten() {
            conn.execute(
                "UPDATE sessions SET last_turn_reason = ?2 WHERE session_id = ?1",
                params![session_id, reconciled],
            )?;
        }
        Ok(reconciled)
    }

    pub fn next_turn_number(&self, session_id: &str) -> Result<u32, rusqlite::Error> {
        let conn = self.conn.lock();
        let highest: i64 = conn.query_row(
            "SELECT COALESCE(MAX(turn_number), 0) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let table_next = highest.max(0) as u32 + 1;
        drop(conn);
        let fold_next = crate::session::wire_journal::fold_session(self, session_id)
            .map(|fold| fold.next_turn_id.max(0) as u32)
            .unwrap_or(0);
        Ok(table_next.max(fold_next))
    }

    pub fn load_session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredMessage>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT role, content, tool_calls, tool_call_id, blocks, prompt_id, created_at, turn_id, tools FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let role = row.get::<_, String>(0)?;
            let role = serde_json::from_value(Value::String(role)).map_err(|source| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(source),
                )
            })?;
            Ok(StoredMessage {
                message: LLMMessage {
                    role,
                    content: row.get(1)?,
                    blocks: decode_json_column(
                        row.get::<_, Option<String>>(4)?,
                        format!("message blocks in session {session_id}"),
                    )?
                    .unwrap_or_default(),
                    tool_calls: decode_json_column(
                        row.get::<_, Option<String>>(2)?,
                        format!("message tool_calls in session {session_id}"),
                    )?
                    .unwrap_or_default(),
                    tool_call_id: row.get(3)?,
                    prompt_id: row.get(5)?,
                    origin: None,
                    tools: decode_json_column(
                        row.get::<_, Option<String>>(8)?,
                        format!("message tools in session {session_id}"),
                    )?
                    .unwrap_or_default(),
                },
                created_at: row.get(6)?,
                turn_id: row.get(7)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_turns(&self, session_id: &str) -> Result<Vec<TurnRecord>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT turn_id, turn_number, status, started_at, completed_at, usage, origin
             FROM turns WHERE session_id = ?1 ORDER BY turn_number ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let turn_id: String = row.get(0)?;
            let usage: Option<String> = row.get(5)?;
            let origin: Option<String> = row.get(6)?;
            Ok(TurnRecord {
                turn_id: turn_id.clone(),
                turn_number: row.get::<_, i64>(1)?.max(0) as u32,
                status: row.get(2)?,
                started_at: row.get(3)?,
                completed_at: row.get(4)?,
                usage: decode_json_column(usage, format!("turn {turn_id} usage"))?,
                origin: decode_json_column(origin, format!("turn {turn_id} origin"))?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn save_turn(
        &self,
        session_id: &str,
        turn_id: &str,
        turn_number: u32,
        messages: &[LLMMessage],
        usage: Option<&TokenUsage>,
        origin: Option<&Value>,
    ) -> Result<(), rusqlite::Error> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        Self::save_turn_in(
            &tx,
            session_id,
            turn_id,
            turn_number,
            messages,
            usage,
            origin,
        )?;
        tx.commit()
    }

    fn save_turn_in(
        tx: &rusqlite::Transaction<'_>,
        session_id: &str,
        turn_id: &str,
        turn_number: u32,
        messages: &[LLMMessage],
        usage: Option<&TokenUsage>,
        origin: Option<&Value>,
    ) -> Result<(), rusqlite::Error> {
        let now = chrono::Utc::now().timestamp_millis();

        tx.execute(
            "INSERT INTO sessions (session_id, title, created_at, updated_at)
             VALUES (?1, NULL, ?2, ?2)
             ON CONFLICT(session_id) DO UPDATE SET updated_at = ?2",
            params![session_id, now],
        )?;

        let usage_json = usage
            .map(|u| encode_stored_json(u, format!("turn {turn_id} usage")))
            .transpose()?;

        let origin_json = origin
            .map(|o| encode_stored_json(o, format!("turn {turn_id} origin")))
            .transpose()?;
        tx.execute(
            "INSERT INTO turns (turn_id, session_id, turn_number, status, started_at, completed_at, usage, origin)
             VALUES (?1, ?2, ?3, 'completed', ?4, ?4, ?5, ?6)
             ON CONFLICT(turn_id) DO UPDATE SET
                status = 'completed',
                completed_at = ?4,
                usage = coalesce(?5, usage)",
            params![turn_id, session_id, turn_number, now, usage_json, origin_json],
        )?;

        {
            let mut insert_message = tx.prepare(
                "INSERT INTO messages (session_id, turn_id, role, content, tool_calls, tool_call_id, blocks, prompt_id, tools, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for m in messages {
                let tool_calls_json = if m.tool_calls.is_empty() {
                    None
                } else {
                    Some(encode_stored_json(
                        &m.tool_calls,
                        format!("turn {turn_id} tool_calls"),
                    )?)
                };
                let blocks_json = if m.blocks.is_empty() {
                    None
                } else {
                    Some(encode_stored_json(
                        &m.blocks,
                        format!("turn {turn_id} blocks"),
                    )?)
                };
                let tools_json = if m.tools.is_empty() {
                    None
                } else {
                    Some(encode_stored_json(
                        &m.tools,
                        format!("turn {turn_id} tools"),
                    )?)
                };
                insert_message.execute(params![
                    session_id,
                    turn_id,
                    m.role.as_str(),
                    m.content,
                    tool_calls_json,
                    m.tool_call_id,
                    blocks_json,
                    m.prompt_id,
                    tools_json,
                    now
                ])?;
            }
        }

        Ok(())
    }

    pub fn load_session_history(
        &self,
        session_id: &str,
    ) -> Result<Vec<LLMMessage>, rusqlite::Error> {
        let journal = crate::session::wire_journal::resume_journal_records(self, session_id);
        let messages: Vec<LLMMessage> = self
            .load_session_messages(session_id)?
            .into_iter()
            .map(|stored| stored.message)
            .collect();
        if let Some(records) = journal
            && let Ok(history) = crate::session::wire_journal::fold_history(&records)
            && !history.is_empty()
        {
            if crate::session::wire_journal::has_undo_marker(&records)
                || messages.is_empty()
                || crate::session::wire_journal::real_user_contents(&messages)
                    == crate::session::wire_journal::real_user_contents(&history)
            {
                return Ok(history);
            }
            tracing::warn!(
                %session_id,
                "the v2 wire journal and the messages table disagree; using the table (legacy delete-style undo)"
            );
            return Ok(messages);
        }
        if !messages.is_empty() {
            return Ok(messages);
        }
        Ok(crate::session::wire_journal::history_from_store(
            self,
            session_id,
            crate::callbacks::MAIN_AGENT_ID,
        )
        .unwrap_or_default())
    }

    pub fn offload_payload(&self, payload: &mut Value) -> Result<bool, EventStoreError> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let changed =
            crate::native::event_store::blobs::offload_data_uris(payload, &tx, &self.blob_cache)?;
        tx.commit()?;
        Ok(changed)
    }

    pub fn rehydrate_payload(&self, payload: &mut Value) -> Result<(), EventStoreError> {
        let conn = self.conn.lock();
        crate::native::event_store::blobs::rehydrate_blob_refs(payload, &conn, &self.blob_cache)?;
        Ok(())
    }

    pub fn wire_journal_path(&self, session_id: &str, agent_id: &str) -> Option<PathBuf> {
        let home = self.path()?.parent()?;
        if home.as_os_str().is_empty() {
            return None;
        }
        let session = self.get_session(session_id).ok().flatten()?;
        let scope = session
            .workspace_id
            .or_else(|| session.cwd.as_deref().map(encode_workdir_key))?;
        Some(
            home.join("sessions")
                .join(scope)
                .join(session_id)
                .join("agents")
                .join(agent_id)
                .join(crate::session::wire_journal::WIRE_FILE_NAME),
        )
    }

    pub fn put_state(&self, domain: &str, key: &str, value: &Value) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        write_state_entry(&conn, &self.blob_cache, domain, key, value)
    }

    pub fn put_session_state(
        &self,
        domain: &str,
        session_id: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let tx = conn.unchecked_transaction()?;
        let value = dehydrate_state_value(value, &tx, &self.blob_cache)?;
        let val_str = encode_stored_json(value.as_ref(), format!("state entry {domain}/{key}"))?;
        tx.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?3,
                updated_at = ?4,
                session_id = ?5",
            params![domain, key, val_str, now, session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn session_state_entries(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String, Value)>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT domain, key, value FROM state_entries WHERE session_id = ?1
             ORDER BY domain, key",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let domain: String = row.get(0)?;
            let key: String = row.get(1)?;
            let raw: String = row.get(2)?;
            Ok((
                domain.clone(),
                key.clone(),
                decode_stored_json(&raw, format!("state entry {domain}/{key}"))?,
            ))
        })?;
        let mut entries = rows.collect::<Result<Vec<_>, rusqlite::Error>>()?;
        for (_, _, value) in &mut entries {
            rehydrate_state_value(value, &conn, &self.blob_cache)?;
        }
        Ok(entries)
    }

    pub fn get_state(&self, domain: &str, key: &str) -> Result<Option<Value>, rusqlite::Error> {
        let conn = self.conn.lock();
        read_state_entry(&conn, &self.blob_cache, domain, key)
    }

    pub fn patch_state(
        &self,
        domain: &str,
        key: &str,
        patch: &crate::session::patch::JsonPatchSet,
    ) -> Result<(Value, crate::session::patch::JsonPatchSet), String> {
        let conn = self.conn.lock();
        let mut val = read_state_entry(&conn, &self.blob_cache, domain, key)
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let inverse = crate::session::patch::apply_patch(&mut val, patch)
            .map_err(|e| format!("failed to apply patch to {domain}/{key}: {e}"))?;
        write_state_entry(&conn, &self.blob_cache, domain, key, &val).map_err(|e| e.to_string())?;
        Ok((val, inverse))
    }

    pub fn diff_state(
        &self,
        domain: &str,
        key: &str,
        new_value: &Value,
    ) -> Result<crate::session::patch::JsonPatchSet, String> {
        let current = self
            .get_state(domain, key)
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        Ok(crate::session::patch::diff_values(&current, new_value))
    }

    pub fn get_latest_checkpoint(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<Value>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(



            "SELECT data FROM checkpoints WHERE session_id = ?1 AND name = ?2 ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )?;
        let raw = {
            let mut rows = stmt.query(params![session_id, name])?;
            match rows.next()? {
                Some(row) => {
                    let data_str: String = row.get(0)?;
                    Some(decode_stored_json(
                        &data_str,
                        format!("checkpoint {session_id}/{name}"),
                    )?)
                }
                None => None,
            }
        };
        match raw {
            Some(mut value) => {
                rehydrate_state_value(&mut value, &conn, &self.blob_cache)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    pub fn save_checkpoint(
        &self,
        session_id: &str,
        checkpoint_id: &str,
        name: &str,
        data: &Value,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let data = dehydrate_state_value(data, &conn, &self.blob_cache)?;
        let data_str =
            encode_stored_json(data.as_ref(), format!("checkpoint {session_id}/{name}"))?;
        conn.execute(
            "INSERT INTO checkpoints (id, session_id, name, data, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                name = ?3,
                data = ?4,
                created_at = ?5",
            params![checkpoint_id, session_id, name, data_str, now],
        )?;
        Ok(())
    }

    pub fn gui_get_item(&self, key: &str) -> Result<Option<String>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT value FROM state_entries WHERE domain = 'gui_store' AND key = ?1")?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            let s: String = row.get(0)?;
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }

    pub fn gui_set_item(&self, key: &str, value: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at)
             VALUES ('gui_store', ?1, ?2, ?3)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?2,
                updated_at = ?3",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn gui_remove_item(&self, key: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "DELETE FROM state_entries WHERE domain = 'gui_store' AND key = ?1",
            params![key],
        )?;
        Ok(affected > 0)
    }

    pub fn gui_clear(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM state_entries WHERE domain = 'gui_store'", [])?;
        Ok(())
    }

    pub fn gui_length(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM state_entries WHERE domain = 'gui_store'",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn search_messages(
        &self,
        query: &str,
        session_id: Option<&str>,
        role: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchHit>, rusqlite::Error> {
        let conn = self.conn.lock();
        let query_trim = query.trim();
        if query_trim.is_empty() {
            return Ok(vec![]);
        }

        let like_pattern = format!("%{}%", query_trim);
        let mut sql = String::from(
            "SELECT m.session_id, COALESCE(s.title, ''), m.role, m.content, m.created_at, m.turn_id
             FROM messages m
             LEFT JOIN sessions s ON m.session_id = s.session_id
             WHERE (m.content LIKE ?1 OR s.title LIKE ?1)",
        );

        let mut params_vec: Vec<rusqlite::types::Value> = vec![like_pattern.into()];

        if let Some(sid) = session_id {
            params_vec.push(sid.to_string().into());
            sql.push_str(&format!(" AND m.session_id = ?{}", params_vec.len()));
        }

        if let Some(r) = role {
            params_vec.push(r.to_string().into());
            sql.push_str(&format!(" AND m.role = ?{}", params_vec.len()));
        }

        params_vec.push((limit as i64).into());
        params_vec.push((offset as i64).into());
        sql.push_str(&format!(
            " ORDER BY m.created_at DESC LIMIT ?{} OFFSET ?{}",
            params_vec.len() - 1,
            params_vec.len()
        ));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
            let session_id: String = row.get(0)?;
            let session_title: String = row.get(1)?;
            let role: String = row.get(2)?;
            let content: String = row.get(3)?;
            let created_at_ms: i64 = row.get(4)?;
            let turn_id: String = row.get(5)?;

            let snippet = extract_snippet(&content, query_trim, 120);

            Ok(SearchHit {
                session_id,
                workspace_id: String::new(),
                session_title,
                agent_id: "main".to_string(),
                role,
                snippet,
                time: format_iso(created_at_ms),
                turn: 1,
                step_id: turn_id,
                score: 1.0,
            })
        })?;

        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    pub fn count_messages(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let count: usize = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn append_wire_event(&self, event: &RawWireEvent) -> Result<u64, EventStoreError> {
        let payload_json = serde_json::to_string(&event.payload)?;
        self.append_wire_event_json(
            &event.id,
            &event.session_id,
            &event.event_type,
            &payload_json,
            event.is_checkpoint,
            event.is_compaction,
            event.created_at,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_wire_event_json(
        &self,
        id: &str,
        session_id: &str,
        event_type: &str,
        payload_json: &str,
        is_checkpoint: bool,
        is_compaction: bool,
        created_at: i64,
    ) -> Result<u64, EventStoreError> {
        if let Some(holder) = crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id) {
            return Err(EventStoreError::SessionOccupied(
                crate::session::lock::session_occupied_message(Some(&holder)),
            ));
        }
        let conn = self.conn.lock();

        let tx = conn.unchecked_transaction()?;
        let payload_json = if payload_json.contains("data:") {
            let mut payload: serde_json::Value = serde_json::from_str(payload_json)?;
            if crate::native::event_store::blobs::offload_data_uris(
                &mut payload,
                &tx,
                &self.blob_cache,
            )? {
                serde_json::to_string(&payload)?
            } else {
                payload_json.to_string()
            }
        } else {
            payload_json.to_string()
        };
        tx.execute(
            "INSERT INTO wire_events (id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at, protocol_version, min_protocol_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                session_id,
                event_type,
                payload_json,
                is_checkpoint,
                is_compaction,
                created_at,
                WIRE_PROTOCOL_VERSION,
                WIRE_MIN_READER_VERSION,
            ],
        )?;
        let seq = tx.last_insert_rowid() as u64;
        tx.commit()?;
        Ok(seq)
    }

    pub fn get_wire_events(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![session_id, since_seq, limit as i64],
            wire_record_from_row,
        )?;
        let (mut records, dropped) = collect_wire_rows(rows)?;
        if dropped > 0 {
            tracing::warn!(
                code = "wire.malformed_record",
                %session_id,
                dropped,
                "wire read skipped records whose payload did not parse"
            );
        }

        for record in &mut records {
            crate::native::event_store::blobs::rehydrate_blob_refs(
                &mut record.payload,
                &conn,
                &self.blob_cache,
            )?;
        }
        Ok(records)
    }

    pub fn wire_event_payloads(
        &self,
        session_id: &str,
        event_type: &str,
    ) -> Result<Vec<Value>, EventStoreError> {
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(
            "SELECT payload FROM wire_events
             WHERE session_id = ?1 AND event_type = ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id, event_type], |row| {
            row.get::<_, String>(0)
        })?;
        let mut payloads = Vec::new();
        for row in rows {
            let payload = row?;
            if let Ok(value) = serde_json::from_str::<Value>(&payload) {
                payloads.push(value);
            }
        }
        Ok(payloads)
    }

    pub fn search_wire_events(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC
             LIMIT ?3",
        )?;

        let rows = stmt.query_map(params![session_id, since_seq, limit as i64], |row| {
            let payload_str: String = row.get(4)?;
            let seq: u64 = row.get(0)?;
            let payload = decode_stored_json(&payload_str, format!("wire event seq {seq}"))?;
            Ok(WireEventRecord {
                seq,
                id: row.get(1)?,
                session_id: row.get(2)?,
                event_type: row.get(3)?,
                payload,
                is_checkpoint: row.get(5)?,
                is_compaction: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, rusqlite::Error>>()?)
    }

    pub fn interaction_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        const TYPES: [&str; 8] = [
            "event.approval.requested",
            "event.approval.resolved",
            "event.approval.expired",
            "event.question.requested",
            "event.question.answered",
            "event.question.dismissed",
            "interaction.request",
            "interaction.resolved",
        ];
        let placeholders = TYPES.map(|_| "?").join(", ");
        let sql = format!(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND event_type IN ({placeholders})
             ORDER BY seq ASC"
        );
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                session_id, TYPES[0], TYPES[1], TYPES[2], TYPES[3], TYPES[4], TYPES[5], TYPES[6],
                TYPES[7]
            ],
            wire_record_from_row,
        )?;
        collect_logged_wire_rows(rows, session_id, "interaction")
    }

    pub fn prompt_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        const TYPES: [&str; 4] = [
            "prompt.submitted",
            "prompt.completed",
            "prompt.aborted",
            "prompt.steered",
        ];
        let placeholders = TYPES.map(|_| "?").join(", ");
        let sql = format!(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND event_type IN ({placeholders})
             ORDER BY seq ASC"
        );
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![session_id, TYPES[0], TYPES[1], TYPES[2], TYPES[3]],
            wire_record_from_row,
        )?;
        collect_logged_wire_rows(rows, session_id, "prompt")
    }

    pub fn turn_prompt_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND event_type = 'turn.prompt'
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], wire_record_from_row)?;
        collect_logged_wire_rows(rows, session_id, "turn.prompt")
    }

    pub fn task_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        const TYPES: [&str; 11] = [
            "subagent.spawned",
            "subagent.completed",
            "subagent.failed",
            "task.started",
            "task.terminated",
            "task.output",
            "event.task.created",
            "event.task.completed",
            "event.task.progress",
            "turn.started",
            "turn.ended",
        ];
        let placeholders = TYPES.map(|_| "?").join(", ");
        let sql = format!(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND event_type IN ({placeholders})
             ORDER BY seq ASC"
        );
        let conn = self.conn.lock();
        refuse_newer_wire_version(&conn, session_id)?;
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::types::ToSql> = vec![&session_id];
        params.extend(TYPES.iter().map(|t| t as &dyn rusqlite::types::ToSql));
        let rows = stmt.query_map(params.as_slice(), wire_record_from_row)?;
        collect_logged_wire_rows(rows, session_id, "task")
    }

    pub fn count_wire_events(&self, session_id: &str) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
    }

    pub fn latest_wire_event_seq(&self, session_id: &str) -> Result<u64, rusqlite::Error> {
        let conn = self.conn.lock();
        let max_seq: Option<u64> = conn.query_row(
            "SELECT MAX(seq) FROM wire_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(max_seq.unwrap_or(0))
    }

    pub fn set_last_turn_reason(
        &self,
        session_id: &str,
        reason: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();

        conn.execute(
            "UPDATE sessions SET last_turn_reason = ?2
             WHERE session_id = ?1 AND last_turn_reason IS NOT ?2",
            params![session_id, reason],
        )?;
        Ok(())
    }

    pub fn last_turn_reason(&self, session_id: &str) -> Option<String> {
        let conn = self.conn.lock();

        conn.query_row(
            "SELECT last_turn_reason FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    pub fn fold_projection(&self, session_id: &str) -> Result<Vec<Message>, EventStoreError> {
        let backup = self.path().map(|path| wire_backup_path(path, session_id));

        let occupied = crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id);
        let raw_rows = {
            let conn = self.conn.lock();
            if occupied.is_some() {
                tracing::warn!(
                    code = "wire.repair_deferred",
                    %session_id,
                    "session is held by another process; skipping wire repair on this read"
                );
                crate::native::event_store::read_fold_rows(&conn, session_id, &self.blob_cache)?.0
            } else {
                read_fold_rows_with_repair(&conn, session_id, &self.blob_cache, backup.as_deref())?
                    .0
            }
        };

        fold_wire_events(raw_rows.iter().map(|(t, p, c)| (t.as_str(), p, *c)))
    }

    pub fn checkpoint_compress(
        &self,
        session_id: &str,
        summary: &str,
    ) -> Result<(), EventStoreError> {
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
        self.append_wire_event(&event)?;
        Ok(())
    }

    pub fn undo_to_last_checkpoint(&self, session_id: &str) -> Result<usize, EventStoreError> {
        if let Some(holder) = crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id) {
            return Err(EventStoreError::SessionOccupied(
                crate::session::lock::session_occupied_message(Some(&holder)),
            ));
        }
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

impl EventStore for SqliteSessionStore {
    fn append_event(&self, event: &RawWireEvent) -> Result<u64, EventStoreError> {
        self.append_wire_event(event)
    }

    fn fold_projection(&self, session_id: &str) -> Result<Vec<Message>, EventStoreError> {
        SqliteSessionStore::fold_projection(self, session_id)
    }

    fn checkpoint_compress(&self, session_id: &str, summary: &str) -> Result<(), EventStoreError> {
        SqliteSessionStore::checkpoint_compress(self, session_id, summary)
    }

    fn undo_to_last_checkpoint(&self, session_id: &str) -> Result<usize, EventStoreError> {
        SqliteSessionStore::undo_to_last_checkpoint(self, session_id)
    }
}

pub fn derive_session_title(
    history: &[LLMMessage],
    source: Option<&str>,
) -> Result<Option<String>, String> {
    const MAX_TITLE_LENGTH: usize = 200;
    match source.unwrap_or("user_prompts") {




        "digest" => {
            return Err(
                "source=digest is the managed chat_title call; use session::title::generate_session_title".into(),
            )
        }
        "first_turn" | "user_prompts" => {}



        _ => {}
    }
    let prompt = history
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.trim())
        .find(|c| !c.is_empty());
    let Some(prompt) = prompt else {
        return Ok(None);
    };

    Ok(crate::session::title::prompt_metadata_text(prompt)
        .map(|text| text.chars().take(MAX_TITLE_LENGTH).collect()))
}

#[cfg(test)]
mod tests;
