use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use futures_util::future::{BoxFuture, join_all};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::_base::utils::paths::is_windows_absolute_path;
use crate::_base::utils::workdir_slug::{encode_work_dir_key, workspace_root_key};
use crate::persistence::interface::append_log_store::{
    AppendLogRewriteOptions, IAppendLogStore,
};
use crate::persistence::interface::storage::IFileSystemStorageService;
use crate::wire::record::WireAsyncError;

use super::workspace::Workspace;

pub const SESSION_INDEX_SCOPE: &str = "";
pub const SESSION_INDEX_KEY: &str = "session_index.jsonl";
pub const SESSION_INDEX_COMPACTION_MIN_STALE: usize = 16;
pub const SESSION_INDEX_COMPACTION_MIN_STALE_RATIO: f64 = 0.2;

const SESSION_INDEX_COMPACTION_MAX_ATTEMPTS: usize = 3;
const SESSION_INDEX_STAT_CONCURRENCY: usize = 64;

pub type SessionDirExists = Arc<
    dyn Fn(String) -> BoxFuture<'static, Result<bool, WireAsyncError>> + Send + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIndexLine {
    pub session_id: String,
    pub session_dir: String,
    pub work_dir: String,
}

pub struct SessionIndexCompactionDeps {
    pub storage: Arc<dyn IFileSystemStorageService>,
    pub append_logs: Arc<dyn IAppendLogStore>,
    pub session_dir_exists: SessionDirExists,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionIndexCompactionOutcome {
    pub lines: usize,
    pub kept: usize,
    pub dropped_stale: usize,
    pub dropped_garbage: usize,
}

enum ClassifiedSessionIndexLine {
    Entry { record: Value, session_dir: String },
    Other { record: Value },
    Garbage,
}

fn classify_session_index_line(line: &str) -> ClassifiedSessionIndexLine {
    let Ok(parsed) = serde_json::from_str::<Value>(line) else {
        return ClassifiedSessionIndexLine::Garbage;
    };
    let Some(object) = parsed.as_object() else {
        return ClassifiedSessionIndexLine::Other { record: parsed };
    };
    let session_id = object.get("sessionId").and_then(Value::as_str);
    let session_dir = object.get("sessionDir").and_then(Value::as_str);
    let work_dir = object.get("workDir").and_then(Value::as_str);
    if session_id.is_some() && session_dir.is_some() && work_dir.is_some() {
        return ClassifiedSessionIndexLine::Entry {
            record: parsed,
            session_dir: session_dir.unwrap().to_string(),
        };
    }
    ClassifiedSessionIndexLine::Other { record: parsed }
}

fn classify_session_index_lines(bytes: &[u8]) -> Vec<ClassifiedSessionIndexLine> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines.push(classify_session_index_line(trimmed));
    }
    lines
}

async fn find_stale_session_dirs(
    dirs: Vec<String>,
    session_dir_exists: SessionDirExists,
) -> Result<HashSet<String>, WireAsyncError> {
    let mut seen = HashSet::new();
    let unique = dirs
        .into_iter()
        .filter(|dir| seen.insert(dir.clone()))
        .collect::<Vec<_>>();
    let mut stale = HashSet::new();
    for batch in unique.chunks(SESSION_INDEX_STAT_CONCURRENCY) {
        let checks = batch.iter().map(|dir| {
            let dir = dir.clone();
            let session_dir_exists = session_dir_exists.clone();
            async move {
                let exists = session_dir_exists(dir.clone()).await?;
                Ok::<Option<String>, WireAsyncError>((!exists).then_some(dir))
            }
        });
        for result in join_all(checks).await {
            if let Some(dir) = result? {
                stale.insert(dir);
            }
        }
    }
    Ok(stale)
}

pub async fn compact_session_index_if_stale(
    deps: SessionIndexCompactionDeps,
) -> Result<Option<SessionIndexCompactionOutcome>, WireAsyncError> {
    let Some(initial) = deps.storage.read(SESSION_INDEX_SCOPE, SESSION_INDEX_KEY).await? else {
        return Ok(None);
    };
    let survey = classify_session_index_lines(&initial);
    let mut entry_dirs = Vec::new();
    for line in &survey {
        if let ClassifiedSessionIndexLine::Entry { session_dir, .. } = line {
            entry_dirs.push(session_dir.clone());
        }
    }
    if entry_dirs.is_empty() {
        return Ok(None);
    }
    let stale_dirs = find_stale_session_dirs(entry_dirs, deps.session_dir_exists.clone()).await?;
    let stale_lines = survey
        .iter()
        .filter(|line| {
            matches!(
                line,
                ClassifiedSessionIndexLine::Entry { session_dir, .. }
                    if stale_dirs.contains(session_dir)
            )
        })
        .count();
    if stale_lines < SESSION_INDEX_COMPACTION_MIN_STALE
        || (stale_lines as f64 / survey.len() as f64) < SESSION_INDEX_COMPACTION_MIN_STALE_RATIO
    {
        return Ok(None);
    }
    for _ in 0..SESSION_INDEX_COMPACTION_MAX_ATTEMPTS {
        deps.append_logs
            .flush_log(SESSION_INDEX_SCOPE, SESSION_INDEX_KEY)
            .await?;
        let Some(bytes) = deps.storage.read(SESSION_INDEX_SCOPE, SESSION_INDEX_KEY).await? else {
            return Ok(None);
        };
        let size = deps.storage.size(SESSION_INDEX_SCOPE, SESSION_INDEX_KEY).await?;
        if size != Some(bytes.len() as u64) {
            continue;
        }
        let lines = classify_session_index_lines(&bytes);
        let mut records = Vec::new();
        let mut dropped_stale = 0;
        let mut dropped_garbage = 0;
        for line in lines.iter() {
            match line {
                ClassifiedSessionIndexLine::Garbage => {
                    dropped_garbage += 1;
                }
                ClassifiedSessionIndexLine::Entry { session_dir, .. }
                    if stale_dirs.contains(session_dir) =>
                {
                    dropped_stale += 1;
                }
                ClassifiedSessionIndexLine::Entry { record, .. }
                | ClassifiedSessionIndexLine::Other { record } => records.push(record.clone()),
            }
        }
        let rewrite_error = Arc::new(Mutex::new(None));
        let captured_error = rewrite_error.clone();
        deps.append_logs
            .rewrite(
                SESSION_INDEX_SCOPE,
                SESSION_INDEX_KEY,
                records.clone(),
                Some(AppendLogRewriteOptions {
                    on_error: Some(Arc::new(move |error| {
                        *captured_error.lock() = Some(error);
                    })),
                }),
            )
            .await?;
        if let Some(error) = rewrite_error.lock().take() {
            return Err(error);
        }
        return Ok(Some(SessionIndexCompactionOutcome {
            lines: lines.len(),
            kept: records.len(),
            dropped_stale,
            dropped_garbage,
        }));
    }
    Ok(None)
}

pub fn collect_alias_ids(
    workspaces: &[Workspace],
    session_index_entries: &[SessionIndexLine],
    root: &str,
) -> Vec<String> {
    let root_key = workspace_root_key(root);
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |alias: String| {
        if seen.insert(alias.clone()) {
            ids.push(alias);
        }
    };
    for workspace in workspaces {
        if workspace_root_key(&workspace.root) == root_key {
            add(workspace.id.clone());
        }
    }
    for line in session_index_entries {
        if workspace_root_key(&line.work_dir) == root_key {
            add(encode_work_dir_key(&line.work_dir));
        }
    }
    ids
}

pub fn dedupe_by_root(by_id: &IndexMap<String, Workspace>) -> Vec<Workspace> {
    let mut by_root = IndexMap::<String, Workspace>::new();
    for workspace in by_id.values() {
        let root_key = workspace_root_key(&workspace.root);
        let Some(existing) = by_root.get(&root_key).cloned() else {
            by_root.insert(root_key, workspace.clone());
            continue;
        };
        let canonical_id = encode_work_dir_key(&workspace.root);
        let (kept, other) = if existing.id != canonical_id && workspace.id == canonical_id {
            (workspace, &existing)
        } else {
            (&existing, workspace)
        };
        let mut seen = HashSet::new();
        let additional_dirs = kept
            .additional_dirs
            .iter()
            .chain(other.additional_dirs.iter())
            .filter(|dir| seen.insert((*dir).clone()))
            .cloned()
            .collect();
        let mut merged = kept.clone();
        merged.additional_dirs = additional_dirs;
        by_root.insert(root_key, merged);
    }
    by_root.into_values().collect()
}

pub async fn read_session_index_entries(
    storage: &Arc<dyn IFileSystemStorageService>,
) -> Result<Vec<SessionIndexLine>, WireAsyncError> {
    let Some(bytes) = storage.read(SESSION_INDEX_SCOPE, SESSION_INDEX_KEY).await? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut entries = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(entry) = parse_session_index_line(trimmed) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

pub async fn read_session_index_work_dirs(
    storage: &Arc<dyn IFileSystemStorageService>,
) -> Result<Vec<String>, WireAsyncError> {
    let mut work_dirs = Vec::new();
    for entry in read_session_index_entries(storage).await? {
        if !Path::new(&entry.work_dir).is_absolute()
            && !is_windows_absolute_path(&entry.work_dir)
        {
            continue;
        }
        work_dirs.push(entry.work_dir);
    }
    Ok(work_dirs)
}

pub fn parse_session_index_line(line: &str) -> Option<SessionIndexLine> {
    let parsed = serde_json::from_str::<Value>(line).ok()?;
    let object = parsed.as_object()?;
    Some(SessionIndexLine {
        session_id: object.get("sessionId")?.as_str()?.to_string(),
        session_dir: object.get("sessionDir")?.as_str()?.to_string(),
        work_dir: object.get("workDir")?.as_str()?.to_string(),
    })
}
