use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::sync::Mutex;

use crate::_base::errors::unexpected_error::on_unexpected_error;
use crate::persistence::interface::atomicDocumentStore::IAtomicDocumentStore;
use crate::wire::record::WireAsyncError;

use super::fileHistory::FILE_HISTORY_BLOB_PREFIX;

pub const FILE_HISTORY_SESSION_WINDOW: usize = 30;
pub const RETENTION_DOC_SCOPE: &str = "file-history";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionEntry {
    pub id: String,
    pub touched_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct RetentionDoc {
    pub sessions: Vec<RetentionEntry>,
}

#[derive(Clone)]
pub struct FileHistoryRetentionInput {
    pub docs: Arc<dyn IAtomicDocumentStore>,
    pub workspace_id: String,
    pub session_dir: PathBuf,
    pub session_id: String,
}

#[derive(Clone)]
pub struct DropFileHistorySessionInput {
    pub docs: Arc<dyn IAtomicDocumentStore>,
    pub workspace_id: String,
    pub session_id: String,
}

async fn read_retention_doc(
    docs: &dyn IAtomicDocumentStore,
    workspace_id: &str,
) -> Result<Option<RetentionDoc>, WireAsyncError> {
    let Some(value) = docs.get(RETENTION_DOC_SCOPE, workspace_id).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_value(value)?))
}

async fn write_retention_doc(
    docs: &dyn IAtomicDocumentStore,
    workspace_id: &str,
    doc: &RetentionDoc,
) -> Result<(), WireAsyncError> {
    docs.set(
        RETENTION_DOC_SCOPE,
        workspace_id,
        serde_json::to_value(doc)?,
    )
    .await
}

static TOUCH_QUEUES: LazyLock<StdMutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn touch_queue(workspace_id: &str) -> Arc<Mutex<()>> {
    TOUCH_QUEUES
        .lock()
        .unwrap()
        .entry(workspace_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub async fn touch_file_history_session(input: FileHistoryRetentionInput) {
    let queue = touch_queue(&input.workspace_id);
    let _guard = queue.lock().await;
    apply_touch(input).await;
}

async fn apply_touch(input: FileHistoryRetentionInput) {
    let mut doc = match read_retention_doc(input.docs.as_ref(), &input.workspace_id).await {
        Ok(doc) => doc.unwrap_or_default(),
        Err(error) => {
            on_unexpected_error(error);
            return;
        }
    };
    doc.sessions.retain(|entry| entry.id != input.session_id);
    doc.sessions.push(RetentionEntry {
        id: input.session_id.clone(),
        touched_at: now_ms(),
    });
    doc.sessions.sort_by_key(|entry| entry.touched_at);
    let evicted_count = doc.sessions.len().saturating_sub(FILE_HISTORY_SESSION_WINDOW);
    let evicted: Vec<RetentionEntry> = doc.sessions.drain(0..evicted_count).collect();
    let Some(session_root) = input.session_dir.parent() else {
        return;
    };
    let mut stuck = Vec::new();
    for victim in evicted {
        let agents_dir = session_root.join(&victim.id).join("agents");
        if !remove_session_blobs(&agents_dir).await {
            stuck.push(victim);
        }
    }
    if !stuck.is_empty() {
        doc.sessions.splice(0..0, stuck);
    }
    if let Err(error) = write_retention_doc(input.docs.as_ref(), &input.workspace_id, &doc).await {
        on_unexpected_error(error);
    }
}

async fn remove_session_blobs(agents_dir: &Path) -> bool {
    let mut entries = match fs::read_dir(agents_dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(error) => {
            on_unexpected_error(error);
            return false;
        }
    };
    let mut agent_names = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => match entry.file_type().await {
                Ok(kind) if kind.is_dir() => agent_names.push(entry.file_name()),
                Ok(_) => {}
                Err(error) => {
                    on_unexpected_error(error);
                    return false;
                }
            },
            Ok(None) => break,
            Err(error) => {
                on_unexpected_error(error);
                return false;
            }
        }
    }
    let mut removed = true;
    for name in agent_names {
        let path = agents_dir.join(name).join(FILE_HISTORY_BLOB_PREFIX);
        match fs::symlink_metadata(&path).await {
            Ok(info) => {
                let result = if info.is_dir() {
                    fs::remove_dir_all(&path).await
                } else {
                    fs::remove_file(&path).await
                };
                if let Err(error) = result {
                    on_unexpected_error(error);
                    removed = false;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                on_unexpected_error(error);
                removed = false;
            }
        }
    }
    removed
}

pub async fn drop_file_history_session(input: DropFileHistorySessionInput) {
    let queue = touch_queue(&input.workspace_id);
    let _guard = queue.lock().await;
    let Some(mut doc) = (match read_retention_doc(input.docs.as_ref(), &input.workspace_id).await {
        Ok(doc) => doc,
        Err(error) => {
            on_unexpected_error(error);
            return;
        }
    }) else {
        return;
    };
    let previous_len = doc.sessions.len();
    doc.sessions.retain(|entry| entry.id != input.session_id);
    if doc.sessions.len() == previous_len {
        return;
    }
    if let Err(error) = write_retention_doc(input.docs.as_ref(), &input.workspace_id, &doc).await {
        on_unexpected_error(error);
    }
}

pub async fn touch_forked_file_history(input: FileHistoryRetentionInput) {
    let agents_dir = input.session_dir.join("agents");
    let mut entries = match fs::read_dir(agents_dir).await {
        Ok(entries) => entries,
        Err(_) => return,
    };
    let mut agent_dirs = Vec::new();
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => match entry.file_type().await {
                Ok(kind) if kind.is_dir() => agent_dirs.push(entry.path()),
                Ok(_) => {}
                Err(_) => return,
            },
            Ok(None) => break,
            Err(_) => return,
        }
    }
    for agent_dir in agent_dirs {
        let blobs_dir = agent_dir.join(FILE_HISTORY_BLOB_PREFIX);
        let Ok(mut blobs) = fs::read_dir(blobs_dir).await else {
            continue;
        };
        if matches!(blobs.next_entry().await, Ok(Some(_))) {
            touch_file_history_session(input).await;
            return;
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
