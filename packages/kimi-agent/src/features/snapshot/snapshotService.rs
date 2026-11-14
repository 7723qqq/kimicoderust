use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Mutex;

use super::fileSnapshotStore::snapshot_path_key;
use super::snapshot::{
    SnapshotAgentMarkInput, SnapshotPruneOptions, SnapshotRecordInput, SnapshotStore, SnapshotTarget,
    resolve_snapshot_target, SNAPSHOT_MAX_BYTES, SNAPSHOT_PRUNE_INTERVAL_MS,
    SNAPSHOT_PRUNE_MAX_AGE_MS, SNAPSHOT_PRUNE_MAX_BYTES_PER_PATH, SNAPSHOT_PRUNE_MAX_PER_PATH,
    SNAPSHOT_PRUNE_MIN_OBJECT_AGE_MS,
};
use crate::rpc::types::BoxFuture;
use crate::tool_input_display::ToolInputDisplay;

const MAX_PENDING: usize = 512;
static NEXT_PRUNE_AT: AtomicI64 = AtomicI64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreImage {
    Missing,
    Unreadable,
    Oversize(u64),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSnapshot {
    path: String,
    id: String,
    hash: String,
    after: Option<(String, u64)>,
}

#[derive(Default)]
struct PendingSnapshots {
    entries: HashMap<String, PendingSnapshot>,
    order: VecDeque<String>,
}

fn written_content(args: &Value) -> Option<String> {
    args.get("content").and_then(Value::as_str).map(str::to_string)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub struct SnapshotService {
    store: Arc<dyn SnapshotStore>,
    workspace_root: PathBuf,
    session_id: String,
    agent_id: String,
    pending: Mutex<PendingSnapshots>,
    releases: Mutex<HashMap<String, tokio::sync::OwnedMutexGuard<()>>>,
    path_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl SnapshotService {
    pub fn new(
        store: Arc<dyn SnapshotStore>,
        workspace_root: PathBuf,
        session_id: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            workspace_root,
            session_id: session_id.into(),
            agent_id: agent_id.into(),
            pending: Mutex::new(PendingSnapshots::default()),
            releases: Mutex::new(HashMap::new()),
            path_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &dyn SnapshotStore {
        self.store.as_ref()
    }

    pub async fn list(&self, path: &str) -> Result<Vec<super::snapshot::SnapshotRecord>, String> {
        let absolute = self.resolve(path);
        self.store.list(absolute.to_string_lossy().into_owned()).await
    }

    pub async fn capture_before(
        &self,
        tool_call_id: &str,
        display: Option<&ToolInputDisplay>,
        args: &Value,
    ) {
        let Some(mut target) = resolve_snapshot_target(display, args) else {
            return;
        };
        target.path = self.resolve(&target.path).to_string_lossy().into_owned();
        let content = if target.mode == super::snapshot::SnapshotWriteMode::Overwrite {
            written_content(args)
        } else {
            None
        };
        let key = super::fileSnapshotStore::snapshot_path_key(&target.path);
        let lock = self.path_lock(&key).await;
        let guard = lock.lock_owned().await;
        self.releases
            .lock()
            .await
            .insert(tool_call_id.to_string(), guard);
        if let Err(error) = self.capture_inner(tool_call_id, target, content).await {
            tracing::warn!(tool_call_id, %error, "snapshot capture failed");
            self.releases.lock().await.remove(tool_call_id);
        }
    }

    pub async fn on_did_execute(&self, call_id: Option<&str>, outcome: &str, failed: bool) {
        let Some(call_id) = call_id else {
            return;
        };
        let _release = self.releases.lock().await.remove(call_id);
        let entry = {
            let mut pending = self.pending.lock().await;
            pending.order.retain(|id| id != call_id);
            pending.entries.remove(call_id)
        };
        let Some(entry) = entry else {
            return;
        };
        if outcome != "executed" {
            let _ = self.store.remove(entry.path, entry.id).await;
            return;
        }
        if failed {
            let after = self.read_pre_image(&entry.path).await;
            let PreImage::Bytes(bytes) = after else {
                return;
            };
            let hash = super::fileSnapshotStore::snapshot_content_hash(&bytes);
            if !entry.hash.is_empty() && hash == entry.hash {
                let _ = self.store.remove(entry.path, entry.id).await;
            } else {
                let _ = self
                    .store
                    .complete(entry.path, entry.id, hash, bytes.len() as u64)
                    .await;
            }
            return;
        }
        if entry.hash.is_empty() {
            return;
        }
        if let Some((hash, size)) = entry.after {
            let _ = self.store.complete(entry.path, entry.id, hash, size).await;
            return;
        }
        let after = self.read_pre_image(&entry.path).await;
        let PreImage::Bytes(bytes) = after else {
            return;
        };
        let _ = self
            .store
            .complete(
                entry.path,
                entry.id,
                super::fileSnapshotStore::snapshot_content_hash(&bytes),
                bytes.len() as u64,
            )
            .await;
    }

    pub async fn mark_agent(&self, input: SnapshotAgentMarkInput) {
        if let Err(error) = self.store.mark_agent(input).await {
            tracing::warn!(%error, "snapshot agent mark failed");
        }
    }

    pub async fn prune_if_due(&self) {
        let now = now_ms();
        if now < NEXT_PRUNE_AT.load(Ordering::SeqCst) {
            return;
        }
        NEXT_PRUNE_AT.store(now + SNAPSHOT_PRUNE_INTERVAL_MS, Ordering::SeqCst);
        if let Err(error) = self
            .store
            .prune(SnapshotPruneOptions {
                now_ms: now,
                max_per_path: SNAPSHOT_PRUNE_MAX_PER_PATH,
                max_bytes_per_path: SNAPSHOT_PRUNE_MAX_BYTES_PER_PATH,
                max_age_ms: SNAPSHOT_PRUNE_MAX_AGE_MS,
                min_object_age_ms: SNAPSHOT_PRUNE_MIN_OBJECT_AGE_MS,
            })
            .await
        {
            tracing::warn!(%error, "snapshot prune failed");
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            return candidate.to_path_buf();
        }
        let slashed = path.replace('\\', "/");
        let mut root = self.workspace_root.clone();
        for segment in slashed.split('/') {
            match segment {
                "" | "." => {}
                ".." => {
                    root.pop();
                }
                other => root.push(other),
            }
        }
        root
    }

    async fn path_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self.path_locks.lock().await;
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn capture_inner(
        &self,
        call_id: &str,
        target: SnapshotTarget,
        content: Option<String>,
    ) -> Result<(), String> {
        let pre = self.read_pre_image(&target.path).await;
        if pre == PreImage::Missing {
            return Ok(());
        }
        let at = now_iso();
        let pending_path = target.path.clone();
        let path = target.path;
        let tool = target.tool;
        let mode = target.mode;
        let record = match pre {
            PreImage::Bytes(bytes) => {
                self.store
                    .record(SnapshotRecordInput {
                        path,
                        at,
                        tool,
                        mode,
                        size: bytes.len() as u64,
                        bytes: Some(bytes),
                        mtime_ms: None,
                        session_id: Some(self.session_id.clone()),
                        agent_id: Some(self.agent_id.clone()),
                        skipped: None,
                    })
                    .await?
            }
            PreImage::Oversize(size) => {
                self.store
                    .record(SnapshotRecordInput {
                        path,
                        at,
                        tool,
                        mode,
                        size,
                        bytes: None,
                        mtime_ms: None,
                        session_id: Some(self.session_id.clone()),
                        agent_id: Some(self.agent_id.clone()),
                        skipped: Some(super::snapshot::SnapshotSkipReason::MaxSize),
                    })
                    .await?
            }
            PreImage::Unreadable | PreImage::Missing => {
                self.store
                    .record(SnapshotRecordInput {
                        path,
                        at,
                        tool,
                        mode,
                        size: 0,
                        bytes: None,
                        mtime_ms: None,
                        session_id: Some(self.session_id.clone()),
                        agent_id: Some(self.agent_id.clone()),
                        skipped: Some(super::snapshot::SnapshotSkipReason::Unreadable),
                    })
                    .await?
            }
        };
        let after = content.map(|content| {
            (
                super::fileSnapshotStore::snapshot_content_hash(content.as_bytes()),
                content.len() as u64,
            )
        });
        let mut pending = self.pending.lock().await;
        let call_id = call_id.to_string();
        let is_new = !pending.entries.contains_key(&call_id);
        pending.entries.insert(
            call_id.clone(),
            PendingSnapshot {
                path: pending_path,
                id: record.id,
                hash: record.hash,
                after,
            },
        );
        if is_new {
            pending.order.push_back(call_id);
        }
        while pending.entries.len() > MAX_PENDING {
            let Some(oldest) = pending.order.pop_front() else {
                break;
            };
            pending.entries.remove(&oldest);
        }
        Ok(())
    }

    async fn read_pre_image(&self, path: &str) -> PreImage {
        let absolute = self.resolve(path);
        let info = match tokio::fs::metadata(&absolute).await {
            Ok(info) => info,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PreImage::Missing,
            Err(_) => return PreImage::Unreadable,
        };
        if !info.is_file() {
            return PreImage::Unreadable;
        }
        let size = info.len();
        if size > SNAPSHOT_MAX_BYTES {
            return PreImage::Oversize(size);
        }
        match tokio::fs::read(&absolute).await {
            Ok(bytes) => {
                if bytes.len() as u64 > SNAPSHOT_MAX_BYTES {
                    return PreImage::Oversize(bytes.len() as u64);
                }
                if bytes.len() as u64 != size {
                    return PreImage::Unreadable;
                }
                PreImage::Bytes(bytes)
            }
            Err(_) => PreImage::Unreadable,
        }
    }
}

pub fn snapshot_path_lock_key(path: &str) -> String {
    snapshot_path_key(path)
}

pub fn spawn_prune(service: Arc<SnapshotService>) -> BoxFuture<'static, ()> {
    Box::pin(async move {
        service.prune_if_due().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::snapshot::fileSnapshotStore::FileSnapshotStore;
    use crate::features::snapshot::snapshot::{SnapshotWriteMode, SNAPSHOT_RECORD_VERSION};

    fn service(dir: &Path) -> Arc<SnapshotService> {
        Arc::new(SnapshotService::new(
            Arc::new(FileSnapshotStore::new(dir.join("store"))),
            dir.to_path_buf(),
            "sess-1",
            "main",
        ))
    }

    #[tokio::test]
    async fn a_write_captures_the_pre_image_and_completes_it() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "one").unwrap();
        let service = service(&workspace);
        let display = ToolInputDisplay::FileIo {
            operation: "write".into(),
            path: "src/lib.rs".into(),
            detail: None,
            content: Some("two".into()),
            before: None,
            after: None,
        };
        service
            .capture_before(
                "call-1",
                Some(&display),
                &serde_json::json!({ "content": "two" }),
            )
            .await;
        let listed = service.list("src/lib.rs").await.unwrap();
        assert_eq!(listed.len(), 1, "the pre-image is recorded");
        assert_eq!(listed[0].size, 3);
        assert!(listed[0].after_hash.is_none());
        service
            .on_did_execute(Some("call-1"), "executed", false)
            .await;
        std::fs::write(workspace.join("src/lib.rs"), "two").unwrap();
        service
            .on_did_execute(Some("call-1"), "executed", false)
            .await;
        let listed = service.list("src/lib.rs").await.unwrap();
        assert_eq!(
            listed[0].after_hash.as_deref(),
            Some(
                crate::features::snapshot::fileSnapshotStore::snapshot_content_hash(b"two").as_str()
            )
        );
        assert_eq!(listed[0].after_size, Some(3));
    }

    #[tokio::test]
    async fn an_edit_mode_write_keeps_the_recorded_hash() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "one").unwrap();
        let service = service(&workspace);
        let display = ToolInputDisplay::FileIo {
            operation: "edit".into(),
            path: "src/lib.rs".into(),
            detail: None,
            content: None,
            before: Some("one".into()),
            after: Some("two".into()),
        };
        service
            .capture_before("call-2", Some(&display), &serde_json::json!({}))
            .await;
        let listed = service.list("src/lib.rs").await.unwrap();
        assert_eq!(listed[0].mode, SnapshotWriteMode::Edit);
        assert!(listed[0].after_hash.is_none());
    }

    #[tokio::test]
    async fn a_non_executed_outcome_removes_the_pending_record() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "one").unwrap();
        let service = service(&workspace);
        let display = ToolInputDisplay::FileIo {
            operation: "write".into(),
            path: "src/lib.rs".into(),
            detail: None,
            content: None,
            before: None,
            after: None,
        };
        service
            .capture_before("call-3", Some(&display), &serde_json::json!({}))
            .await;
        assert_eq!(service.list("src/lib.rs").await.unwrap().len(), 1);
        service.on_did_execute(Some("call-3"), "skipped", false).await;
        assert!(service.list("src/lib.rs").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_file_records_nothing_and_unknown_calls_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let service = service(&workspace);
        let display = ToolInputDisplay::FileIo {
            operation: "write".into(),
            path: "src/absent.rs".into(),
            detail: None,
            content: None,
            before: None,
            after: None,
        };
        service
            .capture_before("call-4", Some(&display), &serde_json::json!({}))
            .await;
        assert!(service.list("src/absent.rs").await.unwrap().is_empty());
        service.on_did_execute(Some("call-4"), "executed", false).await;
        service.on_did_execute(None, "executed", false).await;
        service.on_did_execute(Some("unknown"), "executed", false).await;
    }

    #[tokio::test]
    async fn prune_runs_once_per_interval() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        service.prune_if_due().await;
        let first = NEXT_PRUNE_AT.load(Ordering::SeqCst);
        assert!(first > 0);
        service.prune_if_due().await;
        assert_eq!(NEXT_PRUNE_AT.load(Ordering::SeqCst), first);
    }

    #[tokio::test]
    async fn agent_marks_are_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        service
            .mark_agent(SnapshotAgentMarkInput {
                agent_id: "agent-1".into(),
                session_id: "sess-1".into(),
                at: "2026-01-01T00:00:00.000Z".into(),
                cwd: "/w".into(),
                parent_agent_id: Some("main".into()),
                profile_name: Some("researcher".into()),
                swarm_index: Some(2),
            })
            .await;
        let marks = service.store().agents("sess-1".into()).await.unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].agent_id, "agent-1");
        assert_eq!(marks[0].swarm_index, Some(2));
        assert_eq!(marks[0].version, SNAPSHOT_RECORD_VERSION);
    }

    #[test]
    fn path_lock_keys_use_the_same_normalization_as_the_store() {
        assert_eq!(
            snapshot_path_lock_key(r"C:\repo\a.ts"),
            snapshot_path_lock_key("c:/repo/a.ts")
        );
    }
}
