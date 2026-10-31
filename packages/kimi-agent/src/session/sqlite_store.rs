//! SQLite-backed session persistence store.
//!
//! Provides zero-dependency standalone persistence for sessions, turns,
//! messages, checkpoints, and arbitrary state domains using embedded SQLite (WAL mode).

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::native::event_store::{
    EventStore, EventStoreError, Message, RawWireEvent, fold_wire_events, read_fold_rows,
};
use crate::rpc::types::TokenUsage;
use crate::turn_loop::types::LLMMessage;

/// Format a millisecond timestamp as an ISO-8601 string.
fn format_iso(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExport {
    pub session: SessionSummary,
    pub messages: Vec<LLMMessage>,
    pub turns_count: usize,
    pub exported_at: String,
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
    // and the space is reclaimed by the next prune. Only a hard failure
    // (a closed or corrupt database) is worth surfacing, and even that must
    // not undo a prune that already succeeded, so it is tolerated here rather
    // than propagated.
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
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

impl SqliteSessionStore {
    /// Open or create a SQLite database file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Create an in-memory SQLite database.
    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, rusqlite::Error> {
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
                last_turn_reason TEXT
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
                trusted INTEGER NOT NULL DEFAULT 1
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
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_wire_session_seq ON wire_events(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_messages_session_id ON messages(session_id, id);
            ",
        )?;

        // Ensure columns exist if table was created by older schema
        let columns: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            names.filter_map(std::result::Result::ok).collect()
        };
        if !columns.contains("tool_calls") {
            let _ = conn.execute(
                "ALTER TABLE wire_events ADD COLUMN protocol_version TEXT",
                [],
            );
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN last_turn_reason TEXT", []);
            let _ = conn.execute("ALTER TABLE messages ADD COLUMN tool_calls TEXT", []);
        }
        if !columns.contains("tool_call_id") {
            let _ = conn.execute("ALTER TABLE messages ADD COLUMN tool_call_id TEXT", []);
        }
        if !columns.contains("blocks") {
            let _ = conn.execute("ALTER TABLE messages ADD COLUMN blocks TEXT", []);
        }
        if !columns.contains("prompt_id") {
            let _ = conn.execute("ALTER TABLE messages ADD COLUMN prompt_id TEXT", []);
        }

        let session_columns: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            names.filter_map(std::result::Result::ok).collect()
        };
        if !session_columns.contains("workspace_id") {
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN workspace_id TEXT", []);
        }
        if !session_columns.contains("archived") {
            let _ = conn.execute(
                "ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }
        if !session_columns.contains("parent_session_id") {
            let _ = conn.execute("ALTER TABLE sessions ADD COLUMN parent_session_id TEXT", []);
        }

        let turn_columns: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(turns)")?;
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            names.filter_map(std::result::Result::ok).collect()
        };
        if !turn_columns.contains("origin") {
            let _ = conn.execute("ALTER TABLE turns ADD COLUMN origin TEXT", []);
        }

        // The `oversize` marker for a file-history row whose content was too
        // large to store (see `FILE_HISTORY_MAX_CONTENT_BYTES`). Rows written
        // before the column existed keep 0.
        let file_history_columns: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(session_file_history)")?;
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            names.filter_map(std::result::Result::ok).collect()
        };
        if !file_history_columns.contains("oversize") {
            let _ = conn.execute(
                "ALTER TABLE session_file_history ADD COLUMN oversize INTEGER NOT NULL DEFAULT 0",
                [],
            );
        }

        // Session-scoped state (ROADMAP §6.1 item 4's P2 decision): the
        // column names the owning session so a delete cascades to the
        // session's entries. Rows written before it existed keep NULL and
        // are cascaded by their key pattern instead (see `delete_session`).
        let state_columns: std::collections::HashSet<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(state_entries)")?;
            let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
            names.filter_map(std::result::Result::ok).collect()
        };
        if !state_columns.contains("session_id") {
            let _ = conn.execute("ALTER TABLE state_entries ADD COLUMN session_id TEXT", []);
            let _ = conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_state_entries_session ON state_entries(session_id)",
                [],
            );
        }

        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

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
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO sessions (session_id, title, created_at, updated_at, workspace_id)
             VALUES (?1, ?2, ?3, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                title = coalesce(?2, title),
                updated_at = ?3,
                workspace_id = coalesce(?4, workspace_id)",
            params![session_id, title, now, workspace_id],
        )?;
        Ok(())
    }

    /// List all persisted sessions ordered by last updated time.
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, title, created_at, updated_at, workspace_id, archived, parent_session_id FROM sessions ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                workspace_id: row.get(4)?,
                archived: row.get::<_, i64>(5)? != 0,
                parent_session_id: row.get(6)?,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Get summary for a specific session by ID.
    pub fn get_session(&self, session_id: &str) -> Result<Option<SessionSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, title, created_at, updated_at, workspace_id, archived, parent_session_id FROM sessions WHERE session_id = ?1",
        )?;
        let mut rows = stmt.query(params![session_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SessionSummary {
                session_id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                workspace_id: row.get(4)?,
                archived: row.get::<_, i64>(5)? != 0,
                parent_session_id: row.get(6)?,
            }))
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
        let turns_count: usize = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )
            .unwrap_or(0)
        };
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(Some(SessionExport {
            session,
            messages,
            turns_count,
            exported_at: now_iso,
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
             VALUES (?1, ?2, ?3, ?4, ?4, 1)
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

    /// Query whether a workspace is trusted. Default to true if unconfigured.
    pub fn is_workspace_trusted(&self, workspace_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let trusted: Option<i64> = conn
            .query_row(
                "SELECT trusted FROM workspaces WHERE workspace_id = ?1",
                params![workspace_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(trusted.map(|v| v != 0).unwrap_or(true))
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

    /// Delete a session and all its cascading turns, messages, and checkpoints.
    pub fn delete_session(&self, session_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        // The state entries the session owns: the column-named ones, plus the
        // legacy key-encoded domains whose key *is* the session id
        // (`agent_config` / `metadata`, written before the column existed).
        // The turns / messages / wire_events / file-history rows go with the
        // session through their foreign keys.
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

        // Validation is already done, so creating the child now cannot leave an
        // orphan session behind on a rejected `turnIndex`.
        let effective_title = title.or(source.title.as_deref());
        self.create_session_with_workspace(
            new_session_id,
            effective_title,
            source.workspace_id.as_deref(),
        )?;
        // Children are queryable via /sessions/{id}/children.
        self.link_to_parent(source_session_id, new_session_id)?;
        // One `save_turn` per retained source turn, each re-stamped with its own
        // number so the fork's `next_turn_number` continues the sequence instead
        // of restarting it. The compaction summary turn is a real row in the
        // source and is copied like any other: dropping it would leave the
        // fork's history without the summary that explains its oldest messages.
        // A session with no turns still gets its child, matching the old
        // whole-session path.
        for (number, (turn, turn_messages)) in retained.iter().enumerate() {
            self.save_turn(
                new_session_id,
                &forked_turn_id(&turn.turn_id, new_session_id, number as u32 + 1),
                number as u32 + 1,
                turn_messages,
                turn.usage.as_ref(),
                turn.origin.as_ref(),
            )?;
        }
        Ok(true)
    }

    /// Point `new_session_id` at `parent_session_id`, which is what makes the
    /// fork queryable through `/sessions/{id}/children`.
    fn link_to_parent(
        &self,
        source_session_id: &str,
        new_session_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE sessions SET parent_session_id = ?1 WHERE session_id = ?2",
            params![source_session_id, new_session_id],
        )?;
        Ok(())
    }

    /// Archive a session (v2 `archive` action): hidden from the default
    /// session list until restored.
    pub fn archive_session(&self, session_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE sessions SET archived = 1, updated_at = ?2 WHERE session_id = ?1",
            params![session_id, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(changed > 0)
    }

    /// Restore a previously archived session (v2 `restore` action).
    pub fn restore_session(&self, session_id: &str) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE sessions SET archived = 0, updated_at = ?2 WHERE session_id = ?1",
            params![session_id, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(changed > 0)
    }

    /// Sessions forked from the given session (v2 `/sessions/{id}/children`).
    pub fn list_children(&self, session_id: &str) -> Result<Vec<SessionSummary>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT session_id, title, created_at, updated_at, workspace_id, archived, parent_session_id FROM sessions WHERE parent_session_id = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                title: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                workspace_id: row.get(4)?,
                archived: row.get::<_, i64>(5)? != 0,
                parent_session_id: row.get(6)?,
            })
        })?;
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
        let conn = self.conn.lock();
        let selected = Self::select_undo_turns(&conn, session_id, count)?;

        for (turn_id, turn_number) in &selected {
            conn.execute(
                "DELETE FROM session_file_history WHERE session_id = ?1 AND turn_id = ?2",
                params![session_id, turn_number],
            )
            .map_err(|e| e.to_string())?;
            conn.execute(
                "DELETE FROM messages WHERE session_id = ?1 AND turn_id = ?2",
                params![session_id, turn_id],
            )
            .map_err(|e| e.to_string())?;
            conn.execute(
                "DELETE FROM turns WHERE turn_id = ?1 AND session_id = ?2",
                params![turn_id, session_id],
            )
            .map_err(|e| e.to_string())?;
        }

        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )
        .map_err(|e| e.to_string())?;

        Ok(selected.len())
    }

    /// Revert file modifications recorded in a specific turn of a session,
    /// restoring files to their `content_before` states (or deleting newly created ones).
    pub fn revert_turn_file_changes(
        &self,
        session_id: &str,
        turn_id: usize,
        workspace_root: &Path,
    ) -> Result<Vec<String>, String> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT path, content_before, status FROM session_file_history
                 WHERE session_id = ?1 AND turn_id = ?2 ORDER BY id DESC",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(params![session_id, turn_id as i64], |row| {
                let path: String = row.get(0)?;
                let content_before: Option<String> = row.get(1)?;
                let status: String = row.get(2)?;
                Ok((path, content_before, status))
            })
            .map_err(|e| e.to_string())?;

        let mut reverted = Vec::new();
        for item in rows {
            let (rel_path, before, status) = item.map_err(|e| e.to_string())?;
            let full_path = workspace_root.join(&rel_path);
            if status == "added" {
                if full_path.exists() {
                    let _ = std::fs::remove_file(&full_path);
                }
            } else if let Some(text) = before {
                if let Some(parent) = full_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&full_path, text)
                    .map_err(|e| format!("Failed to restore {}: {e}", full_path.display()))?;
            } else {
                // The pre-image was not stored (a file over the history byte
                // cap). Treating "no pre-image" as "the file was added" deleted
                // the user's file; leave it untouched instead.
                tracing::warn!(
                    %session_id,
                    path = %full_path.display(),
                    "cannot revert a file change without a stored pre-image; leaving the file untouched"
                );
                continue;
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
        let compacted = crate::compaction::apply_compaction_with_summary(&history, count, summary);
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

    /// Update the session title and refresh updated_at timestamp.
    pub fn update_session_title(
        &self,
        session_id: &str,
        title: Option<&str>,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp_millis();
        let affected = conn.execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, title, now],
        )?;
        Ok(affected > 0)
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
                    blocks: row
                        .get::<_, Option<String>>(4)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                    tool_calls: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| serde_json::from_str(&s).ok())
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
            let usage: Option<String> = row.get(5)?;
            Ok(TurnRecord {
                turn_id: row.get(0)?,
                turn_number: row.get::<_, i64>(1)?.max(0) as u32,
                status: row.get(2)?,
                started_at: row.get(3)?,
                completed_at: row.get(4)?,
                usage: usage.and_then(|s| serde_json::from_str(&s).ok()),
                origin: row
                    .get::<_, Option<String>>(6)?
                    .and_then(|s| serde_json::from_str(&s).ok()),
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
        let now = chrono::Utc::now().timestamp_millis();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO sessions (session_id, title, created_at, updated_at)
             VALUES (?1, NULL, ?2, ?2)
             ON CONFLICT(session_id) DO UPDATE SET updated_at = ?2",
            params![session_id, now],
        )?;

        let usage_json = usage.map(|u| serde_json::to_string(u).unwrap_or_default());

        let origin_json = origin.map(|o| serde_json::to_string(o).unwrap_or_default());
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
                    serde_json::to_string(&m.tool_calls).ok()
                };
                let blocks_json = if m.blocks.is_empty() {
                    None
                } else {
                    serde_json::to_string(&m.blocks).ok()
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

        tx.commit()?;
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
        let now = chrono::Utc::now().timestamp_millis();
        let val_str = serde_json::to_string(value).unwrap_or_default();
        conn.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?3,
                updated_at = ?4",
            params![domain, key, val_str, now],
        )?;
        Ok(())
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
        let val_str = serde_json::to_string(value).unwrap_or_default();
        conn.execute(
            "INSERT INTO state_entries (domain, key, value, updated_at, session_id)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(domain, key) DO UPDATE SET
                value = ?3,
                updated_at = ?4,
                session_id = ?5",
            params![domain, key, val_str, now, session_id],
        )?;
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
                domain,
                key,
                serde_json::from_str(&raw).unwrap_or(Value::Null),
            ))
        })?;
        rows.collect()
    }

    /// Get a value from a state domain.
    pub fn get_state(&self, domain: &str, key: &str) -> Result<Option<Value>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT value FROM state_entries WHERE domain = ?1 AND key = ?2")?;
        let mut rows = stmt.query(params![domain, key])?;
        if let Some(row) = rows.next()? {
            let s: String = row.get(0)?;
            let val: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
            Ok(Some(val))
        } else {
            Ok(None)
        }
    }

    /// Apply an RFC 6902 JSON patch to a state entry.
    /// Returns the updated value and the inverse patch for undo.
    pub fn patch_state(
        &self,
        domain: &str,
        key: &str,
        patch: &crate::session::patch::JsonPatchSet,
    ) -> Result<(Value, crate::session::patch::JsonPatchSet), String> {
        let mut val = self
            .get_state(domain, key)
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let inverse = crate::session::patch::apply_patch(&mut val, patch)
            .map_err(|e| format!("failed to apply patch to {domain}/{key}: {e}"))?;
        self.put_state(domain, key, &val)
            .map_err(|e| e.to_string())?;
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
        let mut rows = stmt.query(params![session_id, name])?;
        if let Some(row) = rows.next()? {
            let data_str: String = row.get(0)?;
            let val: Value = serde_json::from_str(&data_str).unwrap_or(Value::Null);
            Ok(Some(val))
        } else {
            Ok(None)
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
        let data_str = serde_json::to_string(data).unwrap_or_default();
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
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO wire_events (id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                session_id,
                event_type,
                payload_json,
                is_checkpoint,
                is_compaction,
                created_at
            ],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Query recorded wire events for a session strictly after `since_seq` up to `limit`.
    pub fn get_wire_events(
        &self,
        session_id: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<Vec<WireEventRecord>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT seq, id, session_id, event_type, payload, is_checkpoint, is_compaction, created_at
             FROM wire_events
             WHERE session_id = ?1 AND seq > ?2
             ORDER BY seq ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![session_id, since_seq, limit as i64], |row| {
            let payload_str: String = row.get(4)?;
            let payload: Value = serde_json::from_str(&payload_str).unwrap_or(Value::Null);
            Ok(WireEventRecord {
                seq: row.get(0)?,
                id: row.get(1)?,
                session_id: row.get(2)?,
                event_type: row.get(3)?,
                payload,
                is_checkpoint: row.get(5)?,
                is_compaction: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// The session's interaction lifecycle events (`event.approval.*` /
    /// `event.question.*`), oldest first — the v3 history fold's source for
    /// interaction entities (ROADMAP §6.1 item 4). The hub persister writes
    /// every lane event here, so the journal is the durable record; this
    /// selects the six interaction types rather than paging the whole log.
    pub fn interaction_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, rusqlite::Error> {
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
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                session_id, TYPES[0], TYPES[1], TYPES[2], TYPES[3], TYPES[4], TYPES[5]
            ],
            |row| {
                let payload_str: String = row.get(4)?;
                let payload: Value = serde_json::from_str(&payload_str).unwrap_or(Value::Null);
                Ok(WireEventRecord {
                    seq: row.get(0)?,
                    id: row.get(1)?,
                    session_id: row.get(2)?,
                    event_type: row.get(3)?,
                    payload,
                    is_checkpoint: row.get(5)?,
                    is_compaction: row.get(6)?,
                    created_at: row.get(7)?,
                })
            },
        )?;
        rows.collect()
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
    ) -> Result<Vec<WireEventRecord>, rusqlite::Error> {
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
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![session_id, TYPES[0], TYPES[1], TYPES[2], TYPES[3]],
            |row| {
                let payload_str: String = row.get(4)?;
                let payload: Value = serde_json::from_str(&payload_str).unwrap_or(Value::Null);
                Ok(WireEventRecord {
                    seq: row.get(0)?,
                    id: row.get(1)?,
                    session_id: row.get(2)?,
                    event_type: row.get(3)?,
                    payload,
                    is_checkpoint: row.get(5)?,
                    is_compaction: row.get(6)?,
                    created_at: row.get(7)?,
                })
            },
        )?;
        rows.collect()
    }

    /// The session's task-relevant events (subagent lifecycle, task
    /// lifecycle, and the turn boundaries the lost-member decision keys off),
    /// oldest first — the cold baseline's source for task entities
    /// (`transcript::cold_tasks`, upstream #3970). Same journal as
    /// [`Self::prompt_wire_events`], different slice.
    pub fn task_wire_events(
        &self,
        session_id: &str,
    ) -> Result<Vec<WireEventRecord>, rusqlite::Error> {
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
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::types::ToSql> = vec![&session_id];
        params.extend(TYPES.iter().map(|t| t as &dyn rusqlite::types::ToSql));
        let rows = stmt.query_map(params.as_slice(), |row| {
            let payload_str: String = row.get(4)?;
            let payload: Value = serde_json::from_str(&payload_str).unwrap_or(Value::Null);
            Ok(WireEventRecord {
                seq: row.get(0)?,
                id: row.get(1)?,
                session_id: row.get(2)?,
                event_type: row.get(3)?,
                payload,
                is_checkpoint: row.get(5)?,
                is_compaction: row.get(6)?,
                created_at: row.get(7)?,
            })
        })?;
        rows.collect()
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
        conn.execute(
            "UPDATE sessions SET last_turn_reason = ?2 WHERE session_id = ?1",
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
    pub fn fold_projection(&self, session_id: &str) -> Result<Vec<Message>, EventStoreError> {
        let raw_rows = {
            let conn = self.conn.lock();
            read_fold_rows(&conn, session_id)?.0
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
/// user message, whitespace-normalized, truncated to 60 chars. The `digest`
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
    let title: String = prompt
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let title = title.trim();
    // Character-wise: `String::truncate` counts bytes and panics when the cut
    // lands inside a code point, which a CJK prompt reaches immediately.
    Ok(Some(title.chars().take(60).collect()))
}

#[cfg(test)]
mod tests;
