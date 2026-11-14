use indexmap::IndexSet;
use once_cell::sync::Lazy;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type Result<T> = std::result::Result<T, String>;

static DATABASE: Lazy<Mutex<Option<Connection>>> = Lazy::new(|| Mutex::new(None));

static DATABASE_PATH: Lazy<Mutex<Option<PathBuf>>> = Lazy::new(|| Mutex::new(None));

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS entries (
    id          TEXT PRIMARY KEY,
    category    TEXT NOT NULL CHECK(category IN ('coding-style','pitfall','architecture','workflow')),
    title       TEXT NOT NULL,
    content     TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '',
    scope       TEXT DEFAULT NULL,
    confidence  REAL NOT NULL DEFAULT 1.0,
    source      TEXT NOT NULL CHECK(source IN ('human','ai-learned','ai-confirmed')),
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    title, content, tags, content='entries', content_rowid='rowid'
);
CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid, title, content, tags) VALUES (new.rowid, new.title, new.content, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, content, tags) VALUES('delete', old.rowid, old.title, old.content, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS entries_au AFTER UPDATE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, content, tags) VALUES('delete', old.rowid, old.title, old.content, old.tags);
    INSERT INTO entries_fts(rowid, title, content, tags) VALUES (new.rowid, new.title, new.content, new.tags);
END;
CREATE INDEX IF NOT EXISTS idx_entries_category ON entries(category);
CREATE INDEX IF NOT EXISTS idx_entries_scope ON entries(scope);
CREATE INDEX IF NOT EXISTS idx_entries_confidence ON entries(confidence);
"#;

#[derive(Debug, Serialize)]
struct SqliteKnowledgeEntry {
    id: String,
    category: String,
    title: String,
    content: String,
    tags: Vec<String>,
    scope: Option<String>,
    confidence: f64,
    source: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Serialize)]
struct SqliteKnowledgeSearchResult {
    entry: SqliteKnowledgeEntry,
    relevance: f64,
    match_source: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SqliteKnowledgeStats {
    total: usize,
    by_category: indexmap::IndexMap<String, usize>,
    by_source: indexmap::IndexMap<String, usize>,
    avg_confidence: f64,
}

fn with_db<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&Connection) -> Result<R>,
{
    let guard = DATABASE.lock().map_err(|error| format!("DB lock: {error}"))?;
    let connection = guard.as_ref().ok_or_else(|| {
        "Knowledge DB not opened. Call knowledgeOpen first.".to_string()
    })?;
    f(connection)
}

fn generate_id() -> String {
    ulid::Ulid::new().to_string()
}

fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

fn split_tags(tags: &str) -> Vec<String> {
    if tags.is_empty() {
        return Vec::new();
    }
    tags.split(',').map(|tag| js_trim(tag).to_string()).collect()
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<SqliteKnowledgeEntry> {
    let tags: String = row.get(4)?;
    Ok(SqliteKnowledgeEntry {
        id: row.get(0)?,
        category: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        tags: split_tags(&tags),
        scope: row.get(5)?,
        confidence: row.get(6)?,
        source: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

fn escape_fts_query(query: &str) -> String {
    query
        .split(is_js_whitespace)
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn normalize_scope_path(raw: &str) -> String {
    let unified = raw.replace('\\', "/");
    let without_trailing = if unified.len() > 1 {
        unified.trim_end_matches('/')
    } else {
        unified.as_str()
    };
    if without_trailing.is_empty() {
        "/".to_string()
    } else {
        without_trailing.to_string()
    }
}

fn add_hit(
    hits: &mut indexmap::IndexMap<String, (SqliteKnowledgeEntry, f64, Vec<String>)>,
    entry: SqliteKnowledgeEntry,
    score: f64,
    source: &str,
) {
    let id = entry.id.clone();
    let hit = hits.entry(id).or_insert_with(|| (entry, 0.0, Vec::new()));
    hit.1 += score;
    hit.2.push(source.to_string());
}

pub fn is_available() -> bool {
    true
}

pub fn knowledge_open(db_path: &str) -> Result<()> {
    let mut guard = DATABASE
        .lock()
        .map_err(|error| format!("DB lock: {error}"))?;
    if guard.is_some() {
        *guard = None;
    }
    std::fs::create_dir_all(
        std::path::Path::new(db_path)
            .parent()
            .unwrap_or(std::path::Path::new(".")),
    )
    .map_err(|error| format!("{error}"))?;
    let connection = Connection::open(db_path).map_err(|error| format!("Open DB: {error}"))?;
    connection
        .execute_batch("PRAGMA journal_mode = WAL;")
        .map_err(|error| format!("{error}"))?;
    let has_table = connection
        .prepare("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='entries'")
        .and_then(|mut statement| statement.query_row([], |row| row.get::<_, i64>(0)))
        .map(|count| count > 0)
        .map_err(|error| format!("{error}"))?;
    if !has_table {
        connection
            .execute_batch(SCHEMA_SQL)
            .map_err(|error| format!("Schema: {error}"))?;
    }
    *guard = Some(connection);
    if let Ok(mut path_guard) = DATABASE_PATH.lock() {
        *path_guard = Some(PathBuf::from(db_path));
    }
    Ok(())
}

pub fn knowledge_close() {
    if let Ok(mut guard) = DATABASE.lock() {
        if let Some(connection) = guard.take() {
            let _ = connection.close();
        }
    }
    if let Ok(mut path_guard) = DATABASE_PATH.lock() {
        *path_guard = None;
    }
}

pub fn knowledge_add(
    title: &str,
    category: &str,
    content: &str,
    tags: &str,
    scope: Option<&str>,
    source: &str,
    confidence: f64,
) -> Result<String> {
    with_db(|connection| {
        let id = generate_id();
        let now = now_iso();
        let scope = scope.filter(|value| !value.is_empty());
        connection
            .execute(
                "INSERT INTO entries (id, category, title, content, tags, scope, confidence, source, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![id, category, title, content, tags, scope, confidence, source, now, now],
            )
            .map_err(|error| format!("Insert: {error}"))?;
        let entry = SqliteKnowledgeEntry {
            id,
            category: category.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: split_tags(tags),
            scope,
            confidence,
            source: source.to_string(),
            created_at: now.clone(),
            updated_at: now,
        };
        serde_json::to_string(&entry).map_err(|error| format!("JSON: {error}"))
    })
}

pub fn knowledge_search(
    query: &str,
    scope_path: Option<&str>,
    tags: Option<&str>,
    limit: usize,
    min_confidence: f64,
) -> Result<String> {
    with_db(|connection| {
        let mut hits: indexmap::IndexMap<String, (SqliteKnowledgeEntry, f64, Vec<String>)> =
            indexmap::IndexMap::new();

        if let Some(scope_path) = scope_path {
            let candidate = normalize_scope_path(scope_path);
            let mut statement = connection
                .prepare(
                    "SELECT id,category,title,content,tags,scope,confidence,source,created_at,updated_at FROM entries WHERE confidence >= ?1 AND (scope IS NULL OR ?2 = rtrim(replace(scope, char(92), '/'), '/') OR instr(?2, rtrim(replace(scope, char(92), '/'), '/') || '/') = 1) ORDER BY confidence DESC LIMIT 20",
                )
                .map_err(|error| format!("{error}"))?;
            let rows = statement
                .query_map(params![min_confidence, candidate], row_to_entry)
                .map_err(|error| format!("{error}"))?;
            for row in rows {
                let entry = row.map_err(|error| format!("{error}"))?;
                let score = if entry.scope.is_some() { 3.0 } else { 1.0 };
                add_hit(&mut hits, entry, score, "scope");
            }
        }

        if !query.is_empty() && query != "*" {
            let fts_query = escape_fts_query(query);
            let sql = "SELECT e.id,e.category,e.title,e.content,e.tags,e.scope,e.confidence,e.source,e.created_at,e.updated_at,rank FROM entries_fts f JOIN entries e ON e.rowid=f.rowid WHERE entries_fts MATCH ?1 AND e.confidence >= ?2 ORDER BY rank LIMIT 20";
            let mut statement = connection
                .prepare(sql)
                .map_err(|error| format!("{error}"))?;
            let rows = statement
                .query_map(params![fts_query, min_confidence], |row| {
                    let entry = row_to_entry(row)?;
                    let rank: f64 = row.get(10)?;
                    Ok((entry, rank))
                })
                .map_err(|error| format!("{error}"))?;
            for row in rows {
                let (entry, rank) = row.map_err(|error| format!("{error}"))?;
                let score = 2.0 * (1.0 / (1.0 + rank.abs()));
                add_hit(&mut hits, entry, score, "fts");
            }
        }

        if let Some(tags) = tags {
            let wanted: HashSet<&str> = tags
                .split(',')
                .map(js_trim)
                .filter(|tag| !tag.is_empty())
                .collect();
            if !wanted.is_empty() {
                let mut statement = connection
                    .prepare(
                        "SELECT id,category,title,content,tags,scope,confidence,source,created_at,updated_at FROM entries WHERE confidence >= ?1 AND tags != ''",
                    )
                    .map_err(|error| format!("{error}"))?;
                let rows = statement
                    .query_map(params![min_confidence], row_to_entry)
                    .map_err(|error| format!("{error}"))?;
                for row in rows {
                    let entry = row.map_err(|error| format!("{error}"))?;
                    let overlap = entry
                        .tags
                        .iter()
                        .filter(|tag| wanted.contains(tag.as_str()))
                        .count();
                    if overlap > 0 {
                        add_hit(&mut hits, entry, overlap as f64, "tag");
                    }
                }
            }
        }

        let mut results: Vec<SqliteKnowledgeSearchResult> = hits
            .into_values()
            .map(|(entry, score, sources)| {
                let relevance = score * entry.confidence;
                let unique: Vec<String> = sources
                    .into_iter()
                    .collect::<IndexSet<_>>()
                    .into_iter()
                    .collect();
                SqliteKnowledgeSearchResult {
                    entry,
                    relevance,
                    match_source: unique,
                }
            })
            .collect();
        results.sort_by(|left, right| {
            right
                .relevance
                .partial_cmp(&left.relevance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);

        serde_json::to_string(&results).map_err(|error| format!("JSON: {error}"))
    })
}

pub fn knowledge_remove(id: &str) -> Result<bool> {
    with_db(|connection| {
        let affected = connection
            .execute("DELETE FROM entries WHERE id = ?1", params![id])
            .map_err(|error| format!("{error}"))?;
        Ok(affected > 0)
    })
}

pub fn knowledge_confirm(id: &str) -> Result<bool> {
    with_db(|connection| {
        let now = now_iso();
        let affected = connection
            .execute(
                "UPDATE entries SET confidence = 1.0, source = 'ai-confirmed', updated_at = ?1 WHERE id = ?2",
                params![now, id],
            )
            .map_err(|error| format!("{error}"))?;
        Ok(affected > 0)
    })
}

pub fn knowledge_stats() -> Result<String> {
    with_db(|connection| {
        let total: usize = connection
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))
            .map_err(|error| format!("{error}"))?;
        let mut by_category = indexmap::IndexMap::new();
        let mut statement = connection
            .prepare("SELECT category, count(*) FROM entries GROUP BY category")
            .map_err(|error| format!("{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
            })
            .map_err(|error| format!("{error}"))?;
        for row in rows {
            let (name, count) = row.map_err(|error| format!("{error}"))?;
            by_category.insert(name, count);
        }

        let mut by_source = indexmap::IndexMap::new();
        let mut statement = connection
            .prepare("SELECT source, count(*) FROM entries GROUP BY source")
            .map_err(|error| format!("{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
            })
            .map_err(|error| format!("{error}"))?;
        for row in rows {
            let (name, count) = row.map_err(|error| format!("{error}"))?;
            by_source.insert(name, count);
        }

        let avg_confidence: f64 = connection
            .query_row("SELECT COALESCE(avg(confidence),0) FROM entries", [], |row| {
                row.get(0)
            })
            .map_err(|error| format!("{error}"))?;

        let stats = SqliteKnowledgeStats {
            total,
            by_category,
            by_source,
            avg_confidence,
        };
        serde_json::to_string(&stats).map_err(|error| format!("JSON: {error}"))
    })
}

pub fn knowledge_import(markdown: &str) -> Result<String> {
    with_db(|connection| {
        let normalized = markdown.replace("\r\n", "\n");
        let mut entries = Vec::new();

        for block in normalized.split("\n---\n") {
            let block = js_trim(block);
            if block.is_empty() {
                continue;
            }
            let lines: Vec<&str> = block.lines().collect();
            if lines.is_empty() {
                continue;
            }

            let header = js_trim(lines[0].trim_start_matches('#'));
            let (category, title) = match header.split_once(':') {
                Some((category, title)) => (js_trim(category), js_trim(title)),
                None => continue,
            };

            let mut tags = String::new();
            let mut scope: Option<String> = None;
            let mut content_start = 1;

            for (index, line) in lines.iter().enumerate().skip(1) {
                if let Some(value) = line.strip_prefix("tags:") {
                    tags = value
                        .split(',')
                        .map(js_trim)
                        .filter(|tag| !tag.is_empty())
                        .collect::<Vec<_>>()
                        .join(",");
                    content_start = index + 1;
                } else if let Some(value) = line.strip_prefix("scope:") {
                    let value = js_trim(value);
                    if !value.is_empty() {
                        scope = Some(value.to_string());
                    }
                    content_start = index + 1;
                } else if line.is_empty() {
                    content_start = index + 1;
                    break;
                } else {
                    break;
                }
            }

            let content = js_trim(&lines[content_start..].join("\n"))
                .replace("\n\\---\n", "\n---\n");
            if content.is_empty() {
                continue;
            }

            let id = generate_id();
            let now = now_iso();
            if connection
                .execute(
                    "INSERT INTO entries (id, category, title, content, tags, scope, confidence, source, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,1.0,'human',?7,?8)",
                    params![id, category, title, content, tags, scope, now, now],
                )
                .is_err()
            {
                continue;
            }
            entries.push(SqliteKnowledgeEntry {
                id,
                category: category.to_string(),
                title: title.to_string(),
                content,
                tags: split_tags(&tags),
                scope,
                confidence: 1.0,
                source: "human".to_string(),
                created_at: now.clone(),
                updated_at: now,
            });
        }

        serde_json::to_string(&entries).map_err(|error| format!("JSON: {error}"))
    })
}

fn home_dir() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("KIMI_CODE_HOME")
        && !home.trim().is_empty()
    {
        return Some(PathBuf::from(home));
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(".kimi-code"))
}

fn resolve_db_path(cwd: &Path, existing_only: bool) -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var("KIMI_KNOWLEDGE_DB") {
        if !override_path.is_empty() {
            let path = PathBuf::from(override_path);
            return (!existing_only || path.exists()).then_some(path);
        }
    }
    let project_db = cwd.join(".kimi-code").join("knowledge.db");
    if project_db.exists() {
        return Some(project_db);
    }
    let user_db = home_dir().map(|home| home.join("knowledge.db"));
    match (existing_only, user_db) {
        (true, Some(path)) => path.exists().then_some(path),
        (_, path) => path,
    }
}

fn is_open_at(path: &Path) -> bool {
    let matches = DATABASE_PATH
        .lock()
        .map(|guard| guard.as_deref() == Some(path))
        .unwrap_or(false);
    matches
        && DATABASE
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
}

pub fn ensure_open(cwd: &Path) -> bool {
    if DATABASE.lock().map(|guard| guard.is_some()).unwrap_or(false) {
        return true;
    }
    let Some(path) = resolve_db_path(cwd, false) else {
        return false;
    };
    if is_open_at(&path) {
        return true;
    }
    open_database(&path)
}

pub fn open_existing(cwd: &Path) -> bool {
    if DATABASE.lock().map(|guard| guard.is_some()).unwrap_or(false) {
        return true;
    }
    let Some(path) = resolve_db_path(cwd, true) else {
        return false;
    };
    if is_open_at(&path) {
        return true;
    }
    open_database(&path)
}

fn open_database(path: &Path) -> bool {
    match knowledge_open(&path.to_string_lossy()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "failed to open knowledge database");
            false
        }
    }
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) fn isolate_test_db() {
    static ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("kimi-agent-knowledge-tests-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the test knowledge directory must be creatable");
        let path = dir.join("knowledge.db");
        unsafe { std::env::set_var("KIMI_KNOWLEDGE_DB", path) };
    });
}

#[cfg(test)]
pub(crate) fn close_db_for_tests() {
    knowledge_close();
}
