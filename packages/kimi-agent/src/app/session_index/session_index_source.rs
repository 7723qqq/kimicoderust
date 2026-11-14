use std::collections::HashMap;
use std::future::Future;

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::environment::environment::LOCAL_ENVIRONMENT_ID;
use crate::persistence::interface::atomic_document_store::IAtomicDocumentStore;
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::session::session_metadata::session_metadata::SessionLastTurnReason;
use crate::wire::record::WireAsyncError;

use super::session_index::{CHILD_SESSION_KIND, CHILD_SESSION_KIND_KEY, SessionSummary};
use super::session_index_dirty_journal::SESSION_INDEX_DIRTY_DIR;

const META_SCOPE: &str = "session-meta";
const META_KEY: &str = "state.json";
const SCAN_CACHE_KEY: &str = "scan.json";
const SCAN_CACHE_VERSION: u64 = 2;

pub const SESSION_INDEX_SCAN_CACHE_DIR: &str = ".index-cache";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionScanCacheEntry {
    pub ws: String,
    pub meta: String,
    pub mtime_ms: i64,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_dir: Option<String>,
    pub summary: SessionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionScanCacheFile {
    version: u64,
    sessions: HashMap<String, SessionScanCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionStateFileStat {
    pub meta: String,
    pub mtime_ms: i64,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSummaryInput {
    pub id: String,
    pub workspace_id: String,
    pub cwd: Option<String>,
    pub environment_id: Option<String>,
    pub environment_cwd: Option<String>,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived: bool,
    pub archived_at: Option<i64>,
    pub custom: Option<HashMap<String, Value>>,
    pub last_turn_reason: Option<SessionLastTurnReason>,
}

fn scan_cache_scope(sessions_scope: &str) -> String {
    format!("{sessions_scope}/{SESSION_INDEX_SCAN_CACHE_DIR}")
}

fn finite_number(value: Option<&Value>) -> bool {
    value.and_then(Value::as_f64).is_some_and(f64::is_finite)
}

fn is_scan_cache_entry_shape(value: &Value) -> bool {
    let Some(entry) = value.as_object() else {
        return false;
    };
    let summary = entry.get("summary");
    let summary_shape = summary
        .and_then(Value::as_object)
        .is_some_and(|summary| {
            summary.get("id").is_some_and(Value::is_string)
                && summary.get("workspaceId").is_some_and(Value::is_string)
                && finite_number(summary.get("createdAt"))
                && finite_number(summary.get("updatedAt"))
                && summary.get("archived").is_some_and(Value::is_boolean)
        });
    entry.get("ws").is_some_and(Value::is_string)
        && entry.get("meta").is_some_and(Value::is_string)
        && finite_number(entry.get("mtimeMs"))
        && finite_number(entry.get("size"))
        && entry.get("primaryDir").is_none_or(|value| value.is_string())
        && summary_shape
}

pub async fn read_session_scan_cache(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
) -> HashMap<String, SessionScanCacheEntry> {
    let mut out = HashMap::new();
    let result = async {
        let bytes = storage
            .read(&scan_cache_scope(sessions_scope), SCAN_CACHE_KEY)
            .await?;
        let Some(bytes) = bytes else {
            return Ok::<(), WireAsyncError>(());
        };
        let parsed: Value = serde_json::from_slice(&bytes)?;
        if parsed.get("version").and_then(Value::as_u64) != Some(SCAN_CACHE_VERSION) {
            return Ok(());
        }
        let Some(sessions) = parsed.get("sessions").and_then(Value::as_object) else {
            return Ok(());
        };
        for (id, entry) in sessions {
            if is_scan_cache_entry_shape(entry)
                && let Ok(entry) = serde_json::from_value::<SessionScanCacheEntry>(entry.clone())
            {
                out.insert(id.clone(), entry);
            }
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        out.clear();
    }
    out
}

pub async fn write_session_scan_cache(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
    entries: &HashMap<String, SessionScanCacheEntry>,
) -> Result<(), WireAsyncError> {
    let file = SessionScanCacheFile {
        version: SCAN_CACHE_VERSION,
        sessions: entries.clone(),
    };
    storage
        .write(
            &scan_cache_scope(sessions_scope),
            SCAN_CACHE_KEY,
            serde_json::to_vec(&file)?,
            None,
        )
        .await
}

pub async fn stat_session_state_file(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
    workspace_id: &str,
    session_id: &str,
) -> Option<SessionStateFileStat> {
    let base = format!("{sessions_scope}/{workspace_id}/{session_id}");
    for (meta, scope) in [
        (META_KEY.to_string(), base.clone()),
        (
            format!("{META_SCOPE}/{META_KEY}"),
            format!("{base}/{META_SCOPE}"),
        ),
    ] {
        let (mtime_ms, size) = tokio::join!(
            storage.mtime(&scope, META_KEY),
            storage.size(&scope, META_KEY)
        );
        let (Ok(Some(mtime_ms)), Ok(Some(size))) = (mtime_ms, size) else {
            continue;
        };
        return Some(SessionStateFileStat {
            meta,
            mtime_ms,
            size,
        });
    }
    None
}

pub fn parse_time(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| value.is_finite())
            .unwrap_or(0.0) as i64,
        Some(Value::String(value)) => chrono::DateTime::parse_from_rfc3339(value)
            .map(|date| date.timestamp_millis())
            .or_else(|_| {
                chrono::DateTime::parse_from_rfc2822(value)
                    .map(|date| date.timestamp_millis())
            })
            .or_else(|_| {
                chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map(|date| {
                    date.and_hms_milli_opt(0, 0, 0, 0)
                        .expect("midnight is valid")
                        .and_utc()
                        .timestamp_millis()
                })
            })
            .unwrap_or(0),
        _ => 0,
    }
}

pub fn parse_turn_outcome(value: Option<&Value>) -> Option<SessionLastTurnReason> {
    match value.and_then(Value::as_str) {
        Some("completed") => Some(SessionLastTurnReason::Completed),
        Some("cancelled") => Some(SessionLastTurnReason::Cancelled),
        Some("failed") => Some(SessionLastTurnReason::Failed),
        _ => None,
    }
}

pub fn recover_cwd(meta: &Map<String, Value>) -> Option<String> {
    for key in ["cwd", "workDir"] {
        if let Some(value) = meta.get(key).and_then(Value::as_str)
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }
    meta.get("custom")
        .and_then(Value::as_object)
        .and_then(|custom| custom.get("cwd"))
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

pub fn build_session_summary(fields: SessionSummaryInput) -> SessionSummary {
    SessionSummary {
        id: fields.id,
        workspace_id: fields.workspace_id,
        cwd: fields.cwd,
        environment_id: fields.environment_id,
        environment_cwd: fields.environment_cwd,
        title: fields.title,
        last_prompt: fields.last_prompt,
        created_at: fields.created_at,
        updated_at: fields.updated_at,
        archived: fields.archived,
        archived_at: fields.archived_at,
        custom: fields.custom,
        last_turn_reason: fields.last_turn_reason,
        extra: Map::new(),
    }
}

pub fn summary_matches_child_of(summary: &SessionSummary, parent_id: Option<&str>) -> bool {
    let Some(parent_id) = parent_id else {
        return true;
    };
    let Some(custom) = &summary.custom else {
        return false;
    };
    custom.get("parent_session_id").and_then(Value::as_str) == Some(parent_id)
        && custom.get(CHILD_SESSION_KIND_KEY).and_then(Value::as_str)
            == Some(CHILD_SESSION_KIND)
}

pub fn summary_equals(a: &SessionSummary, b: &SessionSummary) -> bool {
    a.id == b.id
        && a.workspace_id == b.workspace_id
        && a.cwd == b.cwd
        && a.environment_id == b.environment_id
        && a.environment_cwd == b.environment_cwd
        && a.title == b.title
        && a.last_prompt == b.last_prompt
        && a.created_at == b.created_at
        && a.updated_at == b.updated_at
        && a.archived == b.archived
        && a.archived_at == b.archived_at
        && a.last_turn_reason == b.last_turn_reason
        && a.custom == b.custom
}

pub async fn list_workspace_ids(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
) -> Vec<String> {
    storage
        .list(sessions_scope, None, None)
        .await
        .map(|entries| {
            entries
                .into_iter()
                .filter(|entry| {
                    entry != SESSION_INDEX_DIRTY_DIR && entry != SESSION_INDEX_SCAN_CACHE_DIR
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn list_session_ids(
    storage: &dyn IFileSystemStorageService,
    sessions_scope: &str,
    workspace_id: &str,
) -> Vec<String> {
    storage
        .list(&format!("{sessions_scope}/{workspace_id}"), None, None)
        .await
        .unwrap_or_default()
}

async fn read_meta(
    docs: &dyn IAtomicDocumentStore,
    scope: &str,
) -> Option<Map<String, Value>> {
    match docs.get(scope, META_KEY).await {
        Ok(Some(Value::Object(meta))) => Some(meta),
        _ => None,
    }
}

pub async fn read_session_summary(
    docs: &dyn IAtomicDocumentStore,
    sessions_scope: &str,
    workspace_id: &str,
    session_id: &str,
    primary_dir: Option<&str>,
) -> Option<SessionSummary> {
    let base = format!("{sessions_scope}/{workspace_id}/{session_id}");
    let meta = match read_meta(docs, &base).await {
        Some(meta) => Some(meta),
        None => read_meta(docs, &format!("{base}/{META_SCOPE}")).await,
    }?;
    let custom = meta.get("custom").and_then(Value::as_object).map(|custom| {
        custom
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<HashMap<_, _>>()
    });
    let original_cwd = recover_cwd(&meta);
    let environment_id = meta
        .get("environmentId")
        .and_then(Value::as_str)
        .unwrap_or(LOCAL_ENVIRONMENT_ID);
    let environment_cwd = meta
        .get("environmentCwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cwd = if primary_dir.is_none() || environment_id != LOCAL_ENVIRONMENT_ID {
        original_cwd
    } else {
        environment_cwd
            .clone()
            .or_else(|| primary_dir.map(str::to_string))
    };
    Some(build_session_summary(SessionSummaryInput {
        id: session_id.to_string(),
        workspace_id: workspace_id.to_string(),
        cwd,
        environment_id: meta
            .get("environmentId")
            .and_then(Value::as_str)
            .map(str::to_string),
        environment_cwd,
        title: meta.get("title").and_then(Value::as_str).map(str::to_string),
        last_prompt: meta
            .get("lastPrompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        created_at: parse_time(meta.get("createdAt")),
        updated_at: parse_time(meta.get("updatedAt")),
        archived: meta.get("archived").and_then(Value::as_bool) == Some(true),
        archived_at: meta
            .contains_key("archivedAt")
            .then(|| parse_time(meta.get("archivedAt"))),
        custom,
        last_turn_reason: parse_turn_outcome(meta.get("lastTurnReason")),
    }))
}

pub async fn map_bounded<T, R, F, Fut>(
    items: Vec<T>,
    concurrency: usize,
    function: F,
) -> Vec<R>
where
    T: Send,
    R: Send,
    F: FnMut(T) -> Fut + Send,
    Fut: Future<Output = Option<R>> + Send,
{
    let worker_count = concurrency.min(items.len());
    if worker_count == 0 {
        return Vec::new();
    }
    stream::iter(items)
        .map(function)
        .buffer_unordered(worker_count)
        .filter_map(|value| async move { value })
        .collect()
        .await
}
