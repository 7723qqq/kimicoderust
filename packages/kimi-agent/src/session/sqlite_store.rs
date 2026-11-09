//! SQLite-backed session persistence store.
//!
//! Provides zero-dependency standalone persistence for sessions, turns,
//! messages, checkpoints, and arbitrary state domains using embedded SQLite (WAL mode).

use std::fmt;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::native::event_store::{
    EventStore, EventStoreError, Message, RawWireEvent, WIRE_MIN_READER_VERSION,
    WIRE_PROTOCOL_VERSION, fold_wire_events, read_fold_rows_with_repair, refuse_newer_wire_version,
    wire_backup_path,
};
use crate::rpc::types::TokenUsage;
use crate::turn_loop::types::LLMMessage;

/// A stored JSON column that cannot be decoded.
///
/// These columns used to be read with `unwrap_or(Value::Null)` / `.ok()`: a
/// truncated or hand-edited row became "no value", which the next write then
/// overwrote and an undo then deleted. Naming the row turns the corruption into
/// a reportable failure instead of data loss.
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

/// A value that cannot be encoded for storage. Writing an empty string instead
/// (the old `unwrap_or_default()`) produced a row that every reader then had to
/// treat as corrupt.
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

/// Encode one JSON column, failing with the column's name instead of storing an
/// empty string when serialization fails.
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

/// Dehydrate a state document before it is encoded (v2 `StateBlobCodec`):
/// large inline media becomes a `blobref:` URL in the same `wire_blobs`
/// table the wire uses, so a state entry does not carry the same bytes twice.
fn dehydrate_state_value<'a>(
    value: &'a Value,
    conn: &rusqlite::Connection,
) -> rusqlite::Result<std::borrow::Cow<'a, Value>> {
    // Media-free documents — the overwhelming majority — are encoded straight
    // from the caller's borrow.
    if !crate::native::event_store::blobs::has_inline_data_uri(value) {
        return Ok(std::borrow::Cow::Borrowed(value));
    }
    let mut owned = value.clone();
    crate::native::event_store::blobs::offload_data_uris(&mut owned, conn)?;
    Ok(std::borrow::Cow::Owned(owned))
}

/// Expand the `blobref:` URLs a state document was written with.
fn rehydrate_state_value(value: &mut Value, conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    crate::native::event_store::blobs::rehydrate_blob_refs(value, conn)?;
    Ok(())
}

/// Read one state entry, decoding and rehydrating it.
///
/// A row that exists but does not parse is an error, not `null`: reporting
/// `null` let the next write overwrite the only copy.
fn read_state_entry(conn: &Connection, domain: &str, key: &str) -> rusqlite::Result<Option<Value>> {
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
            rehydrate_state_value(&mut value, conn)?;
            Ok(Some(value))
        }
        None => Ok(None),
    }
}

/// Write one state entry, offloading inline media and committing the blob
/// bytes with the row.
fn write_state_entry(
    conn: &Connection,
    domain: &str,
    key: &str,
    value: &Value,
) -> rusqlite::Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    // The offloaded blob bytes and the state row commit together, so a failed
    // write cannot leave media in `wire_blobs` that no row references.
    let tx = conn.unchecked_transaction()?;
    let value = dehydrate_state_value(value, &tx)?;
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

/// Decode one nullable JSON column into its typed value.
///
/// `None` is a genuinely absent column. An empty string is what older builds
/// wrote when serialization failed (`unwrap_or_default`), so it keeps reading
/// as absent; anything else that fails to parse is reported with the column's
/// name instead of being flattened into an empty list or `None`.
fn decode_json_column<T: serde::de::DeserializeOwned>(
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

/// Decode one JSON column, failing with the column's name instead of yielding
/// `Value::Null`.
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

/// The columns one table currently has, for the additive schema upgrades the
/// store runs on open.
fn table_columns(
    conn: &Connection,
    table: &str,
) -> rusqlite::Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    Ok(names.filter_map(std::result::Result::ok).collect())
}

/// Add one column a database created by an older build may not have.
///
/// Only `duplicate column name` is benign. Every other failure is a migration
/// that did not run, and the caller (`init`) must abort the open instead of
/// continuing with a schema the rest of the store assumes is current.
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

/// Format a millisecond timestamp as an ISO-8601 string.
fn format_iso(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Render one `wire_events` row as a v2 journal line: the record type plus its
/// payload fields, with dehydrated media restored so the exported bundle is
/// readable without the blob table.
fn wire_export_line(
    event_type: &str,
    payload: &str,
    conn: &Connection,
) -> rusqlite::Result<String> {
    // A corrupt payload must not be exported as `"payload": null`: the bundle
    // would look complete while missing the record's content.
    let mut value: Value = serde_json::from_str(payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    crate::native::event_store::blobs::rehydrate_blob_refs(&mut value, conn)?;
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

/// Compute standard deterministic workspace ID `wd_<slug>_<hash12>`.
pub fn encode_workdir_key(work_dir: &str) -> String {
    let normalized = work_dir.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    let base = normalized.split('/').next_back().unwrap_or(normalized);

    let mut slug = String::new();
    for c in base.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            slug.push(c);
        } else {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.len() > 40 {
        slug[..40].trim_matches('-').to_string()
    } else {
        slug
    };
    let final_slug = if slug.is_empty() || slug == "." || slug == ".." {
        "workspace".to_string()
    } else {
        slug
    };

    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    let hash12 = &hash[..12];

    format!("wd_{final_slug}_{hash12}")
}

/// The identity of a workspace root, independent of how it was spelled.
///
/// Two spellings of the same directory — a symlink and its target, a different
/// path case, a relative and an absolute form — must resolve to one workspace.
///
/// This is v2 `workdir-slug.ts` `workspaceRootKey`, verbatim in behaviour: it is
/// **lexical only**. No filesystem call, so no symlink resolution and no on-disk
/// case lookup. An earlier version of this function canonicalized instead, which
/// reached the filesystem and therefore answered differently for a symlinked root
/// and for a path whose stored case differs from the disk — two answers v2 never
/// had. v2's own tests pin the lexical contract:
///
/// ```text
/// workspaceRootKey('C:\\Users\\Foo\\Proj') === 'c:/users/foo/proj'
/// workspaceRootKey('c:/Users/Foo/Proj/')  === 'c:/users/foo/proj'
/// ```
///
/// The case fold is keyed on the **path's shape**, not on the host OS: only a
/// Windows-shaped root (`C:/…`, `//…`) is lower-cased. A POSIX path keeps its
/// case wherever this runs, because on a case-sensitive filesystem two spellings
/// really are two directories.
pub fn workspace_root_key(root: &str) -> String {
    let slashed = root.replace('\\', "/");
    let normalized = slashed.trim_end_matches('/');
    if is_windows_shaped(&slashed) {
        normalized.to_lowercase()
    } else {
        normalized.to_string()
    }
}

/// v2 `WIN_SHAPED`: a drive-letter root, a UNC root, or a double-slash root.
///
/// Tested against the slash-converted form, as v2 does, so `C:\\x` and `C:/x`
/// agree.
fn is_windows_shaped(slashed: &str) -> bool {
    let bytes = slashed.as_bytes();
    let drive =
        bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/';
    drive || slashed.starts_with("//")
}

/// Upper bound on the blobs one [`SqliteSessionStore::blob_usage`] audit probes.
pub const BLOB_AUDIT_MAX_ROWS: u64 = 10_000;

/// Result of [`SqliteSessionStore::blob_usage`]: a read-only view of the
/// content-addressed blob store, including the bytes no row references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobUsage {
    /// Rows in `wire_blobs`.
    pub stored: u64,
    /// Total stored payload bytes.
    pub bytes: u64,
    /// Of the scanned blobs, the ones no wire payload or state value mentions.
    pub unreferenced: u64,
    /// Blobs actually probed (capped at [`BLOB_AUDIT_MAX_ROWS`]).
    pub scanned: u64,
    /// Whether `stored` exceeds `scanned`.
    pub truncated: bool,
}

/// Decode one row selected in `WIRE_EVENT_COLUMNS` order.
///
/// A payload that is not valid JSON yields `None` instead of failing the read:
/// v2's reader reports the line through `onTruncate` and keeps collecting the
/// records that follow, and one bad row must not make a session's whole
/// transcript unreadable. The drop is logged with the row's `seq`, and the
/// fold path repairs the row away.
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

/// Collect the rows a wire read decoded, dropping the ones whose payload did
/// not parse and reporting how many were dropped.
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

/// Collect every row of an unpaged wire read.
///
/// No row cap: v2's reader assigns `this.lines = records.length` over the whole
/// collected journal (`wire/wireService.ts:475`, `:526`) without a limit.
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

/// v2 `SESSION_META_VERSION`: the schema version of the session metadata
/// document. Rows written by earlier builds are normalized on read.
pub const SESSION_META_VERSION: u32 = 2;

/// v2 `DEFAULT_SESSION_TITLE` (v2 `sessionMetadata/promptMetadata.ts:7`).
pub const DEFAULT_SESSION_TITLE: &str = "New Session";

/// v2 `isUntitled`: absent, blank, or the default title means "no title yet",
/// so an automatically derived title may replace it.
pub fn is_untitled(title: Option<&str>) -> bool {
    match title {
        None => true,
        Some(title) => title.trim().is_empty() || title == DEFAULT_SESSION_TITLE,
    }
}

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
    /// v2 `titleKind`: `replaceable` / `generated` / `custom`. `None` on rows
    /// written before the column existed; the read path normalizes those to
    /// `replaceable` (see [`SessionSummary::normalized_title_kind`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    /// v2 `forkedFrom`: the session this one was forked from. Distinct from
    /// `parent_session_id`, which records child-session parentage.
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
    /// v2 `agents`: the session's agent roster, keyed by agent id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Value>,
    /// v2 `custom`: free-form host metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<String>,
}

impl SessionSummary {
    /// v2 `normalizeSessionTitle`: an absent `titleKind` on a row with a title
    /// reads as `replaceable`; a row with no title has no kind.
    pub fn normalized_title_kind(&self) -> Option<&'static str> {
        match self.title_kind.as_deref() {
            Some("custom") => Some("custom"),
            Some("generated") => Some("generated"),
            Some("replaceable") => Some("replaceable"),
            _ => {
                if self.title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
                    Some("replaceable")
                } else {
                    None
                }
            }
        }
    }

    /// v2 `isCustomTitle` compatibility encoding for the wire.
    pub fn is_custom_title(&self) -> bool {
        self.normalized_title_kind() == Some("custom")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExport {
    pub session: SessionSummary,
    pub messages: Vec<LLMMessage>,
    pub turns_count: usize,
    pub exported_at: String,
    /// v2 `sessionFirstActivity` / `sessionLastActivity` (the wire scan's
    /// min/max record time), falling back to the session row's own times when
    /// the session has no wire rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_activity_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_at: Option<String>,
    /// The raw journal view v2 bundles as `agents/<id>/wire.jsonl`, rendered
    /// from `wire_events` with dehydrated media restored. Kept out of
    /// `session.json` so the document does not duplicate the bundle member.
    #[serde(default, skip_serializing)]
    pub wire_jsonl: Vec<String>,
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

/// The per-side content cap for `session_file_history`, matching the v2
/// `FILE_HISTORY_MAX_FILE_BYTES` (v2 `features/fileHistory/
/// fileHistoryService.ts`). A side over the cap is recorded as a metadata-only
/// change with `oversize: true` instead of as text: the retention window is a
/// row count (`LIMIT 500`), so without a byte cap one 20 MB `Write` leaves two
/// 20 MB blobs per row in the table and in the WAL.
pub const FILE_HISTORY_MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;

/// How many recent turns keep their file snapshots, matching v2
/// `CHECKPOINT_MAX_TURNS` (`agent/checkpoint/checkpointService.ts:20`). Undo
/// can restore files only within this window; older turns are pruned whenever
/// a new snapshot is recorded.
const CHECKPOINT_MAX_TURNS: i64 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHistoryContent {
    pub version: usize,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<bool>,
    /// `Some(true)` when the content was not stored because it was over
    /// [`FILE_HISTORY_MAX_CONTENT_BYTES`], so `content: None` here means
    /// "too large to keep", not "the file was empty". Absent (not `false`)
    /// for every row that does store its content, so the response shape is
    /// unchanged for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oversize: Option<bool>,
}

/// Helper to compute status, additions, and deletions for file content changes.
pub fn compute_line_diff(before: Option<&str>, after: Option<&str>) -> (String, usize, usize) {
    match (before, after) {
        (None, Some(a)) => {
            let lines = a.lines().count();
            ("added".to_string(), lines, 0)
        }
        (Some(b), None) => {
            let lines = b.lines().count();
            ("deleted".to_string(), 0, lines)
        }
        (Some(b), Some(a)) => {
            if b == a {
                ("modified".to_string(), 0, 0)
            } else {
                let b_lines: Vec<&str> = b.lines().collect();
                let a_lines: Vec<&str> = a.lines().collect();
                let b_set: std::collections::HashSet<&str> = b_lines.iter().copied().collect();
                let a_set: std::collections::HashSet<&str> = a_lines.iter().copied().collect();
                let mut additions = 0;
                let mut deletions = 0;
                for l in &a_lines {
                    if !b_set.contains(l) {
                        additions += 1;
                    }
                }
                for l in &b_lines {
                    if !a_set.contains(l) {
                        deletions += 1;
                    }
                }
                if additions == 0 && deletions == 0 {
                    additions = a_lines.len().abs_diff(b_lines.len());
                }
                ("modified".to_string(), additions, deletions)
            }
        }
        (None, None) => ("modified".to_string(), 0, 0),
    }
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

/// Apply the WAL to the main database file and truncate it, so a prune gives
/// the log back instead of leaving it to grow until SQLite checkpoints it on
/// its own schedule.
///
/// Two things are deliberately *not* done here, because both are policy rather
/// than mechanics:
///   * `PRAGMA incremental_vacuum` — the stores are not opened with
///     `auto_vacuum = INCREMENTAL`, so it is a no-op today (measured: one page).
///   * a full `VACUUM` — that is what actually shrinks the main file
///     (measured on a 3.3 MB file whose file history was pruned from 8 rows to
///     1: 806 pages -> 120 pages, 3.3 MB -> 0.49 MB), but it rewrites the whole
///     database, needs room for a second copy, and belongs to whoever owns the
///     session-database lifecycle. The pages a prune frees are on the freelist
///     and get reused, so the file stops growing either way.
///
/// Best-effort by design: a checkpoint that cannot complete because another
/// reader holds the WAL leaves the space to the next prune, and it must not
/// turn a successful prune into an error.
fn reclaim_file_history_space(conn: &Connection) -> Result<(), rusqlite::Error> {
    // `wal_checkpoint` returns `(busy, log_frames, checkpointed_frames)`.
    // TRUNCATE resets the log to zero bytes, which is the point: after a
    // multi-megabyte prune the deleted pages are otherwise still occupying
    // the log, and the file keeps growing on every subsequent write until
    // SQLite happens to checkpoint on its own schedule.
    //
    // A busy result is not an error — another reader holding the log is normal
    // and the space is reclaimed by the next prune. It must not be silent,
    // though: answering `busy = 0`-style success while the log still holds the
    // pruned content is exactly what leaves the disk dirty while the caller
    // believes it was reclaimed. A hard failure is warned for the same reason
    // and still does not undo a prune that already succeeded.
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        row.get::<_, i64>(0)
    }) {
        Ok(0) => {}
        Ok(busy) => tracing::warn!(
            busy,
            "file history rows were pruned but another reader held the WAL; the freed space stays on disk until the next checkpoint"
        ),
        Err(error) => tracing::warn!(
            %error,
            "file history rows were pruned but the WAL could not be checkpointed; the freed space may remain on disk until SQLite checkpoints it"
        ),
    }
    Ok(())
}

/// The synthetic turn id the compaction rewrite saves its summary under.
/// Row key of the synthetic turn that carries a compaction summary. Callers
/// outside this module need it to tell a compaction turn from a real one when
/// they project history: the v3 `turn` entity has an origin for exactly that.
pub const COMPACT_TURN_ID: &str = "turn-compact";

/// Why a fork could not be sliced at the requested turn (v2
/// `assertForkTurnIndex` / `sliceMainRecordsAtTurn`'s `REQUEST_INVALID`).
/// These are caller mistakes, not store failures, so they travel separately
/// from [`rusqlite::Error`] and surface as a 4xx rather than a 500.
#[derive(Debug)]
pub enum ForkError {
    /// `turnIndex` was present but not a non-negative safe integer
    /// (v2 `assertForkTurnIndex`).
    InvalidTurnIndex,
    /// The session has fewer visible turns than the index names (v2
    /// `sliceMainRecordsAtTurn`'s "Turn N was not found" arm).
    TurnNotFound {
        turn_index: u64,
        available_turns: usize,
    },
    /// The store failed while copying.
    Store(rusqlite::Error),
}

impl From<rusqlite::Error> for ForkError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e)
    }
}

/// Why a session could not be removed or archived.
///
/// These are destructive writes, so they follow the same rule as the wire write
/// guard: a session held by another live process is refused instead of being
/// deleted underneath it.
#[derive(Debug)]
pub enum SessionRemovalError {
    /// Another process holds the session; the text names the holder.
    Occupied { message: String },
    /// The store failed.
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

/// The row key a copied turn gets in the forked session.
///
/// It must be **fresh**: `turns.turn_id` is the table's PRIMARY KEY — globally,
/// not per session — and `save_turn` upserts on it, so reusing the source's key
/// would silently update the *source's* row instead of inserting the fork's,
/// and the copy would hold no turns at all. The turn's position is carried
/// separately in `turn_number`, which is what callers and the wire project read,
/// so the opaque key is free to be fork-local.
///
/// The compaction turn is the one key that is *recognised* rather than opaque:
/// `select_undo_turns` refuses an undo crossing it by name. Copying the
/// sentinel verbatim would re-collide on the primary key (see above) and
/// overwrite the source's summary row, so the fork gets a fork-local key that
/// [`turn_is_compaction`] still recognises — otherwise the fork would silently
/// lose the undo guard and an undo could delete a turn out from under the
/// summary explaining it.
fn forked_turn_id(source_turn_id: &str, new_session_id: &str, number: u32) -> String {
    if turn_is_compaction(source_turn_id) {
        return format!("{new_session_id}-{COMPACT_TURN_ID}");
    }
    format!("{new_session_id}-turn-{number}")
}

/// Whether a `turn_id` names the compaction turn — in a live session or in a
/// fork of one, whose copy carries the fork id as a prefix (see
/// [`forked_turn_id`]).
fn turn_is_compaction(turn_id: &str) -> bool {
    turn_id == COMPACT_TURN_ID || turn_id.ends_with(&format!("-{COMPACT_TURN_ID}"))
}

/// Whether a persisted turn counts as a user-visible turn, i.e. one a fork can
/// name as a cut point.
///
/// This is v2 `isUserVisibleTurnRecord` (forkTurnSlice.ts:86-103) applied to
/// what this store actually persists. v2 reads the classification off the
/// `context.append_message` record's `origin.kind`; the fork's equivalent
/// record is a `turns` row plus its `origin` JSON, written by `save_turn`
/// from the request's prompt origin.
///
/// **Both of v2's guards, in order.** v2 checks the message `role` *before* it
/// looks at `origin`, and so does this: the origin vocabulary is permissive by
/// design (`undefined` and `user` are visible), so a turn that opened with a
/// non-user first message is only excluded if the role is actually consulted.
/// [`TurnRecord`] carries no role of its own — it is a row header — so the
/// caller passes the turn's opening message role in, read from the `messages`
/// table the same way v2 reads it off the record.
///
/// The vocabulary is v2's and is deliberately not extended: `undefined` and
/// `user` are visible; `skill_activation` / `plugin_command` only when the
/// client says the trigger was a typed slash (`user-slash`); `shell_command`
/// only in its `input` phase; every other kind is invisible. A turn with no
/// origin at all is visible — that is the `undefined` arm, and it is the
/// common case for a session recorded before origins were persisted.
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

/// v2 `isUndoAnchorOrigin` applied to a persisted turn: a real user prompt
/// (origin absent or `user`), a user-typed skill/plugin activation, and never
/// an in-turn steer or a derived turn (goal/cron/task/shell).
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

/// Whether one wire row starts a turn for a sliced fork.
///
/// `turn.prompt` is this ledger's user-prompt record and `message.user` is the
/// native loop's user message; both mean "a new user turn starts here". A
/// `context.append_message` row is read the way v2 reads it, with the role and
/// origin on the message itself.
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
    /// One connection, guarded for the whole store — so this lock is the
    /// highest-blast-radius mutex in the engine.
    ///
    /// `parking_lot` rather than `std::sync` because a std mutex **poisons**:
    /// a panic anywhere inside a guard scope makes every later
    /// `lock().unwrap()` panic too, and that cascade is permanent for the
    /// process. Here that would mean the entire session store — every
    /// session read and write — dead after one bad panic, with no recovery and
    /// nothing in the log. `parking_lot` has no poisoning, so the failure stays
    /// confined to the call that caused it.
    ///
    /// **This is insurance, not a fix for a live defect.** Measured 2026-09-28:
    /// 32 methods take this guard and none of their guard scopes contains a
    /// reachable panic source (the bodies are rusqlite calls returning
    /// `Result`; the only textual hits were constant array indices in format
    /// macros). The value is that the next refactor cannot introduce a cascade
    /// by accident.
    conn: Mutex<Connection>,
    /// The file this store was opened from; `None` for an in-memory store.
    /// A caller that needs the registry later reopens the file instead of
    /// holding this handle (see `PluginManager`): a retained connection keeps a
    /// Windows file lock on `<data dir>/sessions.db`, and the whole data
    /// directory then cannot be removed.
    path: Option<PathBuf>,
}

/// What one compaction did, mirroring the v2 `full_compaction.completed`
/// payload (`fullCompactionService.ts`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CompactionReport {
    /// Messages in the history before compaction.
    pub messages_before: usize,
    pub removed: usize,
    /// Messages the fold consumed — the index of the first kept message on
    /// this axis (the store history carries the system prompt at `[0]`). This
    /// is the `compactedCount` the host's `compaction.completed` event carries:
    /// a replay slices the pre-compaction history here.
    pub compacted_count: u32,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub kept_user_message_count: usize,
}

/// One persisted message and the time the store appended it.
///
/// `load_session_history` reads the same rows but drops the timestamp, because
/// the engine's `LLMMessage` has nowhere to carry it; a projection that has to
/// date its entities needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub message: LLMMessage,
    pub created_at: i64,
    /// The turn row this message belongs to — the store's own `turn-<random>`
    /// key, which is exactly the id the live ACP / WS stream namespaces tool
    /// calls with, so a replay can rebuild matching `toolCallId`s.
    pub turn_id: String,
}

/// Execution metadata of one persisted turn, without its messages.
///
/// `turn_id` is this store's own opaque row key — `server/engine.rs` generates
/// it as `turn-{random}`, so it is not an identity anything else can match.
/// `turn_number` is: it is the number the live stream reports as `turnId`.
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub turn_id: String,
    pub turn_number: u32,
    pub status: String,
    pub started_at: i64,
    pub completed_at: Option<i64>,
    pub usage: Option<crate::rpc::types::TokenUsage>,
    /// The prompt origin the turn was opened with (`v2 PromptOrigin`): `None`
    /// for turns persisted before the column existed. The v3 projector maps
    /// it to the turn entity's origin instead of assuming `user`.
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

/// Why a turn's workspace files could not be reverted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileRevertError {
    /// The file no longer matches what the tool wrote, so restoring the
    /// pre-image would destroy a change made after it (v2 `manual_edit` /
    /// `deleted`). The undo is refused rather than applied over the user's
    /// edit.
    Conflict { path: String, reason: String },
    /// The filesystem refused the operation.
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

/// Decode one row selected with
/// [`SqliteSessionStore::SESSION_SUMMARY_COLUMNS`] into a [`SessionSummary`].
fn session_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    let session_id: String = row.get(0)?;
    let mut summary = SessionSummary {
        session_id: session_id.clone(),
        title: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        workspace_id: row.get(4)?,
        archived: row.get::<_, i64>(5)? != 0,
        parent_session_id: row.get(6)?,
        title_kind: row.get(7)?,
        last_prompt: row.get(8)?,
        archived_at: row.get(9)?,
        forked_from: row.get(10)?,
        version: row.get::<_, Option<i64>>(11)?.map(|v| v.max(0) as u32),
        cwd: row.get(12)?,
        environment_id: row.get(13)?,
        environment_cwd: row.get(14)?,
        agents: decode_json_column(row.get(15)?, format!("session {session_id} agents"))?,
        custom: decode_json_column(row.get(16)?, format!("session {session_id} custom"))?,
        last_turn_reason: row.get(17)?,
    };
    // Normalize on read too: a row written by an older build before the
    // migration ran still answers the derived kind, so every consumer sees the
    // same value without a write.
    if summary.title_kind.is_none() {
        summary.title_kind = summary.normalized_title_kind().map(str::to_string);
    }
    Ok(summary)
}

impl SqliteSessionStore {
    /// Open or create a SQLite database file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let path = path.as_ref();
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    /// The database file this store was opened from, `None` in memory.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Create an in-memory SQLite database.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self, rusqlite::Error> {
        // Bounded wait when another connection holds the write lock. Without
        // it SQLite fails `SQLITE_BUSY` immediately, so two stores open on the
        // same file turn a normal write race into a lost append.
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

        // Column upgrades for databases created by older builds. Each table's
        // upgrades read that table's own column list: the wire version columns
        // used to be gated on `messages.tool_calls`, so a database that already
        // had `tool_calls` never got the wire columns and every later append
        // failed on a column that was never added.
        //
        // `ensure_column` tolerates only "duplicate column"; every other ALTER
        // failure (read-only database, disk error) aborts the open. A migration
        // that silently did not run leaves a half-upgraded database that fails
        // much later, where the cause is no longer visible.
        let messages = table_columns(&conn, "messages")?;
        for column in ["tool_calls", "tool_call_id", "blocks", "prompt_id"] {
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
        // The v2 `SessionMeta` fields that the structured columns did not carry
        // before (sessionMetadata.ts:17-34). Each is nullable; older rows read
        // as absent and are normalized on read.
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

        // One-time normalization of rows written before the metadata columns
        // existed (v2 `normalizeSessionMeta`, sessionMetadataService.ts:285-312):
        // a non-empty title with no recorded kind is `replaceable`, and the
        // document version is stamped so a later write keeps it current.
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

        // The `oversize` marker for a file-history row whose content was too
        // large to store (see `FILE_HISTORY_MAX_CONTENT_BYTES`). Rows written
        // before the column existed keep 0.
        let file_history = table_columns(&conn, "session_file_history")?;
        ensure_column(
            &conn,
            "session_file_history",
            &file_history,
            "oversize",
            "INTEGER NOT NULL DEFAULT 0",
        )?;

        // Session-scoped state (ROADMAP §6.1 item 4's P2 decision): the
        // column names the owning session so a delete cascades to the
        // session's entries. Rows written before it existed keep NULL and
        // are cascaded by their key pattern instead (see `delete_session`).
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
            path,
        })
    }

    /// The column list every session summary read uses, in
    /// [`session_summary_from_row`]'s order.
    const SESSION_SUMMARY_COLUMNS: &'static str = "session_id, title, created_at, updated_at, workspace_id, archived, parent_session_id, title_kind, last_prompt, archived_at, forked_from, version, cwd, environment_id, environment_cwd, agents, custom, last_turn_reason";

    /// Create or update a session header.
    pub fn create_session(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        self.create_session_with_workspace(session_id, title, None)
    }

    /// Create or update a session header with an optional workspace association.
    pub fn create_session_with_workspace(
        &self,
        session_id: &str,
        title: Option<&str>,
        workspace_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        Self::insert_session_row(&conn, session_id, title, workspace_id)
    }

    /// Insert (or revive) one session row on the given connection.
    ///
    /// Taking a `&Connection` rather than locking the store is what lets the
    /// fork create the session and copy its rows in one transaction: a copy
    /// that fails must roll the session row back with it.
    fn insert_session_row(
        conn: &Connection,
        session_id: &str,
        title: Option<&str>,
        workspace_id: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let now = chrono::Utc::now().timestamp_millis();
        // v2 `load()` seeds `agents` / `custom` with empty objects and stamps
        // the document version, so a freshly created session has the same shape
        // as a persisted one.
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

    /// Point an existing session at a workspace (upstream #4224: the
    /// `default_workspace` arm registers the resolved workspace after the
    /// session row exists, because the root is resolved + mkdir'd first).
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

    /// List all persisted sessions ordered by last updated time.
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

    /// Get summary for a specific session by ID.
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

    /// Export an entire session's metadata and conversation history as a snapshot.
    pub fn export_session(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExport>, rusqlite::Error> {
        let session = match self.get_session(session_id)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let messages = self.load_session_history(session_id)?;
        // Aggregates are reported, not defaulted: an export that silently says
        // "0 turns / no activity" because the query failed is a wrong artifact
        // that looks complete.
        let turns_count: usize = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?
        };
        // v2 `scanSessionWire` reports the first and last activity as the
        // min/max record timestamp. The wire is a table here, so the scan is
        // one aggregate; a session with no wire rows falls back to its row.
        let (first_activity, last_activity): (Option<i64>, Option<i64>) = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT MIN(created_at), MAX(created_at) FROM wire_events WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
        };
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // The raw journal member (v2 `wire.jsonl`): one line per record, in
        // sequence order, with dehydrated media restored so the bundle stands
        // on its own without the blob table.
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
                .map(|(event_type, payload)| wire_export_line(&event_type, &payload, &conn))
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(Some(SessionExport {
            first_activity_at: first_activity.or(Some(session.created_at)).map(format_iso),
            last_activity_at: last_activity.or(Some(session.updated_at)).map(format_iso),
            session,
            messages,
            turns_count,
            exported_at: now_iso,
            wire_jsonl,
        }))
    }

    /// The existing workspace id for `root`, or the freshly derived one.
    ///
    /// v2 `resolveAliasIds`: an alias is another id that points at the same
    /// root. The place duplicates are *born* is creation, so the lookup happens
    /// here — before a second row is written — rather than as a repair pass
    /// afterwards. Existing rows keep the id they were stored with, so no
    /// session's `workspace_id` is invalidated by this.
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
                // The guard is dropped when this function returns, so the
                // caller's own `lock()` does not deadlock against it.
                return Ok(id);
            }
        }
        Ok(encode_workdir_key(root))
    }

    /// Create or update a workspace.
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

    /// List all registered workspaces ordered by last opened time.
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

    /// Retrieve a single workspace summary by ID.
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

    /// Update a workspace's display name.
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

    /// Delete a workspace entry.
    pub fn delete_workspace(&self, workspace_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "DELETE FROM workspaces WHERE workspace_id = ?1",
            params![workspace_id],
        )?;
        Ok(affected > 0)
    }

    /// Query whether a workspace is trusted. Fail-closed: a workspace without an
    /// explicit trust record is untrusted, matching the v2 workspace trust
    /// default and the NAPI path.
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

    /// Set trust state for a workspace.
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

    /// Read-only blob audit: how much the content-addressed store holds and how
    /// much of it no row references.
    ///
    /// v2 has no blob garbage collector (its blob store is put/get/delete with
    /// no refcounts), so unreferenced blobs are not reclaimed here either. This
    /// is the observability that decides whether a future GC is worth building:
    /// an unreferenced count that keeps growing across deletes/undos is the
    /// signal. One `LIKE` per blob, capped at [`BLOB_AUDIT_MAX_ROWS`]; the scan
    /// reports whether it was cut short.
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

    /// Refuse a destructive session operation while another process holds the
    /// session (v2's session lock; see `session::lock`).
    fn ensure_not_occupied(session_id: &str) -> Result<(), SessionRemovalError> {
        match crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id) {
            Some(holder) => Err(SessionRemovalError::Occupied {
                message: crate::session::lock::session_occupied_message(Some(&holder)),
            }),
            None => Ok(()),
        }
    }

    /// Delete a session and all its cascading turns, messages, and checkpoints.
    pub fn delete_session(&self, session_id: &str) -> Result<bool, SessionRemovalError> {
        Self::ensure_not_occupied(session_id)?;
        let conn = self.conn.lock();
        // The state entries the session owns: the column-named ones, plus the
        // legacy key-encoded domains whose key *is* the session id
        // (`agent_config` / `metadata`, written before the column existed).
        // The turns / messages / wire_events / file-history rows go with the
        // session through their foreign keys.
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
        // File-history rows carry the file contents themselves, so deleting the
        // session only moves them to SQLite's freelist: the bytes keep
        // occupying the database and the WAL. v2 deletes the session's blob
        // directory outright (`sessionLifecycleService.ts:580`); reclaim the
        // same space here, and report it when that cannot happen — a delete
        // that answers `deleted: true` while the content survives is the
        // silent-failure mode this guards against.
        if affected > 0 && file_history_rows > 0 {
            reclaim_file_history_space(&conn)?;
        }
        Ok(affected > 0)
    }

    /// Fork an existing session into a new session with copied history.
    ///
    /// `turn_index` names a user-visible turn to cut at (v2 `forkSession`'s
    /// `opts.turnIndex`). `None` copies the whole session, which is what the
    /// parameter's absence means upstream too
    /// (`sessionLifecycleService.ts:569-576`). When it is `Some`, the copy
    /// keeps turns `0..=turn_index` and drops the rest — v2's `sliceMainRecordsAtTurn`
    /// takes `records.slice(0, turnStarts[turnIndex + 1])`, i.e. the named turn
    /// *and everything before it*.
    ///
    /// The retained turns keep their own `turn_id` / `turn_number` / `origin`
    /// rather than collapsing into one synthetic row. That is the defect this
    /// replaces: the old path called `save_turn(new, "turn-fork", 1, &history)`,
    /// so a four-message two-turn session forked into a single turn and
    /// `list_turns` reported one row. Turn identity is what makes a later
    /// `/undo`, a turn-scoped file-history read, and a re-fork of the fork work.
    pub fn fork_session(
        &self,
        source_session_id: &str,
        new_session_id: &str,
        title: Option<&str>,
        turn_index: Option<u64>,
    ) -> Result<bool, ForkError> {
        self.fork_session_with_metadata(source_session_id, new_session_id, title, turn_index, None)
    }

    /// Fork with additional host metadata merged into the copy's `custom`
    /// document (v2 `forkSession({ metadata })`).
    ///
    /// The fork copies the v2 `fullCopy` metadata — `title`, `titleKind`,
    /// `agents`, `custom`, `cwd`, `environmentId`, `environmentCwd` — records
    /// `forkedFrom`, and derives `lastPrompt` from the selected historical
    /// turn. Nothing is inherited by accident: every field is an explicit copy
    /// from the source row.
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
        let turns = self.list_turns(source_session_id)?;
        // Loaded before the visibility pass because v2's first guard is the
        // message `role`, which lives in the `messages` table, not on the turn
        // row. A turn with no messages at all is not nameable: v2 classifies a
        // `context.append_message` record, and a turn that never appended one
        // has nothing to classify.
        let messages = self.load_session_messages(source_session_id)?;
        let opening_role = |turn_id: &str| -> bool {
            messages
                .iter()
                .find(|m| m.turn_id == turn_id)
                .is_some_and(|m| m.message.role == "user")
        };

        // Which turns are nameable cut points, and where the copy stops.
        // `turn_index` counts *visible* turns, not rows, so an invisible turn
        // sitting between two visible ones must not shift the numbering.
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

        // Validation is already done, so the writes below cannot be interrupted
        // by a rejected `turnIndex`.
        let effective_title = title.or(source.title.as_deref());
        // v2 derives `lastPrompt` from the selected historical turn: the newest
        // user message inside the retained slice.
        let last_prompt = retained
            .iter()
            .rev()
            .flat_map(|(_, msgs)| msgs.iter().rev())
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .filter(|text| !text.trim().is_empty());
        // `fullCopy` metadata (v2 `forkSession`): the title kind travels with
        // the title, the roster and free-form custom document are copied, and
        // the environment binding is preserved. An explicit fork title is
        // user-authored, so it must not be replaced by a later generated one.
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

        // Everything the fork creates commits together. v2 rewrites the child
        // journal with one atomic file write; the equivalent here is a single
        // SQLite transaction, so a failed copy cannot leave behind the empty
        // child session that creating it separately used to commit.
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        // A sliced fork cuts the journal at a wire position, the way v2's
        // `computeForkLine` does: a line is the record's ordinal in the source
        // journal ordered by `seq` (v2 numbers records `index + 1`), never the
        // `seq` value itself — `seq` is an AUTOINCREMENT primary key and undo
        // deletes rows, so the numbers have gaps. `turn.prompt` rows are this
        // ledger's user-prompt anchors (v2 `isUndoAnchor`); the native loop
        // writes `message.user` instead. The cut is the anchor of the first
        // turn the fork drops, counted from the end of the journal, and with no
        // anchor to drop the whole journal is copied. Because the position is
        // journal order, two records written in the same millisecond are
        // ordered rather than ambiguous.
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
        // One `save_turn` per retained source turn, each re-stamped with its own
        // number so the fork's `next_turn_number` continues the sequence instead
        // of restarting it. The compaction summary turn is a real row in the
        // source and is copied like any other: dropping it would leave the
        // fork's history without the summary that explains its oldest messages.
        // A session with no turns still gets its child, matching the old
        // whole-session path.
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
        // v2 `forkSession`'s full copy carries the source's file history: the
        // fork continues the same work, and without these rows its undo could
        // not restore the files the copied turns changed. A sliced fork
        // (`turn_index.is_some()`) keeps none — v2's `pruneTruncatedForkFiles`
        // deletes the history when the turn sequence is cut. Rows are copied
        // per retained turn so `turn_id` (a turn *number*) points at the same
        // turn after the fork renumbers its sequence.
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
        // The fork's engine context is built by `fold_projection` over the wire
        // journal, so an empty journal means a fork with no history at all.
        //
        // `seq` is the table's global primary key, so the source's numbers
        // cannot be reused: the destination assigns fresh ones, and because the
        // copy runs in source-`seq` order the fold and the paged readers see the
        // same order they saw in the source. Ids are namespaced for the same
        // reason (`wire_events.id` is globally UNIQUE). Blob bytes are not
        // copied: `wire_blobs` is content-addressed and shared.
        //
        // The window is selected by line — `ROW_NUMBER()` over the source rows
        // in `seq` order — not by the `seq` value, so a cut keeps its meaning
        // after undo deletes leave gaps. `line < cut_line` keeps everything
        // before the anchor of the first dropped turn.
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
        Ok(true)
    }

    /// Lines (1-based journal ordinals, v2's wire `line`) of the user-prompt
    /// anchors a sliced fork cuts on.
    ///
    /// v2 counts undo anchors on `context.append_message` records
    /// (`conversationTime.ts` `isUndoAnchor`). This ledger's journal has no such
    /// record in either vocabulary and no turn id on the rows, so the prompt
    /// records are the only positional turn boundary: the engine journal writes
    /// one `turn.prompt` per turn (v2's own prompt record) and the native loop
    /// writes `message.user`. A session uses one of the two; prompt records win
    /// if both ever appear so a turn is not counted twice.
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

    /// Archive a session (v2 `archive` action): hidden from the default
    /// session list until restored. Stamps `archived_at` and leaves
    /// `updated_at` alone, matching v2 `setArchived`.
    pub fn archive_session(&self, session_id: &str) -> Result<bool, SessionRemovalError> {
        Self::ensure_not_occupied(session_id)?;
        Ok(self.set_session_archived(session_id, true)?)
    }

    /// Restore a previously archived session (v2 `restore` action). Clears
    /// `archived_at` and leaves `updated_at` alone.
    pub fn restore_session(&self, session_id: &str) -> Result<bool, SessionRemovalError> {
        Self::ensure_not_occupied(session_id)?;
        Ok(self.set_session_archived(session_id, false)?)
    }

    /// Sessions forked from the given session (v2 `/sessions/{id}/children`).
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

    /// Associate a child session with its parent.
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

    /// Deterministic title derivation from the session's own history
    /// (v2 `title/generate` source=first_turn|user_prompts). The `digest`
    /// source needs a managed LLM call and is rejected here.
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

    /// Undo the latest `count` completed turns in the session.
    ///
    /// Refuses to undo across a compaction boundary (the `turn-compact`
    /// summary produced by [`Self::compact_session`]): the pre-compaction
    /// messages no longer exist as rows, so deleting the summary turn would
    /// corrupt the projection — the same guarantee the TS event ledger
    /// enforces as `UndoCompactionBoundary` (event_store/mod.rs:43-47).
    /// Turns selected by an undo of `count` turns, newest first, as
    /// `(turn_id, turn_number)`. Refuses to cross the compaction boundary —
    /// both [`Self::undo_turns`] and [`Self::plan_undo_turns`] go through this
    /// so the refusal can never drift between planning and deleting.
    fn select_undo_turns(
        conn: &rusqlite::Connection,
        session_id: &str,
        count: usize,
    ) -> Result<Vec<(String, i64)>, String> {
        if count == 0 {
            return Err("undo refused: count must be a positive integer".into());
        }
        // v2 undoes user anchors, not "the newest N turn rows": a derived turn
        // (goal/cron/task) or an in-turn steer is not an anchor, and cutting at
        // an anchor removes every turn after it as well.
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

    /// Plan an undo without deleting anything: the `turn_number`s that would
    /// be reverted, newest first. Errors exactly like [`Self::undo_turns`]
    /// when the selection would cross the compaction boundary, so callers can
    /// refuse *before* touching workspace files.
    pub fn plan_undo_turns(&self, session_id: &str, count: usize) -> Result<Vec<i64>, String> {
        let conn = self.conn.lock();
        Ok(Self::select_undo_turns(&conn, session_id, count)?
            .into_iter()
            .map(|(_, turn_number)| turn_number)
            .collect())
    }

    pub fn undo_turns(&self, session_id: &str, count: usize) -> Result<usize, String> {
        let mut conn = self.conn.lock();
        // v2 `switchBranch` + `assertSwitchTripleAppended`: the switch re-reads
        // the log *after* writing and refuses when it changed mid-flight. One
        // immediate transaction gives the same guarantee in this ledger: the
        // selection is evaluated while the write lock is held (a concurrent
        // writer either finished before it or is refused with SQLITE_BUSY), and
        // a failure at any DELETE rolls the earlier ones back instead of
        // leaving a half-undone session.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let selected = Self::select_undo_turns(&tx, session_id, count)?;

        for (turn_id, turn_number) in &selected {
            tx.execute(
                "DELETE FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2",
                params![session_id, turn_number],
            )
            .map_err(|e| e.to_string())?;
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

        let now = chrono::Utc::now().timestamp_millis();
        tx.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;

        Ok(selected.len())
    }

    /// Revert file modifications recorded in a specific turn of a session,
    /// restoring files to their `content_before` states (or deleting newly created ones).
    pub fn revert_turn_file_changes(
        &self,
        session_id: &str,
        turn_id: usize,
        workspace_root: &Path,
    ) -> Result<Vec<String>, FileRevertError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT path, content_before, content_after, status, oversize
                 FROM session_file_history
                 WHERE session_id = ?1 AND turn_id = ?2 ORDER BY id DESC",
            )
            .map_err(|e| FileRevertError::Io(e.to_string()))?;

        let rows = stmt
            .query_map(params![session_id, turn_id as i64], |row| {
                let path: String = row.get(0)?;
                let content_before: Option<String> = row.get(1)?;
                let content_after: Option<String> = row.get(2)?;
                let status: String = row.get(3)?;
                let oversize: bool = row.get(4)?;
                Ok((path, content_before, content_after, status, oversize))
            })
            .map_err(|e| FileRevertError::Io(e.to_string()))?;

        let mut reverted = Vec::new();
        for item in rows {
            let (rel_path, before, after, status, oversize) =
                item.map_err(|e| FileRevertError::Io(e.to_string()))?;
            if oversize {
                // No pre-image was stored (over the history byte cap): v2 keeps
                // such a change in its `gaps` set and restores nothing for it.
                tracing::warn!(
                    %session_id,
                    path = %rel_path,
                    "cannot revert a file change without a stored pre-image; leaving the file untouched"
                );
                continue;
            }
            let full_path = workspace_root.join(&rel_path);
            let current = match std::fs::read_to_string(&full_path) {
                Ok(text) => Some(text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => {
                    return Err(FileRevertError::Io(format!(
                        "Failed to read {}: {error}",
                        full_path.display()
                    )));
                }
            };

            if status == "added" {
                // Nothing to delete when the file is already gone; deleting a
                // file whose content moved on would destroy a manual edit
                // (v2 `manual_edit`).
                let Some(current) = current else {
                    continue;
                };
                if let Some(expected) = &after
                    && *expected != current
                {
                    return Err(FileRevertError::Conflict {
                        path: rel_path,
                        reason: "it was edited after the tool wrote it".into(),
                    });
                }
                std::fs::remove_file(&full_path).map_err(|e| {
                    FileRevertError::Io(format!("Failed to remove {}: {e}", full_path.display()))
                })?;
            } else {
                let Some(current) = current else {
                    return Err(FileRevertError::Conflict {
                        path: rel_path,
                        reason: "it was deleted after the tool wrote it".into(),
                    });
                };
                if let Some(expected) = &after
                    && *expected != current
                {
                    return Err(FileRevertError::Conflict {
                        path: rel_path,
                        reason: "it was edited after the tool wrote it".into(),
                    });
                }
                let Some(text) = before else {
                    // The pre-image is missing for a non-oversize row: the same
                    // "no pre-image" case, left untouched rather than guessed.
                    tracing::warn!(
                        %session_id,
                        path = %full_path.display(),
                        "cannot revert a file change without a stored pre-image; leaving the file untouched"
                    );
                    continue;
                };
                if let Some(parent) = full_path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        FileRevertError::Io(format!("Failed to create {}: {e}", parent.display()))
                    })?;
                }
                std::fs::write(&full_path, text).map_err(|e| {
                    FileRevertError::Io(format!("Failed to restore {}: {e}", full_path.display()))
                })?;
            }
            reverted.push(rel_path);
        }
        Ok(reverted)
    }

    /// Compact history for the session using the manual compaction algorithm
    /// (`POST :compact` is a user-triggered compaction, so it scans from the
    /// tail for the deepest safe split instead of the auto window).
    ///
    /// Records a `compaction-boundary` checkpoint first so undo bookkeeping
    /// can refuse to cross it, mirroring the ledger's compaction markers
    /// (event_store `is_compaction`). `instruction` is the caller's
    /// summarization hint (v2 `rest-session.ts:131-137`); it is recorded with
    /// the checkpoint.
    ///
    /// Without a summary, only a no-op can succeed. Callers that hold an LLM
    /// use [`Self::compact_session_with_summary`] to replace history safely.
    pub fn compact_session(
        &self,
        session_id: &str,
        instruction: Option<&str>,
    ) -> Result<CompactionReport, String> {
        self.compact_session_with_summary(session_id, instruction, None)
    }

    /// [`Self::compact_session`] with a caller-supplied summary text.
    ///
    /// `summary` is the LLM-written replacement for the omitted prefix; missing
    /// or blank summaries are rejected before writing any history or checkpoint. The split point is recomputed here rather than
    /// taken from the caller, so a summary produced from a stale history
    /// cannot desynchronize the count from the messages it replaces.
    pub fn compact_session_with_summary(
        &self,
        session_id: &str,
        instruction: Option<&str>,
        summary: Option<String>,
    ) -> Result<CompactionReport, String> {
        let history = self
            .load_session_history(session_id)
            .map_err(|e| e.to_string())?;
        if history.len() <= 2 {
            return Ok(CompactionReport::skipped());
        }
        let config = crate::compaction::CompactionConfig::default();
        let count = crate::compaction::compute_compact_count_manual(&history, &config);
        if count == 0 {
            return Ok(CompactionReport::skipped());
        }
        let summary = summary
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| crate::compaction::CompactionError::EmptySummary.to_string())?;
        let compacted =
            crate::compaction::apply_compaction_with_summary(&history, count, summary, false);
        if compacted.len() >= history.len() {
            return Ok(CompactionReport::skipped());
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
                .filter(|message| message.role == "user")
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
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )
        .map_err(|e| e.to_string())?;
        drop(conn);
        self.save_turn(session_id, COMPACT_TURN_ID, 1, &compacted, None, None)
            .map_err(|e| e.to_string())?;
        Ok(report)
    }

    /// Set the session title as a **user-authored** title (v2 `setTitle`,
    /// sessionMetadataService.ts:135-137): `titleKind` becomes `custom`, and
    /// `updated_at` is deliberately left untouched (`touchUpdatedAt: false`).
    /// Renaming a session is not activity, and refreshing the timestamp would
    /// reorder the session list.
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

    /// Apply a generated title only when the existing title is not custom
    /// (v2 `setGeneratedTitleIfUncustomized`, sessionMetadataService.ts:139-148).
    ///
    /// `force` bypasses the custom guard, which is what a host-forced
    /// regeneration means. Returns whether the title was written; `false` means
    /// the custom title was preserved. Does not touch `updated_at`.
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

    /// Record the first-prompt metadata (v2 `applyPromptMetadata`,
    /// sessionMetadataService.ts:150-159).
    ///
    /// `last_prompt` is always written. The derived `title` replaces the
    /// current one only while it is still untitled and not custom, in which
    /// case the kind becomes `replaceable`. Returns whether the title was
    /// applied. Unlike the title setters, this is ordinary activity and does
    /// refresh `updated_at`.
    pub fn apply_prompt_metadata(
        &self,
        session_id: &str,
        last_prompt: &str,
        title: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let current: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT title, title_kind FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current_title, current_kind)) = current else {
            return Ok(false);
        };
        let apply_title =
            current_kind.as_deref() != Some("custom") && is_untitled(current_title.as_deref());
        if apply_title {
            conn.execute(
                "UPDATE sessions
                    SET last_prompt = ?2, title = ?3, title_kind = 'replaceable',
                        version = ?4, updated_at = ?5
                  WHERE session_id = ?1",
                params![session_id, last_prompt, title, SESSION_META_VERSION, now],
            )?;
        } else {
            conn.execute(
                "UPDATE sessions SET last_prompt = ?2, updated_at = ?3 WHERE session_id = ?1",
                params![session_id, last_prompt, now],
            )?;
        }
        Ok(apply_title)
    }

    /// Archive or restore a session (v2 `setArchived`,
    /// sessionMetadataService.ts:161-168): archive stamps `archived_at`,
    /// restore clears it, and neither touches `updated_at`.
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

    /// Import a session metadata document written by an earlier build
    /// (v2 `normalizeSessionMeta` + `sessionMetaTitleNeedsMigration`, then
    /// `encodeSessionMeta`'s write-back in `sessionMetadataService.load`).
    ///
    /// Legacy `state.json` documents carry `workDir`, `customTitle`,
    /// `isCustomTitle` and `titleSource` alongside the normalized fields. The
    /// import folds them into this store's columns (`title`, `title_kind`,
    /// `cwd`) and rewrites the metadata bag without the legacy keys. The
    /// columns win when they already hold a value: they are written by this
    /// build's own setters, so a stale legacy `isCustomTitle` must not undo a
    /// newer rename.
    ///
    /// Returns whether anything was written. A missing session is `Ok(false)`,
    /// like a load of a session that no longer exists.
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

        // v2: `cwd = clean.cwd ?? workDir` — the column is this build's `cwd`.
        let mut next_cwd = cwd.clone();
        if next_cwd.is_none()
            && let Some(work_dir) = metadata
                .get("workDir")
                .and_then(Value::as_str)
                .filter(|dir| !dir.trim().is_empty())
        {
            next_cwd = Some(work_dir.to_string());
        }

        // v2's precedence over the legacy title fields, applied only while the
        // column has no kind of its own.
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

        // v2 `encodeSessionMeta`: the normalized title kind is persisted and
        // its `isCustomTitle` compatibility bit is written alongside it.
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

    /// Persist a session's metadata bag with the `isCustomTitle`
    /// compatibility field every earlier build reads (`encodeSessionMeta`).
    ///
    /// The bit comes from the session's own `title_kind`, so a bag assembled
    /// before a rename cannot carry a stale flag.
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
        let value = dehydrate_state_value(&metadata, &conn)?;
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

    /// Register one agent in the session's roster (v2 `registerAgent`,
    /// sessionMetadataService.ts:170-188).
    ///
    /// An identical entry is a no-op. `updated_at` is not touched. Returns
    /// whether the roster changed.
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

    /// Write the free-form `custom` metadata document (v2 `custom`) without
    /// touching `updated_at`.
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

    /// Point a session at its execution environment (v2 `environmentId` /
    /// `environmentCwd`).
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

    /// Freeze a session's environment at creation (v2 `createSession` persists
    /// `environmentId` + `environmentCwd` on the session document).
    ///
    /// The variables live in `state_entries` under the `environment` domain so
    /// an existing database needs no new column; the binding also lands on the
    /// session row, where [`SessionSummary`] already surfaces it.
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

    /// Read the frozen environment back for a resume.
    ///
    /// `None` means the session has no binding (a row written before the
    /// columns existed, or a chat-only session). A row that exists but cannot
    /// be decoded is an error, never `None`: a corrupt snapshot must not make
    /// the next turn silently rebind to the current defaults.
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
        // A row that predates `environment_cwd` still knows its creation cwd
        // (`sessions.cwd` is written from the workspace root), so use it.
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

    /// Reconcile the persisted turn outcome against the turns the store
    /// actually holds (v2 `reconcileAfterRestore`,
    /// sessionOutcomeMirrorService.ts:131-145): no turn means no outcome; a
    /// turn present with no recorded outcome adopts its persisted status.
    /// Never touches `updated_at`.
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
            // Adopt the persisted turn status verbatim, except for a turn that
            // is still running: that outcome is not final, so there is nothing
            // to mirror yet.
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

    /// The turn number a new turn for this session should take, derived from
    /// the `turns` table so a caller that owns the store does not have to keep
    /// its own counter — one would reset on restart and collide on insert.
    pub fn next_turn_number(&self, session_id: &str) -> Result<u32, rusqlite::Error> {
        let conn = self.conn.lock();
        let highest: i64 = conn.query_row(
            "SELECT COALESCE(MAX(turn_number), 0) FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(highest.max(0) as u32 + 1)
    }

    /// Every message of a session with its append time, oldest first.
    pub fn load_session_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<StoredMessage>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT role, content, tool_calls, tool_call_id, blocks, prompt_id, created_at, turn_id FROM messages WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(StoredMessage {
                message: LLMMessage {
                    role: row.get(0)?,
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

    /// Every turn of a session, oldest first, with its execution metadata.
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

    /// Append turn messages and record turn execution metadata.
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

    /// Write one turn, its messages, and its session header on an existing
    /// transaction.
    ///
    /// The fork calls this inside its own transaction: creating the child and
    /// copying its turns either both commit or neither does.
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
                "INSERT INTO messages (session_id, turn_id, role, content, tool_calls, tool_call_id, blocks, prompt_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
                insert_message.execute(params![
                    session_id,
                    turn_id,
                    m.role,
                    m.content,
                    tool_calls_json,
                    m.tool_call_id,
                    blocks_json,
                    m.prompt_id,
                    now
                ])?;
            }
        }

        Ok(())
    }

    /// Load the linear conversation history for a session.
    /// The session history in the shape the engine replays it.
    ///
    /// Delegates to `load_session_messages` so the two reads cannot disagree
    /// about how a row becomes a message; this one drops the timestamps.
    pub fn load_session_history(
        &self,
        session_id: &str,
    ) -> Result<Vec<LLMMessage>, rusqlite::Error> {
        Ok(self
            .load_session_messages(session_id)?
            .into_iter()
            .map(|stored| stored.message)
            .collect())
    }

    /// Put a key-value pair in a state domain (state bridge storage).
    pub fn put_state(&self, domain: &str, key: &str, value: &Value) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        write_state_entry(&conn, domain, key, value)
    }

    /// Put a session-scoped state entry: the `session_id` column names the
    /// owning session, so `delete_session` cascades to it (ROADMAP §6.1 item
    /// 4's P2 decision — session-scoped state is session-scoped in the
    /// schema, not only in the key). The primary key stays `(domain, key)`;
    /// a legacy row under the same key adopts the owner on conflict.
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
        let value = dehydrate_state_value(value, &tx)?;
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

    /// The state entries a session owns through the `session_id` column, as
    /// `(domain, key, value)` — the cascade's own view of what it will
    /// delete.
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
            rehydrate_state_value(value, &conn)?;
        }
        Ok(entries)
    }

    /// Get a value from a state domain.
    pub fn get_state(&self, domain: &str, key: &str) -> Result<Option<Value>, rusqlite::Error> {
        let conn = self.conn.lock();
        read_state_entry(&conn, domain, key)
    }

    /// Apply an RFC 6902 JSON patch to a state entry.
    /// Returns the updated value and the inverse patch for undo.
    ///
    /// The read, the patch and the write all run under the store's one
    /// connection lock. Splitting them across `get_state` + `put_state` left a
    /// window where a concurrent writer's change was read, overwritten with
    /// this patch's base, and lost.
    pub fn patch_state(
        &self,
        domain: &str,
        key: &str,
        patch: &crate::session::patch::JsonPatchSet,
    ) -> Result<(Value, crate::session::patch::JsonPatchSet), String> {
        let conn = self.conn.lock();
        let mut val = read_state_entry(&conn, domain, key)
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let inverse = crate::session::patch::apply_patch(&mut val, patch)
            .map_err(|e| format!("failed to apply patch to {domain}/{key}: {e}"))?;
        write_state_entry(&conn, domain, key, &val).map_err(|e| e.to_string())?;
        Ok((val, inverse))
    }

    /// Compute RFC 6902 JSON diff between current state and a new value.
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

    /// Get the latest checkpoint for a session by name.
    pub fn get_latest_checkpoint(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<Option<Value>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            // `created_at` is millisecond-precision; two checkpoints saved in
            // the same millisecond tie on it, so break the tie with rowid
            // (insertion order) or "latest" resolves to an arbitrary row.
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
                rehydrate_state_value(&mut value, &conn)?;
                Ok(Some(value))
            }
            None => Ok(None),
        }
    }

    /// Save a named checkpoint for a session.
    pub fn save_checkpoint(
        &self,
        session_id: &str,
        checkpoint_id: &str,
        name: &str,
        data: &Value,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let data = dehydrate_state_value(data, &conn)?;
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

    /// Read a string value from the GUI store domain.
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

    /// Set a string value in the GUI store domain.
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

    /// Remove an item from the GUI store domain.
    pub fn gui_remove_item(&self, key: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let affected = conn.execute(
            "DELETE FROM state_entries WHERE domain = 'gui_store' AND key = ?1",
            params![key],
        )?;
        Ok(affected > 0)
    }

    /// Clear all items in the GUI store domain.
    pub fn gui_clear(&self) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM state_entries WHERE domain = 'gui_store'", [])?;
        Ok(())
    }

    /// Get count of items in the GUI store domain.
    pub fn gui_length(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let count: usize = conn.query_row(
            "SELECT COUNT(*) FROM state_entries WHERE domain = 'gui_store'",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// Search messages and session titles matching `query`.
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

    /// Count total messages in store.
    pub fn count_messages(&self) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let count: usize = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Record a file change checkpoint in a turn.
    ///
    /// Content over [`FILE_HISTORY_MAX_CONTENT_BYTES`] is not stored: the row
    /// keeps the change metadata and is marked `oversize`, so the retention
    /// window stays a bound on bytes and not only on rows.
    pub fn record_file_change(
        &self,
        session_id: &str,
        turn_id: usize,
        path: &str,
        content_before: Option<&str>,
        content_after: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let (status, additions, deletions) = compute_line_diff(content_before, content_after);
        let before_bytes = content_before.map_or(0, str::len);
        let after_bytes = content_after.map_or(0, str::len);
        let oversize = before_bytes > FILE_HISTORY_MAX_CONTENT_BYTES
            || after_bytes > FILE_HISTORY_MAX_CONTENT_BYTES;
        if oversize {
            tracing::warn!(
                %session_id,
                %path,
                before_bytes,
                after_bytes,
                limit_bytes = FILE_HISTORY_MAX_CONTENT_BYTES,
                "file history content is over the byte cap: recording the change without its content"
            );
        }
        let now = chrono::Utc::now().timestamp_millis();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO session_file_history (session_id, turn_id, path, status, content_before, content_after, additions, deletions, oversize, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                turn_id as i64,
                path,
                status,
                if oversize { None } else { content_before },
                if oversize { None } else { content_after },
                additions as i64,
                deletions as i64,
                oversize,
                now
            ],
        )?;
        // Auto-prune older entries beyond retention limit (500 records per session)
        conn.execute(
            "DELETE FROM session_file_history
             WHERE session_id = ?1 AND id NOT IN (
                 SELECT id FROM session_file_history
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT 500
             )",
            params![session_id],
        )?;
        // v2 `checkpointService.ts:20` retains the last `CHECKPOINT_MAX_TURNS`
        // turns of snapshots; the row cap above bounds bytes, this bounds how
        // far back an undo can restore files.
        conn.execute(
            "DELETE FROM session_file_history
             WHERE session_id = ?1 AND turn_id NOT IN (
                 SELECT DISTINCT turn_id FROM session_file_history
                 WHERE session_id = ?1
                 ORDER BY turn_id DESC
                 LIMIT ?2
             )",
            params![session_id, CHECKPOINT_MAX_TURNS],
        )?;
        Ok(())
    }

    /// Explicitly prune file history records for a session to enforce retention limit.
    ///
    /// The delete frees the pages into SQLite's freelist; the freed space only
    /// leaves the file (and the WAL) when that list is large enough to be
    /// worth returning, so this also asks for an incremental vacuum and
    /// truncates the WAL when the store is in WAL mode.
    pub fn prune_file_history(
        &self,
        session_id: &str,
        max_entries: usize,
    ) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        let deleted = conn.execute(
            "DELETE FROM session_file_history
             WHERE session_id = ?1 AND id NOT IN (
                 SELECT id FROM session_file_history
                 WHERE session_id = ?1
                 ORDER BY id DESC
                 LIMIT ?2
             )",
            params![session_id, max_entries as i64],
        )?;
        if deleted > 0 {
            reclaim_file_history_space(&conn)?;
        }
        Ok(deleted)
    }

    /// List turn-level file history changes for a session.
    pub fn get_file_history_changes(
        &self,
        session_id: &str,
        turn_id: Option<usize>,
    ) -> Result<(Vec<FileHistoryChange>, bool), rusqlite::Error> {
        let conn = self.conn.lock();
        let (query, has_turn) = match turn_id {
            Some(t) => (
                "SELECT path, status, additions, deletions, oversize FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2 ORDER BY id ASC",
                Some(t as i64),
            ),
            None => (
                "SELECT path, status, additions, deletions, oversize FROM session_file_history WHERE session_id = ?1 ORDER BY id ASC",
                None,
            ),
        };

        let mut stmt = conn.prepare(query)?;
        let mut changes = Vec::new();
        let mut rows = match has_turn {
            Some(t) => stmt.query(params![session_id, t])?,
            None => stmt.query(params![session_id])?,
        };

        while let Some(row) = rows.next()? {
            changes.push(FileHistoryChange {
                path: row.get(0)?,
                status: row.get(1)?,
                additions: row.get::<_, i64>(2)? as usize,
                deletions: row.get::<_, i64>(3)? as usize,
                binary: Some(false),
                oversize: Some(row.get::<_, i64>(4)? != 0),
            });
        }

        let recorded = !changes.is_empty();
        Ok((changes, recorded))
    }

    /// Retrieve file content for a given turn and path.
    pub fn get_file_history_content(
        &self,
        session_id: &str,
        turn_id: usize,
        path: &str,
        phase: &str,
    ) -> Result<Option<FileHistoryContent>, rusqlite::Error> {
        let conn = self.conn.lock();
        let query = if phase == "start" {
            "SELECT content_before, id, oversize FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2 AND path = ?3 ORDER BY id ASC LIMIT 1"
        } else {
            "SELECT content_after, id, oversize FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2 AND path = ?3 ORDER BY id DESC LIMIT 1"
        };

        let mut stmt = conn.prepare(query)?;
        let mut rows = stmt.query(params![session_id, turn_id as i64, path])?;
        if let Some(row) = rows.next()? {
            let content: Option<String> = row.get(0)?;
            let version: i64 = row.get(1)?;
            let oversize: i64 = row.get(2)?;
            Ok(Some(FileHistoryContent {
                version: version as usize,
                content,
                binary: Some(false),
                oversize: (oversize != 0).then_some(true),
            }))
        } else {
            Ok(None)
        }
    }

    /// Append a raw wire event to the `wire_events` table and return its auto-increment sequence number.
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

    /// Append a wire event whose payload is already serialized. The per-event
    /// persistence path serializes the event once for the record, so this entry
    /// point avoids a second `Value` round-trip inside the store.
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
        // The occupancy write guard (v2 `bindWriteGuard`, upstream #4246): a
        // live foreign holder closes the journal to us. Checks rather than
        // v2's acquire — the turn seams hold the lock for their duration, so
        // this seam's job is refusal (see session::lock's module note).
        if let Some(holder) = crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id) {
            return Err(EventStoreError::SessionOccupied(
                crate::session::lock::session_occupied_message(Some(&holder)),
            ));
        }
        let conn = self.conn.lock();
        // Dehydrate before the row is written (v2 `appendRecord` →
        // `RecordDehydrator`): a large inline media part becomes a `blobref:`
        // URL and its bytes live once in `wire_blobs`.
        //
        // Blob bytes and the event row commit together: a row insert that fails
        // after the offload must not leave bytes in `wire_blobs` that no row
        // references (content without a reference is unreachable, and nothing
        // in this store reclaims it).
        let tx = conn.unchecked_transaction()?;
        let payload_json = if payload_json.contains("data:") {
            let mut payload: serde_json::Value = serde_json::from_str(payload_json)?;
            if crate::native::event_store::blobs::offload_data_uris(&mut payload, &tx)? {
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

    /// Query recorded wire events for a session strictly after `since_seq` up to `limit`.
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
        // Expand the media the write dehydrated, so a reader receives the same
        // content the model saw (v2 `rehydrateStates` → `loadParts`).
        for record in &mut records {
            crate::native::event_store::blobs::rehydrate_blob_refs(&mut record.payload, &conn)?;
        }
        Ok(records)
    }

    /// Paged journal rows for read-only full-text search (`session_query`).
    ///
    /// Same journal and ordering as [`Self::get_wire_events`], but without
    /// expanding `blobref:` media: a search reads only the textual fields, and
    /// rehydrating every image in a session would pull the whole media payload
    /// into memory for a query that never shows it.
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
        // Unlike the replay readers, search keeps a corrupt row a hard error:
        // its caller (`session_query`) tells the user the journal is corrupt
        // rather than answering "no matches", which would read as a legitimate
        // empty result.
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

    /// The session's interaction lifecycle events (`event.approval.*` /
    /// `event.question.*`), oldest first — the v3 history fold's source for
    /// interaction entities (ROADMAP §6.1 item 4). The hub persister writes
    /// every lane event here, so the journal is the durable record; this
    /// selects the six interaction types rather than paging the whole log.
    pub fn interaction_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        const TYPES: [&str; 6] = [
            "event.approval.requested",
            "event.approval.resolved",
            "event.approval.expired",
            "event.question.requested",
            "event.question.answered",
            "event.question.dismissed",
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
                session_id, TYPES[0], TYPES[1], TYPES[2], TYPES[3], TYPES[4], TYPES[5]
            ],
            wire_record_from_row,
        )?;
        collect_logged_wire_rows(rows, session_id, "interaction")
    }

    /// The session's prompt-lifecycle events (`prompt.submitted` /
    /// `prompt.completed` / `prompt.aborted` / `prompt.steered`), oldest
    /// first — the cold baseline's source for prompt entities
    /// (`transcript::cold_prompts`). The hub persister writes every lane
    /// event here, so the journal is the durable record; this selects the
    /// four prompt types rather than paging the whole log.
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

    /// The session's `turn.prompt` records, oldest first — the wire turn id
    /// axis the cold baseline realigns its positionally-numbered turns to
    /// (`transcript::cold_items`, upstream `foldFacts.ts`'s `wireIdByOrdinal`).
    /// Same journal as [`Self::prompt_wire_events`], different slice.
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

    /// The session's task-relevant events (subagent lifecycle, task
    /// lifecycle, and the turn boundaries the lost-member decision keys off),
    /// oldest first — the cold baseline's source for task entities
    /// (`transcript::cold_tasks`, upstream #3970). Same journal as
    /// [`Self::prompt_wire_events`], different slice.
    pub fn task_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, EventStoreError> {
        const TYPES: [&str; 8] = [
            "subagent.spawned",
            "subagent.completed",
            "subagent.failed",
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

    /// Count total wire events recorded for a session.
    pub fn count_wire_events(&self, session_id: &str) -> Result<usize, rusqlite::Error> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*) FROM wire_events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
    }

    /// Query the maximum sequence number recorded for a session, or 0 if none exist.
    pub fn latest_wire_event_seq(&self, session_id: &str) -> Result<u64, rusqlite::Error> {
        let conn = self.conn.lock();
        let max_seq: Option<u64> = conn
            .query_row(
                "SELECT MAX(seq) FROM wire_events WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(max_seq.unwrap_or(0))
    }

    /// Persist the main agent's most recent turn outcome
    /// (v2 `SessionOutcomeMirror`).
    ///
    /// Deliberately does **not** touch `updated_at`. This mirrors a turn that
    /// already ended on another path, and treating it as activity would reorder
    /// the session list on every turn end — v2 passes `touchUpdatedAt: false`
    /// for the same reason.
    pub fn set_last_turn_reason(
        &self,
        session_id: &str,
        reason: Option<&str>,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        // v2 `setLastTurnReason` returns early when the reason is unchanged, so
        // a cleared-at-start turn does not rewrite the row every time. `IS NOT`
        // (not `!=`) so the NULL → NULL case is the no-op too.
        conn.execute(
            "UPDATE sessions SET last_turn_reason = ?2
             WHERE session_id = ?1 AND last_turn_reason IS NOT ?2",
            params![session_id, reason],
        )?;
        Ok(())
    }

    /// The persisted outcome, for the session wire. `None` for a session that
    /// has not ended a turn since it was created, and for rows written before
    /// the column existed.
    pub fn last_turn_reason(&self, session_id: &str) -> Option<String> {
        let conn = self.conn.lock();
        // `query_row` already yields `Option<String>` for a NULL column, and
        // `QueryReturnedNoRows` covers an unknown session; `.ok().flatten()`
        // collapses both into the single `None` the wire wants.
        conn.query_row(
            "SELECT last_turn_reason FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    /// Fold stored wire events for `session_id` into a sanitized context message projection.
    ///
    /// Rows whose payload is not valid JSON are skipped and counted, not
    /// propagated: one bad payload must not make the whole session unopenable.
    /// The same read repairs the log (v2 `readEntries` → `repairWireJournal`):
    /// the original is preserved once beside the database and the undecodable
    /// rows are dropped, so the next open does not hit them again.
    pub fn fold_projection(&self, session_id: &str) -> Result<Vec<Message>, EventStoreError> {
        let backup = self.path().map(|path| wire_backup_path(path, session_id));
        // The repair is a write to the journal. While another process holds the
        // session, this read must not delete rows that holder may be appending
        // to: v2's rewrite goes through the append log's write guard, which is
        // the same rule. The projection is still built from every row that
        // decodes; only the rewrite is deferred to the next exclusive open.
        let occupied = crate::session::lock::SESSION_LOCKS.occupied_by_other(session_id);
        let raw_rows = {
            let conn = self.conn.lock();
            if occupied.is_some() {
                tracing::warn!(
                    code = "wire.repair_deferred",
                    %session_id,
                    "session is held by another process; skipping wire repair on this read"
                );
                crate::native::event_store::read_fold_rows(&conn, session_id)?.0
            } else {
                read_fold_rows_with_repair(&conn, session_id, backup.as_deref())?.0
            }
        };

        fold_wire_events(raw_rows.iter().map(|(t, p, c)| (t.as_str(), p, *c)))
    }

    /// Compress conversation context by inserting a compaction checkpoint boundary event.
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

    /// Undo events backward to the most recent checkpoint. Refuses across compaction boundaries.
    pub fn undo_to_last_checkpoint(&self, session_id: &str) -> Result<usize, EventStoreError> {
        // Undo deletes journal rows, so it is a write: the same occupancy
        // refusal [`Self::append_wire_event`] applies (v2's write guard covers
        // the rewrite/undo path too, not only appends).
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

/// Deterministic title derivation from a conversation's user prompts (v2
/// `title/generate` source=first_turn|user_prompts): the first non-empty
/// user message, run through the same secret-redaction / whitespace pipeline
/// as v2's `promptMetadataTextFromText`, then capped at 200 characters
/// (`titleFromPromptMetadataText`, `promptMetadataText.ts:4-9`). The `digest`
/// source is the managed `chat_title` call and lives in
/// [`crate::session::title::generate_session_title`]; it is rejected here so a
/// caller reaching this function directly cannot silently get the
/// deterministic title instead.
/// Free-standing so the napi embedded-session path (which owns its history
/// in memory, not in this store) applies the identical rule.
pub fn derive_session_title(
    history: &[LLMMessage],
    source: Option<&str>,
) -> Result<Option<String>, String> {
    /// v2 `MAX_TITLE_LENGTH` (`titleFromPromptMetadataText`).
    const MAX_TITLE_LENGTH: usize = 200;
    match source.unwrap_or("user_prompts") {
        // Digest needs the managed `chat_title` call, which is async and needs
        // the session's LLM — neither is available here. Route the caller to
        // the entry point that has them instead of silently returning a title
        // derived from the wrong source.
        "digest" => {
            return Err(
                "source=digest is the managed chat_title call; use session::title::generate_session_title".into(),
            )
        }
        "first_turn" | "user_prompts" => {}
        // v2 `composeTitleInput` falls through to the prompt list for anything
        // that is not `first_turn` or `digest`, so an unrecognised source
        // degrades to `user_prompts` rather than failing the request.
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
    // Character-wise: `String::truncate` counts bytes and panics when the cut
    // lands inside a code point, which a CJK prompt reaches immediately.
    Ok(crate::session::title::prompt_metadata_text(prompt)
        .map(|text| text.chars().take(MAX_TITLE_LENGTH).collect()))
}

#[cfg(test)]
mod tests;
